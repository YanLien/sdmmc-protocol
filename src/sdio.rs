//! SDIO (Secure Digital Input Output) mode transport layer
//!
//! SDIO mode uses a dedicated host controller with 1-bit or 4-bit data bus.
//! Implement [`SdioHost`] for your platform's SDIO peripheral, and supply a
//! [`DelayNs`] implementation so the driver can apply wall-clock timeouts.

use embedded_hal::delay::DelayNs;

use crate::cmd::Command;
use crate::common::block_addr_of;
#[allow(unused_imports)]
use crate::diag::{debug, info, trace, warn_};
use crate::error::{Error, ErrorContext, Phase};
use crate::response::{
    CardState, CidResponse, CsdResponse, OcrResponse, Response, ResponseType, SwitchStatus,
};

pub use crate::cmd::DataDirection;

/// SDIO bus width
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub enum BusWidth {
    /// 1-bit bus
    Bit1,
    /// 4-bit bus
    Bit4,
    /// 8-bit bus (eMMC). Configured via the MMC `CMD6 SWITCH` flow which is
    /// outside the scope of the SD ACMD6 path used by this driver.
    Bit8,
}

/// SDIO clock speed
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub enum ClockSpeed {
    /// Default speed: up to 25 MHz
    Default,
    /// High speed: up to 50 MHz
    HighSpeed,
    /// SDR12: 12.5 MB/s
    Sdr12,
    /// SDR25: 25 MB/s
    Sdr25,
    /// SDR50: 50 MB/s
    Sdr50,
    /// SDR104: 104 MB/s
    Sdr104,
    /// DDR50: 50 MB/s (DDR)
    Ddr50,
}

/// Trait that the platform must implement for the SDIO host controller.
///
/// The driver tracks the published RCA itself, so host implementations no
/// longer need to snoop R6 responses or expose a `rca()` accessor.
pub trait SdioHost {
    /// Send a command and receive the response
    fn send_command(&mut self, cmd: &Command) -> Result<Response, Error>;

    /// Read data from the card via the data bus
    fn read_data(&mut self, buf: &mut [u8], block_size: u32) -> Result<(), Error>;

    /// Write data to the card via the data bus
    fn write_data(&mut self, buf: &[u8], block_size: u32) -> Result<(), Error>;

    /// Set the bus width
    fn set_bus_width(&mut self, width: BusWidth) -> Result<(), Error>;

    /// Set the clock speed
    fn set_clock(&mut self, speed: ClockSpeed) -> Result<(), Error>;

    /// Tell the host how many data blocks the next multi-block transfer will
    /// move (CMD18 read or CMD25 write). Single-block commands always pass 1
    /// — hosts can ignore this for that case. Default is a no-op for hosts
    /// that derive the count internally.
    fn set_block_count(&mut self, _count: u32) -> Result<(), Error> {
        Ok(())
    }

    /// Tell the host the shape of the data phase that the *next* command
    /// will trigger.
    ///
    /// This is needed because some commands (notably CMD6, which is reused
    /// for ACMD6 SET_BUS_WIDTH and CMD6 SWITCH_FUNC) can't be classified
    /// from the index alone. The driver always calls this before issuing a
    /// data-bearing command and passes `direction = None` to clear any
    /// previous hint. Default is a no-op for hosts that derive the data
    /// shape from the command index themselves.
    fn prepare_data_transfer(
        &mut self,
        _direction: DataDirection,
        _block_size: u32,
        _block_count: u32,
    ) -> Result<(), Error> {
        Ok(())
    }
}

/// SDIO mode SD/MMC driver
pub struct SdioSdmmc<H: SdioHost, D: DelayNs> {
    host: H,
    delay: D,
    rca: u16,
    high_capacity: bool,
    bus_width: BusWidth,
}

impl<H: SdioHost, D: DelayNs> SdioSdmmc<H, D> {
    /// Maximum total time to wait for ACMD41 to report card power-up.
    const INIT_TIMEOUT_MS: u32 = 1_000;
    /// Interval between ACMD41 polls.
    const INIT_POLL_MS: u32 = 10;

    pub fn new(host: H, delay: D) -> Self {
        Self {
            host,
            delay,
            rca: 0,
            high_capacity: false,
            bus_width: BusWidth::Bit1,
        }
    }

    /// Currently published Relative Card Address. `0` until [`init`](Self::init)
    /// has run successfully.
    pub fn rca(&self) -> u16 {
        self.rca
    }

    /// Initialize the card in SDIO mode
    pub fn init(&mut self) -> Result<CardInfo, Error> {
        debug!("sdio: init starting");
        // CMD0: reset
        self.host.send_command(&crate::cmd::CMD0)?;

        // CMD8: check SD v2
        let sd_v2 = self.check_cmd8()?;
        debug!("sdio: sd_v2={}", sd_v2);

        // ACMD41: initialize and read OCR
        let ocr = self.wait_ready(sd_v2)?;

        // CMD2: get CID
        let cid = match self.host.send_command(&crate::cmd::CMD2)? {
            Response::R2(raw) => Some(CidResponse::from_raw(raw)),
            _ => None,
        };

        // CMD3: get RCA — driver records it for subsequent commands
        self.rca = self.get_rca()?;
        debug!("sdio: rca={:#x}", self.rca);

        // CMD9: get CSD → derive capacity
        let cmd9 = crate::cmd::cmd9(self.rca);
        let csd_response = self.host.send_command(&cmd9)?;
        let capacity_blocks = match csd_response {
            Response::R2(raw) => CsdResponse::from_raw(raw).capacity_blocks(),
            _ => None,
        };

        // CMD7: select card
        let cmd7 = crate::cmd::cmd7(self.rca);
        self.host.send_command(&cmd7)?;

        self.high_capacity = ocr.ccs();

        // Set bus width to 4-bit
        self.set_bus_width(BusWidth::Bit4)?;

        info!(
            "sdio: init done sd_v2={} high_capacity={} rca={:#x} ocr={:#x}",
            sd_v2, self.high_capacity, self.rca, ocr.raw
        );
        Ok(CardInfo {
            sd_v2,
            high_capacity: self.high_capacity,
            ocr: ocr.raw,
            rca: self.rca,
            capacity_blocks,
            cid,
        })
    }

    /// Check CMD8 response
    fn check_cmd8(&mut self) -> Result<bool, Error> {
        let cmd = crate::cmd::cmd8(0x01, 0xAA);
        match self.host.send_command(&cmd)? {
            Response::R7(resp) => Ok(resp.verify(0x01, 0xAA)),
            _ => Ok(false),
        }
    }

    /// Send ACMD41 until card is ready
    fn wait_ready(&mut self, sd_v2: bool) -> Result<OcrResponse, Error> {
        let mut elapsed = 0u32;
        loop {
            let cmd55 = crate::cmd::cmd55(0);
            self.host.send_command(&cmd55)?;

            let acmd41 = crate::cmd::cmd41(sd_v2, 0xFF8000);
            match self.host.send_command(&acmd41)? {
                Response::R3(ocr) => {
                    if ocr.card_powered_up() {
                        return Ok(ocr);
                    }
                }
                _ => return Err(Error::BadResponse(ErrorContext::for_cmd(Phase::Init, 41))),
            }

            if elapsed >= Self::INIT_TIMEOUT_MS {
                warn_!("sdio: ACMD41 timed out after {}ms", elapsed);
                return Err(Error::Timeout(ErrorContext::for_cmd(Phase::Init, 41)));
            }
            self.delay.delay_ms(Self::INIT_POLL_MS);
            elapsed = elapsed.saturating_add(Self::INIT_POLL_MS);
        }
    }

    /// CMD3: get relative card address
    fn get_rca(&mut self) -> Result<u16, Error> {
        match self.host.send_command(&crate::cmd::CMD3_SD)? {
            Response::R6(resp) => Ok(resp.rca()),
            _ => Err(Error::BadResponse(ErrorContext::for_cmd(Phase::Init, 3))),
        }
    }

    /// Switch bus width via ACMD6
    fn set_bus_width(&mut self, width: BusWidth) -> Result<(), Error> {
        // CMD55
        let cmd55 = crate::cmd::cmd55(self.rca);
        self.host.send_command(&cmd55)?;

        // ACMD6: set bus width — ACMD6 only encodes 1-bit or 4-bit. 8-bit
        // bus configuration on eMMC is done through MMC CMD6 (SWITCH) and is
        // not part of this driver's ACMD6 path.
        let arg = match width {
            BusWidth::Bit1 => 0,
            BusWidth::Bit4 => 2,
            BusWidth::Bit8 => return Err(Error::UnsupportedCommand),
        };
        let acmd6 = Command::new(6, arg, ResponseType::R1);
        self.host.send_command(&acmd6)?;

        self.host.set_bus_width(width)?;
        self.bus_width = width;
        Ok(())
    }

    // ── Data Transfer ───────────────────────────────────────────

    /// Read a single 512-byte block
    pub fn read_block(&mut self, addr: u32, buf: &mut [u8; 512]) -> Result<(), Error> {
        let block_addr = block_addr_of(addr, self.high_capacity);
        self.host
            .prepare_data_transfer(DataDirection::Read, 512, 1)?;
        let cmd = crate::cmd::cmd17(block_addr);
        self.host.send_command(&cmd)?;
        self.host.read_data(buf, 512)
    }

    /// Write a single 512-byte block
    pub fn write_block(&mut self, addr: u32, buf: &[u8; 512]) -> Result<(), Error> {
        let block_addr = block_addr_of(addr, self.high_capacity);
        self.host
            .prepare_data_transfer(DataDirection::Write, 512, 1)?;
        let cmd = crate::cmd::cmd24(block_addr);
        self.host.send_command(&cmd)?;
        self.host.write_data(buf, 512)
    }

    /// Read multiple blocks
    pub fn read_blocks<F>(&mut self, addr: u32, count: u32, mut handler: F) -> Result<(), Error>
    where
        F: FnMut(u32, &[u8; 512]),
    {
        let block_addr = block_addr_of(addr, self.high_capacity);
        self.host
            .prepare_data_transfer(DataDirection::Read, 512, count)?;
        self.host.set_block_count(count)?;
        let cmd = crate::cmd::cmd18(block_addr);
        self.host.send_command(&cmd)?;

        let mut buf = [0u8; 512];
        for i in 0..count {
            self.host.read_data(&mut buf, 512)?;
            handler(addr + i, &buf);
        }

        // CMD12: stop
        self.host.send_command(&crate::cmd::CMD12)?;
        Ok(())
    }

    /// Write multiple blocks
    pub fn write_blocks(&mut self, addr: u32, blocks: &[[u8; 512]]) -> Result<(), Error> {
        let block_addr = block_addr_of(addr, self.high_capacity);
        let count = blocks.len() as u32;
        self.host
            .prepare_data_transfer(DataDirection::Write, 512, count)?;
        self.host.set_block_count(count)?;
        let cmd = crate::cmd::cmd25(block_addr);
        self.host.send_command(&cmd)?;

        for block in blocks {
            self.host.write_data(block, 512)?;
        }

        // CMD12: stop
        self.host.send_command(&crate::cmd::CMD12)?;
        Ok(())
    }

    /// Erase a range of blocks
    pub fn erase(&mut self, start: u32, end: u32) -> Result<(), Error> {
        let start_addr = block_addr_of(start, self.high_capacity);
        let end_addr = block_addr_of(end, self.high_capacity);

        let cmd32 = crate::cmd::cmd32(start_addr);
        self.host.send_command(&cmd32)?;

        let cmd33 = crate::cmd::cmd33(end_addr);
        self.host.send_command(&cmd33)?;

        self.host.send_command(&crate::cmd::CMD38)?;
        Ok(())
    }

    /// Get card status
    pub fn status(&mut self) -> Result<CardState, Error> {
        let cmd13 = crate::cmd::cmd13(self.rca);
        match self.host.send_command(&cmd13)? {
            Response::R1(r1) => Ok(r1.current_state()),
            _ => Err(Error::BadResponse(ErrorContext::for_cmd(
                Phase::ResponseWait,
                13,
            ))),
        }
    }

    /// Issue a CMD6 SWITCH_FUNC and read back the 64-byte status block.
    ///
    /// Use [`SdioSdmmc::switch_to_high_speed`] for the most common case
    /// (group 1 → high-speed). This lower-level entry point exposes the
    /// raw [`SwitchStatus`] for callers that need to inspect other groups.
    pub fn switch_function(&mut self, cmd: &Command) -> Result<SwitchStatus, Error> {
        // CMD6 SWITCH_FUNC has a 64-byte read data phase. The host trait
        // can't infer this from the command index alone (ACMD6 also uses
        // index 6 with no data phase), so signal it explicitly here.
        self.host
            .prepare_data_transfer(DataDirection::Read, 64, 1)?;
        self.host.send_command(cmd)?;
        let mut buf = [0u8; 64];
        self.host.read_data(&mut buf, 64)?;
        Ok(SwitchStatus::from_raw(buf))
    }

    /// Switch the card to high-speed (50 MHz) by issuing CMD6 with mode=1
    /// and group 1 = 1. Returns `Ok(true)` if the card reports high-speed
    /// active; `Ok(false)` if it acknowledged the command but didn't switch
    /// (e.g. unsupported); `Err` if the bus transaction itself failed.
    ///
    /// The host is responsible for actually raising the bus clock after this
    /// returns success; this driver only handles the protocol-level switch.
    pub fn switch_to_high_speed(&mut self) -> Result<bool, Error> {
        let status = self.switch_function(&crate::cmd::cmd6_high_speed(true))?;
        let active = status.high_speed_active();
        if active {
            info!("sdio: switched to high-speed mode");
        } else {
            warn_!("sdio: high-speed switch did not take effect");
        }
        Ok(active)
    }
}

/// Card information obtained during SDIO initialization
#[derive(Debug, Clone, Copy)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub struct CardInfo {
    pub sd_v2: bool,
    pub high_capacity: bool,
    pub ocr: u32,
    pub rca: u16,
    /// User-data capacity in 512-byte blocks, parsed from the CSD.
    /// `None` if the CSD reports a structure version we do not yet support.
    pub capacity_blocks: Option<u64>,
    /// Card identification register (manufacturer / OEM / serial / date).
    /// `None` if the host returned an unexpected response type to CMD2.
    pub cid: Option<CidResponse>,
}

#[cfg(test)]
mod tests {
    extern crate std;

    use super::*;
    use crate::response::{IfCondResponse, OcrResponse, R1Response, RcaResponse};
    use std::vec::Vec;

    struct NullDelay;

    impl DelayNs for NullDelay {
        fn delay_ns(&mut self, _ns: u32) {}
    }

    /// Mock host that replays canned responses in order. Used to verify the
    /// init sequence and that the driver tracks RCA on its own.
    struct MockHost {
        replies: Vec<Response>,
        commands: Vec<Command>,
        bus_width: Option<BusWidth>,
        next_read_payload: Option<Vec<u8>>,
    }

    impl MockHost {
        fn new(replies: Vec<Response>) -> Self {
            Self {
                replies,
                commands: Vec::new(),
                bus_width: None,
                next_read_payload: None,
            }
        }
    }

    impl SdioHost for MockHost {
        fn send_command(&mut self, cmd: &Command) -> Result<Response, Error> {
            self.commands.push(*cmd);
            if self.replies.is_empty() {
                return Err(Error::Timeout(ErrorContext::default()));
            }
            Ok(self.replies.remove(0))
        }

        fn read_data(&mut self, buf: &mut [u8], _block_size: u32) -> Result<(), Error> {
            match self.next_read_payload.take() {
                Some(data) if data.len() == buf.len() => {
                    buf.copy_from_slice(&data);
                    Ok(())
                }
                _ => Err(Error::UnsupportedCommand),
            }
        }

        fn write_data(&mut self, _buf: &[u8], _block_size: u32) -> Result<(), Error> {
            Err(Error::UnsupportedCommand)
        }

        fn set_bus_width(&mut self, width: BusWidth) -> Result<(), Error> {
            self.bus_width = Some(width);
            Ok(())
        }

        fn set_clock(&mut self, _speed: ClockSpeed) -> Result<(), Error> {
            Ok(())
        }
    }

    fn ok_r1() -> Response {
        Response::R1(R1Response::from_native_raw(0).unwrap())
    }

    fn rca_response(rca: u16) -> Response {
        Response::R6(RcaResponse::from_raw((rca as u32) << 16))
    }

    fn ocr_ready_sdhc() -> Response {
        // bit 31 = power-up done, bit 30 = CCS (high capacity)
        Response::R3(OcrResponse::from_raw(0xC0FF_8000))
    }

    fn csd_v2_response() -> Response {
        let mut raw = [0u8; 16];
        raw[0] = 0x40;
        raw[7] = 0x00;
        raw[8] = 0x0F;
        raw[9] = 0x0F;
        Response::R2(raw)
    }

    fn cid_response() -> Response {
        let mut raw = [0u8; 16];
        raw[0] = 0x03;
        raw[1] = b'S';
        raw[2] = b'D';
        raw[3] = b'A';
        raw[4] = b'B';
        raw[5] = b'C';
        raw[6] = b'1';
        raw[7] = b'2';
        Response::R2(raw)
    }

    #[test]
    fn init_records_rca_in_driver_state() {
        let replies = std::vec![
            ok_r1(),                                             // CMD0
            Response::R7(IfCondResponse::from_raw(0x0000_01AA)), // CMD8
            ok_r1(),                                             // CMD55 (ACMD41 prologue)
            ocr_ready_sdhc(),                                    // ACMD41
            cid_response(),                                      // CMD2 (CID)
            rca_response(0x1234),                                // CMD3
            csd_v2_response(),                                   // CMD9
            ok_r1(),                                             // CMD7 (select)
            ok_r1(),                                             // CMD55 (ACMD6 prologue)
            ok_r1(),                                             // ACMD6
        ];
        let host = MockHost::new(replies);
        let mut driver = SdioSdmmc::new(host, NullDelay);
        let info = driver.init().unwrap();

        assert_eq!(info.rca, 0x1234);
        assert_eq!(driver.rca(), 0x1234);
        assert!(info.high_capacity);
        assert_eq!(info.capacity_blocks, Some((0x0F0F + 1) * 1024));
        let cid = info.cid.expect("CID captured in init");
        assert_eq!(cid.manufacturer_id(), 0x03);
        assert_eq!(&cid.product_name(), b"ABC12");
        assert_eq!(driver.host.bus_width, Some(BusWidth::Bit4));

        // Verify CMD7 / CMD55 / ACMD6 used the recorded RCA, not 0.
        let cmd7 = driver
            .host
            .commands
            .iter()
            .find(|c| c.cmd == 7)
            .expect("CMD7 issued");
        assert_eq!(cmd7.arg, (0x1234u32) << 16);
    }

    #[test]
    fn set_bus_width_bit8_is_unsupported_via_acmd6() {
        let mut driver = SdioSdmmc::new(MockHost::new(std::vec![ok_r1()]), NullDelay);
        driver.rca = 0x1;
        assert_eq!(
            driver.set_bus_width(BusWidth::Bit8),
            Err(Error::UnsupportedCommand)
        );
    }

    #[test]
    fn switch_to_high_speed_returns_true_when_status_confirms() {
        let mut host = MockHost::new(std::vec![ok_r1()]);
        // Stage the 64-byte status block where group 1 reports HS active.
        let mut status = std::vec![0u8; 64];
        status[16] = 0x01;
        host.next_read_payload = Some(status);

        let mut driver = SdioSdmmc::new(host, NullDelay);
        let active = driver.switch_to_high_speed().unwrap();
        assert!(active);
        let cmd6 = driver
            .host
            .commands
            .iter()
            .find(|c| c.cmd == 6)
            .expect("CMD6 issued");
        assert_eq!(cmd6.arg, 0x80FF_FFF1);
    }

    #[test]
    fn switch_to_high_speed_returns_false_when_card_keeps_default() {
        let mut host = MockHost::new(std::vec![ok_r1()]);
        host.next_read_payload = Some(std::vec![0u8; 64]); // group 1 = 0
        let mut driver = SdioSdmmc::new(host, NullDelay);
        let active = driver.switch_to_high_speed().unwrap();
        assert!(!active);
    }
}

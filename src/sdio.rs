//! SDIO (Secure Digital Input Output) mode transport layer
//!
//! SDIO mode uses a dedicated host controller with 1-bit or 4-bit data bus.
//! Implement [`SdioHost`] for your platform's SDIO peripheral.

use crate::cmd::Command;
use crate::error::Error;
use crate::response::{CardState, OcrResponse, Response, ResponseType};

/// SDIO bus width
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub enum BusWidth {
    /// 1-bit bus
    Bit1,
    /// 4-bit bus
    Bit4,
    /// 8-bit bus (eMMC)
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

/// Trait that the platform must implement for the SDIO host controller
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

    /// Get the current relative card address
    fn rca(&self) -> u16;
}

/// SDIO mode SD/MMC driver
pub struct SdioSdmmc<H: SdioHost> {
    host: H,
    high_capacity: bool,
    bus_width: BusWidth,
}

impl<H: SdioHost> SdioSdmmc<H> {
    pub fn new(host: H) -> Self {
        Self {
            host,
            high_capacity: false,
            bus_width: BusWidth::Bit1,
        }
    }

    /// Initialize the card in SDIO mode
    pub fn init(&mut self) -> Result<CardInfo, Error> {
        // CMD0: reset
        self.host.send_command(&crate::cmd::CMD0)?;

        // CMD8: check SD v2
        let sd_v2 = self.check_cmd8()?;

        // ACMD41: initialize
        self.wait_ready(sd_v2)?;

        // CMD2: get CID
        self.host.send_command(&crate::cmd::CMD2)?;

        // CMD3: get RCA
        let rca = self.get_rca()?;

        // CMD9: get CSD
        let cmd9 = crate::cmd::cmd9(rca);
        self.host.send_command(&cmd9)?;

        // CMD7: select card
        let cmd7 = crate::cmd::cmd7(rca);
        self.host.send_command(&cmd7)?;

        // CMD58: read OCR
        let ocr = self.read_ocr()?;
        self.high_capacity = ocr.ccs();

        // Set bus width to 4-bit
        self.set_bus_width(BusWidth::Bit4)?;

        Ok(CardInfo {
            sd_v2,
            high_capacity: self.high_capacity,
            ocr: ocr.raw,
            rca,
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
    fn wait_ready(&mut self, sd_v2: bool) -> Result<(), Error> {
        for _ in 0..1000 {
            let cmd55 = crate::cmd::cmd55(0);
            self.host.send_command(&cmd55)?;

            let acmd41 = crate::cmd::cmd41(sd_v2, 0xFF8000);
            match self.host.send_command(&acmd41)? {
                Response::R3(ocr) => {
                    if ocr.card_powered_up() {
                        return Ok(());
                    }
                }
                _ => return Err(Error::BadResponse),
            }
        }
        Err(Error::Timeout)
    }

    /// CMD3: get relative card address
    fn get_rca(&mut self) -> Result<u16, Error> {
        match self.host.send_command(&crate::cmd::CMD3_SD)? {
            Response::R6(resp) => Ok(resp.rca()),
            _ => Err(Error::BadResponse),
        }
    }

    /// CMD58: read OCR
    fn read_ocr(&mut self) -> Result<OcrResponse, Error> {
        match self.host.send_command(&crate::cmd::CMD58)? {
            Response::R3(ocr) => Ok(ocr),
            _ => Err(Error::BadResponse),
        }
    }

    /// Switch bus width via ACMD6
    fn set_bus_width(&mut self, width: BusWidth) -> Result<(), Error> {
        let rca = self.host.rca();

        // CMD55
        let cmd55 = crate::cmd::cmd55(rca);
        self.host.send_command(&cmd55)?;

        // ACMD6: set bus width
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
        let block_addr = if self.high_capacity { addr } else { addr * 512 };
        let cmd = crate::cmd::cmd17(block_addr);
        self.host.send_command(&cmd)?;
        self.host.read_data(buf, 512)
    }

    /// Write a single 512-byte block
    pub fn write_block(&mut self, addr: u32, buf: &[u8; 512]) -> Result<(), Error> {
        let block_addr = if self.high_capacity { addr } else { addr * 512 };
        let cmd = crate::cmd::cmd24(block_addr);
        self.host.send_command(&cmd)?;
        self.host.write_data(buf, 512)
    }

    /// Read multiple blocks
    pub fn read_blocks<F>(&mut self, addr: u32, count: u32, mut handler: F) -> Result<(), Error>
    where
        F: FnMut(u32, &[u8; 512]),
    {
        let block_addr = if self.high_capacity { addr } else { addr * 512 };
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
        let block_addr = if self.high_capacity { addr } else { addr * 512 };
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
        let start_addr = if self.high_capacity {
            start
        } else {
            start * 512
        };
        let end_addr = if self.high_capacity { end } else { end * 512 };

        let cmd32 = crate::cmd::cmd32(start_addr);
        self.host.send_command(&cmd32)?;

        let cmd33 = crate::cmd::cmd33(end_addr);
        self.host.send_command(&cmd33)?;

        self.host.send_command(&crate::cmd::CMD38)?;
        Ok(())
    }

    /// Get card status
    pub fn status(&mut self) -> Result<CardState, Error> {
        let cmd13 = crate::cmd::cmd13(self.host.rca());
        match self.host.send_command(&cmd13)? {
            Response::R1(r1) => Ok(r1.current_state()),
            _ => Err(Error::BadResponse),
        }
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
}

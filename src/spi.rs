//! SPI mode transport layer for SD/MMC cards
//!
//! Usage: implement [`SpiTransport`] for your platform's SPI peripheral,
//! then use [`SpiSdmmc`] to interact with the card.

use embedded_hal::spi::SpiDevice;

use crate::cmd::Command;
use crate::error::Error;
use crate::response::{IfCondResponse, OcrResponse, R1Response, Response, ResponseType};

/// Token markers for SPI mode data transfer
const TOKEN_START_BLOCK: u8 = 0xFE;
const TOKEN_START_MULTI_BLOCK: u8 = 0xFC;
const TOKEN_STOP_TRAN: u8 = 0xFD;

/// SPI transport trait — users implement this for their platform
pub trait SpiTransport {
    /// Send and receive a single byte
    fn transfer_byte(&mut self, byte: u8) -> Result<u8, Error>;
    /// Send a byte (ignore response)
    fn send_byte(&mut self, byte: u8) -> Result<(), Error> {
        self.transfer_byte(byte)?;
        Ok(())
    }
    /// Send 8 clock cycles (write 0xFF)
    fn clock(&mut self) -> Result<(), Error> {
        self.transfer_byte(0xFF)?;
        Ok(())
    }
}

/// Blanket impl for embedded-hal v1 `SpiDevice<u8>`
impl<SPI> SpiTransport for SpiDeviceWrapper<SPI>
where
    SPI: SpiDevice<u8>,
{
    fn transfer_byte(&mut self, byte: u8) -> Result<u8, Error> {
        let mut buf = [byte];
        self.spi
            .transfer(&mut buf, &[byte])
            .map_err(|_| Error::BusError)?;
        Ok(buf[0])
    }
}

/// Wrapper that owns an `SpiDevice`
pub struct SpiDeviceWrapper<SPI> {
    spi: SPI,
}

impl<SPI> SpiDeviceWrapper<SPI> {
    pub fn new(spi: SPI) -> Self {
        Self { spi }
    }
}

/// SPI mode SD/MMC driver
pub struct SpiSdmmc<T: SpiTransport> {
    transport: T,
    sd_v2: bool,
    high_capacity: bool,
}

impl<T: SpiTransport> SpiSdmmc<T> {
    pub fn new(transport: T) -> Self {
        Self {
            transport,
            sd_v2: false,
            high_capacity: false,
        }
    }

    // ── Initialization ──────────────────────────────────────────

    /// Initialize the card. Must be called before any other operation.
    ///
    /// Performs the standard SD card initialization sequence:
    /// 1. Send 80+ clock cycles (CMD0 preamble)
    /// 2. CMD0 → idle
    /// 3. CMD8 → detect SD v2
    /// 4. ACMD41 → wait for card ready
    /// 5. CMD58 → determine capacity type (SDHC vs SDSC)
    pub fn init(&mut self) -> Result<CardInfo, Error> {
        for _ in 0..10 {
            self.transport.clock()?;
        }

        self.send_command(&crate::cmd::CMD0)?;
        self.sd_v2 = self.check_cmd8()?;
        self.wait_ready()?;

        let ocr = self.read_ocr()?;
        self.high_capacity = ocr.ccs();

        Ok(CardInfo {
            sd_v2: self.sd_v2,
            high_capacity: self.high_capacity,
            ocr: ocr.raw,
        })
    }

    fn check_cmd8(&mut self) -> Result<bool, Error> {
        let cmd = crate::cmd::cmd8(0x01, 0xAA);
        match self.send_command_raw(&cmd) {
            Ok(Response::R7(resp)) => Ok(resp.verify(0x01, 0xAA)),
            Ok(Response::R1(resp)) if resp.illegal_command() => Ok(false),
            Ok(_) => Err(Error::BadResponse),
            Err(Error::Timeout) => Ok(false),
            Err(e) => Err(e),
        }
    }

    fn wait_ready(&mut self) -> Result<(), Error> {
        for _ in 0..1000 {
            let cmd55 = crate::cmd::cmd55(0);
            self.send_command(&cmd55)?;

            let acmd41 = crate::cmd::cmd41(self.sd_v2, 0xFF8000);
            let spi_acmd41 = Command::new(acmd41.cmd, acmd41.arg, ResponseType::R1);
            match self.send_command_raw(&spi_acmd41)? {
                Response::R1(r1) => {
                    if !r1.idle() {
                        return Ok(());
                    }
                }
                _ => return Err(Error::BadResponse),
            }
        }
        Err(Error::Timeout)
    }

    fn read_ocr(&mut self) -> Result<OcrResponse, Error> {
        match self.send_command_raw(&crate::cmd::CMD58)? {
            Response::R3(ocr) => Ok(ocr),
            _ => Err(Error::BadResponse),
        }
    }

    // ── Data Transfer ───────────────────────────────────────────

    /// Read a single 512-byte block at the given address
    pub fn read_block(&mut self, addr: u32, buf: &mut [u8; 512]) -> Result<(), Error> {
        let block_addr = self.block_addr(addr);
        let cmd = crate::cmd::cmd17(block_addr);
        self.send_command(&cmd)?;
        self.read_data_block(buf)
    }

    /// Write a single 512-byte block at the given address
    pub fn write_block(&mut self, addr: u32, buf: &[u8; 512]) -> Result<(), Error> {
        let block_addr = self.block_addr(addr);
        let cmd = crate::cmd::cmd24(block_addr);
        self.send_command(&cmd)?;
        self.write_data_block(buf)
    }

    /// Read multiple blocks starting at `addr`
    pub fn read_blocks<F>(&mut self, addr: u32, count: u32, mut handler: F) -> Result<(), Error>
    where
        F: FnMut(u32, &[u8; 512]),
    {
        let block_addr = self.block_addr(addr);
        let cmd = crate::cmd::cmd18(block_addr);
        self.send_command(&cmd)?;

        let mut buf = [0u8; 512];
        for i in 0..count {
            self.read_data_block(&mut buf)?;
            handler(addr + i, &buf);
        }

        self.send_command(&crate::cmd::CMD12)?;
        self.wait_not_busy()?;
        Ok(())
    }

    /// Write multiple blocks starting at `addr`
    pub fn write_blocks(&mut self, addr: u32, blocks: &[[u8; 512]]) -> Result<(), Error> {
        let block_addr = self.block_addr(addr);
        let cmd = crate::cmd::cmd25(block_addr);
        self.send_command(&cmd)?;

        for block in blocks {
            self.transport.send_byte(TOKEN_START_MULTI_BLOCK)?;
            for &b in block {
                self.transport.send_byte(b)?;
            }
            // CRC (ignored in SPI mode but must be sent)
            self.transport.send_byte(0xFF)?;
            self.transport.send_byte(0xFF)?;

            let resp = self.wait_for_response(100)?;
            if (resp & 0x1F) != 0x05 {
                return Err(Error::WriteError);
            }
            self.wait_not_busy()?;
        }

        self.transport.send_byte(TOKEN_STOP_TRAN)?;
        self.transport.clock()?;
        self.wait_not_busy()?;
        Ok(())
    }

    // ── Low-level helpers ───────────────────────────────────────

    fn block_addr(&self, addr: u32) -> u32 {
        if self.high_capacity { addr } else { addr * 512 }
    }

    fn send_command(&mut self, cmd: &Command) -> Result<R1Response, Error> {
        let resp = self.send_command_raw(cmd)?;
        match resp {
            Response::R1(r1) | Response::R1b(r1) => Ok(r1),
            _ => Err(Error::BadResponse),
        }
    }

    fn send_command_raw(&mut self, cmd: &Command) -> Result<Response, Error> {
        let bytes = cmd.to_spi_bytes();
        for &b in &bytes {
            self.transport.send_byte(b)?;
        }

        match cmd.resp_type {
            crate::response::ResponseType::None => {
                let r1 = self.read_r1()?;
                Ok(Response::R1(r1))
            }
            crate::response::ResponseType::R1 => {
                let r1 = self.read_r1()?;
                Ok(Response::R1(r1))
            }
            crate::response::ResponseType::R1b => {
                let r1 = self.read_r1()?;
                self.wait_not_busy()?;
                Ok(Response::R1b(r1))
            }
            crate::response::ResponseType::R3 => {
                self.read_r1()?;
                let mut ocr = [0u8; 4];
                for b in &mut ocr {
                    *b = self.transport.transfer_byte(0xFF)?;
                }
                let raw = u32::from_be_bytes(ocr);
                Ok(Response::R3(OcrResponse::from_raw(raw)))
            }
            crate::response::ResponseType::R7 => {
                let r1 = self.read_r1()?;
                if r1.illegal_command() {
                    return Ok(Response::R1(r1));
                }
                let mut data = [0u8; 4];
                for b in &mut data {
                    *b = self.transport.transfer_byte(0xFF)?;
                }
                let raw = u32::from_be_bytes(data);
                Ok(Response::R7(IfCondResponse::from_raw(raw)))
            }
            crate::response::ResponseType::R2 => {
                let r1 = self.read_r1()?;
                if r1.raw != 0 {
                    return Ok(Response::R1(r1));
                }
                let mut buf = [0u8; 16];
                for b in &mut buf {
                    *b = self.transport.transfer_byte(0xFF)?;
                }
                Ok(Response::R2(buf))
            }
            _ => {
                let r1 = self.read_r1()?;
                Ok(Response::R1(r1))
            }
        }
    }

    fn read_r1(&mut self) -> Result<R1Response, Error> {
        let raw = self.wait_for_response(100)?;
        R1Response::from_raw(raw as u32)
    }

    fn wait_for_response(&mut self, retries: u32) -> Result<u8, Error> {
        for _ in 0..retries {
            let b = self.transport.transfer_byte(0xFF)?;
            if b != 0xFF {
                return Ok(b);
            }
        }
        Err(Error::Timeout)
    }

    fn read_data_block(&mut self, buf: &mut [u8; 512]) -> Result<(), Error> {
        let mut found_start = false;
        for _ in 0..10000 {
            let b = self.transport.transfer_byte(0xFF)?;
            if b == TOKEN_START_BLOCK {
                found_start = true;
                break;
            }
            if b != 0xFF {
                return Err(Error::ReadError);
            }
        }
        if !found_start {
            return Err(Error::Timeout);
        }

        for b in buf.iter_mut() {
            *b = self.transport.transfer_byte(0xFF)?;
        }

        // 2 CRC bytes (ignored in SPI mode)
        self.transport.clock()?;
        self.transport.clock()?;
        Ok(())
    }

    fn write_data_block(&mut self, buf: &[u8; 512]) -> Result<(), Error> {
        self.transport.send_byte(TOKEN_START_BLOCK)?;
        for &b in buf {
            self.transport.send_byte(b)?;
        }
        self.transport.send_byte(0xFF)?;
        self.transport.send_byte(0xFF)?;

        let resp = self.wait_for_response(100)?;
        if (resp & 0x1F) != 0x05 {
            return Err(Error::WriteError);
        }

        self.wait_not_busy()?;
        Ok(())
    }

    fn wait_not_busy(&mut self) -> Result<(), Error> {
        for _ in 0..500_000 {
            if self.transport.transfer_byte(0xFF)? == 0xFF {
                return Ok(());
            }
        }
        Err(Error::Timeout)
    }
}

/// Card information obtained during initialization
#[derive(Debug, Clone, Copy)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub struct CardInfo {
    pub sd_v2: bool,
    pub high_capacity: bool,
    pub ocr: u32,
}

#[cfg(test)]
mod tests {
    extern crate std;

    use super::*;
    use crate::cmd;
    use std::vec::Vec;

    struct ScriptedTransport {
        rx: Vec<u8>,
        tx: Vec<u8>,
    }

    impl ScriptedTransport {
        fn new(rx: Vec<u8>) -> Self {
            Self { rx, tx: Vec::new() }
        }

        fn push_ignored(rx: &mut Vec<u8>, count: usize) {
            for _ in 0..count {
                rx.push(0xFF);
            }
        }

        fn push_command_response(rx: &mut Vec<u8>, r1: u8, extra: &[u8]) {
            Self::push_ignored(rx, 6);
            rx.push(r1);
            rx.extend_from_slice(extra);
        }

        fn tx_contains(&self, bytes: &[u8]) -> bool {
            self.tx.windows(bytes.len()).any(|window| window == bytes)
        }
    }

    impl SpiTransport for ScriptedTransport {
        fn transfer_byte(&mut self, byte: u8) -> Result<u8, Error> {
            self.tx.push(byte);
            if self.rx.is_empty() {
                return Err(Error::Timeout);
            }
            Ok(self.rx.remove(0))
        }
    }

    #[test]
    fn init_polls_acmd41_until_spi_r1_leaves_idle() {
        let mut rx = Vec::new();
        ScriptedTransport::push_ignored(&mut rx, 10);
        ScriptedTransport::push_command_response(&mut rx, 0x01, &[]);
        ScriptedTransport::push_command_response(&mut rx, 0x01, &[0x00, 0x00, 0x01, 0xAA]);
        ScriptedTransport::push_command_response(&mut rx, 0x01, &[]);
        ScriptedTransport::push_command_response(&mut rx, 0x01, &[]);
        ScriptedTransport::push_command_response(&mut rx, 0x01, &[]);
        ScriptedTransport::push_command_response(&mut rx, 0x00, &[]);
        ScriptedTransport::push_command_response(&mut rx, 0x00, &[0xC0, 0xFF, 0x80, 0x00]);

        let mut driver = SpiSdmmc::new(ScriptedTransport::new(rx));
        let info = driver.init().unwrap();

        assert!(info.sd_v2);
        assert!(info.high_capacity);
        assert_eq!(info.ocr, 0xC0FF_8000);
        assert!(driver.transport.tx_contains(&cmd::CMD0.to_spi_bytes()));
        assert!(
            driver
                .transport
                .tx_contains(&cmd::cmd8(0x01, 0xAA).to_spi_bytes())
        );
        assert!(driver.transport.tx_contains(&cmd::CMD58.to_spi_bytes()));
    }

    #[test]
    fn read_block_times_out_when_start_token_never_arrives() {
        let mut rx = Vec::new();
        ScriptedTransport::push_command_response(&mut rx, 0x00, &[]);
        ScriptedTransport::push_ignored(&mut rx, 10_000);
        rx.extend_from_slice(&[0xAA; 512]);
        rx.extend_from_slice(&[0xFF; 2]);

        let mut driver = SpiSdmmc::new(ScriptedTransport::new(rx));
        driver.high_capacity = true;
        let mut buf = [0u8; 512];

        assert_eq!(driver.read_block(7, &mut buf), Err(Error::Timeout));
        assert!(driver.transport.tx_contains(&cmd::cmd17(7).to_spi_bytes()));
    }
}

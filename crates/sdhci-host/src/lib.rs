//! SDHCI host controller backend for the `sdmmc-protocol` driver crate.
//!
//! This crate ports the [SD Host Controller Standard Specification][sdhci]
//! v3.x register layout and PIO data path into a [`SdioHost`] implementation
//! that the [`sdmmc_protocol::sdio::SdioSdmmc`] driver can drive directly.
//!
//! # Scope
//!
//! - **Implemented**: PIO transfers, 1-bit / 4-bit bus, default-speed and
//!   high-speed clocking, 32-bit response slots, 136-bit R2 reconstruction,
//!   software reset / clock setup.
//! - **Out of scope (for now)**: DMA, ADMA2, 8-bit eMMC bus, HS200 / SDR50 /
//!   SDR104 clocking, voltage / signaling switch (CMD11), tuning (CMD19 /
//!   CMD21), eMMC-specific commands.
//!
//! # Usage
//!
//! ```no_run
//! use embedded_hal::delay::DelayNs;
//! use sdmmc_protocol::sdio::SdioSdmmc;
//! use sdhci_host::Sdhci;
//!
//! # fn make_delay() -> impl DelayNs { struct N; impl DelayNs for N { fn delay_ns(&mut self, _: u32) {} } N }
//! let host = unsafe { Sdhci::new(0xFE31_0000) };
//! let delay = make_delay();
//! let mut card = SdioSdmmc::new(host, delay);
//! // card.init()?;
//! ```
//!
//! Construction is `unsafe` because the caller must guarantee that the
//! supplied address is a valid, exclusively-owned SDHCI register file.
//!
//! [sdhci]: https://www.sdcard.org/downloads/pls/

#![no_std]
#![allow(clippy::missing_safety_doc)]

mod command;
mod data;
mod host;
mod regs;

pub use host::Sdhci;

use sdmmc_protocol::cmd::{Command, DataDirection};
use sdmmc_protocol::error::{Error, ErrorContext, Phase};
use sdmmc_protocol::response::Response;
use sdmmc_protocol::sdio::{BusWidth, ClockSpeed, SdioHost};

use crate::host::PendingData;
use crate::regs::*;

impl SdioHost for Sdhci {
    fn send_command(&mut self, cmd: &Command) -> Result<Response, Error> {
        self.issue_command(cmd)
    }

    fn read_data(&mut self, buf: &mut [u8], block_size: u32) -> Result<(), Error> {
        self.pio_read(buf, block_size, 0)
    }

    fn write_data(&mut self, buf: &[u8], block_size: u32) -> Result<(), Error> {
        self.pio_write(buf, block_size, 0)
    }

    fn set_bus_width(&mut self, width: BusWidth) -> Result<(), Error> {
        let mut ctrl = self.read_u8(REG_HOST_CONTROL1);
        ctrl &= !(HOST_CTRL1_4BIT | HOST_CTRL1_8BIT);
        match width {
            BusWidth::Bit1 => {}
            BusWidth::Bit4 => ctrl |= HOST_CTRL1_4BIT,
            // 8-bit is eMMC territory and is intentionally not part of the
            // MVP — surface it as Unsupported so the protocol layer can
            // refuse cleanly instead of silently writing the bit and
            // misconfiguring the bus.
            BusWidth::Bit8 => return Err(Error::UnsupportedCommand),
        }
        self.write_u8(REG_HOST_CONTROL1, ctrl);
        Ok(())
    }

    fn set_clock(&mut self, speed: ClockSpeed) -> Result<(), Error> {
        let target_hz = match speed {
            ClockSpeed::Default | ClockSpeed::Sdr12 => 25_000_000,
            ClockSpeed::HighSpeed | ClockSpeed::Sdr25 => 50_000_000,
            ClockSpeed::Sdr50 | ClockSpeed::Ddr50 => 50_000_000,
            ClockSpeed::Sdr104 => 104_000_000,
        };

        // Toggle the High-Speed Enable bit in HOST_CONTROL1 alongside the
        // divider change so the controller pipelines reflect the new
        // timing window.
        let mut ctrl = self.read_u8(REG_HOST_CONTROL1);
        if matches!(speed, ClockSpeed::Default | ClockSpeed::Sdr12) {
            ctrl &= !HOST_CTRL1_HIGH_SPEED;
        } else {
            ctrl |= HOST_CTRL1_HIGH_SPEED;
        }
        self.write_u8(REG_HOST_CONTROL1, ctrl);

        let base = self.base_clock_hz();
        if base == 0 {
            return Err(Error::BadResponse(ErrorContext::new(Phase::Init)));
        }
        self.enable_clock(base, target_hz)
    }

    fn set_block_count(&mut self, _count: u32) -> Result<(), Error> {
        // We push BLOCK_COUNT in `configure_data_phase` once we know both
        // the count and the direction, so this hint is intentionally a
        // no-op.
        Ok(())
    }

    fn prepare_data_transfer(
        &mut self,
        direction: DataDirection,
        block_size: u32,
        block_count: u32,
    ) -> Result<(), Error> {
        if direction.is_none() {
            self.pending_data = None;
        } else {
            self.pending_data = Some(PendingData {
                direction,
                block_size,
                block_count,
            });
        }
        Ok(())
    }
}

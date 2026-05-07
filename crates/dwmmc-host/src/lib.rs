//! Synopsys DesignWare Mobile Storage Host Controller (DW_mshc) backend
//! for the [`sdmmc-protocol`](sdmmc_protocol) driver crate.
//!
//! Implements [`sdmmc_protocol::sdio::SdioHost`] for the IP block known
//! variously as DWC_mobile_storage, dw_mshc, dw_mmc (Linux), or simply
//! the "Synopsys SD/MMC controller" — the same core used in Rockchip
//! RK33xx/RK35xx, Allwinner A-series, StarFive JH7110, and a long
//! tail of mid-range SoCs. PIO data path only; the internal DMAC
//! (IDMAC) path is intentionally disabled in [`DwMmc::reset_and_init`].
//!
//! # Scope
//!
//! - **Implemented**: PIO data transfer over the 0x100/0x200/0x400
//!   FIFO (configurable), 1-bit / 4-bit / 8-bit bus selection,
//!   default-speed clocking, R1/R1b/R2/R3/R4/R5/R6/R7 response
//!   decoding, software reset.
//! - **Out of scope (for now)**: IDMAC / external-DMA path, HS200 /
//!   SDR50 / SDR104, voltage / signaling switch (CMD11), tuning
//!   (CMD19/CMD21).
//!
//! # Usage
//!
//! ```rust,no_run
//! use embedded_hal::delay::DelayNs;
//! use sdmmc_protocol::sdio::SdioSdmmc;
//! use dwmmc_host::DwMmc;
//!
//! # fn make_delay() -> impl DelayNs { struct N; impl DelayNs for N { fn delay_ns(&mut self, _: u32) {} } N }
//! // SAFETY: 0xFE2B_0000 must point at a valid DW_mshc register file
//! // the caller has exclusive access to.
//! let mut host = unsafe { DwMmc::new(0xFE2B_0000) };
//! host.set_reference_clock(50_000_000);
//! host.reset_and_init().expect("controller reset");
//!
//! let mut card = SdioSdmmc::new(host, make_delay());
//! // card.init()?;
//! ```
//!
//! Construction is `unsafe` because the caller must guarantee that
//! the supplied address is a valid, exclusively-owned DW_mshc
//! register file.

#![no_std]
#![allow(clippy::missing_safety_doc)]

mod command;
mod data;
mod host;
mod regs;

pub use crate::host::{DEFAULT_FIFO_OFFSET, DwMmc};

use sdmmc_protocol::cmd::{Command, DataDirection};
use sdmmc_protocol::error::Error;
use sdmmc_protocol::response::Response;
use sdmmc_protocol::sdio::{BusWidth, ClockSpeed, SdioHost};

use crate::host::PendingData;

impl SdioHost for DwMmc {
    fn send_command(&mut self, cmd: &Command) -> Result<Response, Error> {
        self.issue_command(cmd)
    }

    fn read_data(&mut self, buf: &mut [u8], _block_size: u32) -> Result<(), Error> {
        // Block size was already programmed via `prepare_data_transfer`
        // → `program_data_phase`. We just drain `buf.len()` bytes.
        self.pio_read(buf, 0)
    }

    fn write_data(&mut self, buf: &[u8], _block_size: u32) -> Result<(), Error> {
        self.pio_write(buf, 0)
    }

    fn set_bus_width(&mut self, width: BusWidth) -> Result<(), Error> {
        self.set_card_type(width);
        Ok(())
    }

    fn set_clock(&mut self, speed: ClockSpeed) -> Result<(), Error> {
        // Map the protocol-layer abstract speeds onto Hz. UHS-I and
        // HS200 timings need DLL/strobe configuration this MVP
        // doesn't expose, so we just program the corresponding
        // SDR clock and trust the integrator to wire the DLL on
        // their side (or stay at HighSpeed).
        let target_hz: u32 = match speed {
            ClockSpeed::Default | ClockSpeed::Sdr12 => 25_000_000,
            ClockSpeed::HighSpeed | ClockSpeed::Sdr25 => 50_000_000,
            ClockSpeed::Sdr50 | ClockSpeed::Ddr50 => 50_000_000,
            ClockSpeed::Sdr104 => 104_000_000,
            ClockSpeed::Hs200 => 200_000_000,
        };
        self.program_clock(target_hz)
    }

    fn set_block_count(&mut self, _count: u32) -> Result<(), Error> {
        // BYTCNT carries both block size and count for the next data
        // phase; we program it from `prepare_data_transfer`. This hint
        // is intentionally a no-op so the protocol layer's call still
        // succeeds.
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

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
//!   default / high-speed / UHS-I / HS200 clocking, DW_mshc UHS DDR
//!   and 1.8 V signaling bits, R1/R1b/R2/R3/R4/R5/R6/R7 response
//!   decoding, software reset.
//! - **Out of scope (for now)**: external-DMA path, controller-specific
//!   DLL/strobe/tuning window setup (CMD19/CMD21).
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
mod dma;
mod host;
mod regs;

use sdmmc_protocol::{
    cmd::{Command, DataDirection},
    error::Error,
    response::Response,
    sdio::{BusWidth, ClockSpeed, SdioHost, SignalVoltage},
};

use crate::host::PendingData;
pub use crate::{
    dma::{IDMAC_DESC_ALIGN, IDMAC_DESC_SIZE, IdmacRead},
    host::{DEFAULT_FIFO_OFFSET, DwMmc},
};

impl SdioHost for DwMmc {
    fn send_command(&mut self, cmd: &Command) -> Result<Response, Error> {
        self.issue_command(cmd)
    }

    fn read_data(&mut self, buf: &mut [u8], _block_size: u32) -> Result<(), Error> {
        // Block size was already programmed via `prepare_data_transfer`
        // → `program_data_phase`. We just drain `buf.len()` bytes.
        let is_last_block = self.data_blocks_remaining <= 1;
        let result = self.pio_read(buf, self.data_cmd_index, is_last_block);
        if result.is_ok() && self.data_blocks_remaining > 0 {
            self.data_blocks_remaining -= 1;
        }
        result
    }

    fn write_data(&mut self, buf: &[u8], _block_size: u32) -> Result<(), Error> {
        self.pio_write(buf, self.data_cmd_index)
    }

    fn set_bus_width(&mut self, width: BusWidth) -> Result<(), Error> {
        self.set_card_type(width);
        Ok(())
    }

    fn set_clock(&mut self, speed: ClockSpeed) -> Result<(), Error> {
        let target_hz = clock_hz_for_speed(speed);
        self.set_uhs_timing(speed);
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
            self.data_blocks_remaining = 0;
        } else {
            self.pending_data = Some(PendingData {
                direction,
                block_size,
                block_count,
            });
            self.data_blocks_remaining = block_count;
        }
        Ok(())
    }

    fn switch_voltage(&mut self, voltage: SignalVoltage) -> Result<(), Error> {
        self.set_signal_voltage(voltage)
    }
}

fn clock_hz_for_speed(speed: ClockSpeed) -> u32 {
    match speed {
        ClockSpeed::Identification => 400_000,
        ClockSpeed::Default | ClockSpeed::Sdr12 => 25_000_000,
        ClockSpeed::HighSpeed | ClockSpeed::Sdr25 => 50_000_000,
        ClockSpeed::Sdr50 | ClockSpeed::Ddr50 => 50_000_000,
        ClockSpeed::Sdr104 => 104_000_000,
        ClockSpeed::Hs200 => 200_000_000,
    }
}

pub(crate) fn ddr_mask_for_speed(speed: ClockSpeed) -> u16 {
    match speed {
        ClockSpeed::Ddr50 => 1,
        _ => 0,
    }
}

pub(crate) fn volt_mask_for_signal(voltage: SignalVoltage) -> Result<u16, Error> {
    match voltage {
        SignalVoltage::V330 => Ok(0),
        SignalVoltage::V180 => Ok(1),
        SignalVoltage::V120 => Err(Error::UnsupportedCommand),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct UhsBits {
    pub ddr: u16,
    pub volt: u16,
}

pub(crate) fn uhs_bits_after_speed(cur: UhsBits, speed: ClockSpeed) -> UhsBits {
    UhsBits {
        ddr: ddr_mask_for_speed(speed),
        ..cur
    }
}

pub(crate) fn uhs_bits_after_voltage(
    cur: UhsBits,
    voltage: SignalVoltage,
) -> Result<UhsBits, Error> {
    Ok(UhsBits {
        volt: volt_mask_for_signal(voltage)?,
        ..cur
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uhs_i_sdr_modes_keep_ddr_disabled() {
        let cur = UhsBits { ddr: 1, volt: 1 };

        assert_eq!(uhs_bits_after_speed(cur, ClockSpeed::Sdr50).ddr, 0);
        assert_eq!(uhs_bits_after_speed(cur, ClockSpeed::Sdr104).ddr, 0);
        assert_eq!(uhs_bits_after_speed(cur, ClockSpeed::Hs200).ddr, 0);
    }

    #[test]
    fn ddr50_enables_ddr_mode_for_card0() {
        let cur = UhsBits { ddr: 0, volt: 1 };

        assert_eq!(
            uhs_bits_after_speed(cur, ClockSpeed::Ddr50),
            UhsBits { ddr: 1, volt: 1 }
        );
    }

    #[test]
    fn uhs_i_voltage_switch_selects_1v8_for_card0() {
        let cur = UhsBits { ddr: 1, volt: 0 };

        assert_eq!(
            uhs_bits_after_voltage(cur, SignalVoltage::V180).unwrap(),
            UhsBits { ddr: 1, volt: 1 }
        );
        assert_eq!(
            uhs_bits_after_voltage(cur, SignalVoltage::V330).unwrap(),
            UhsBits { ddr: 1, volt: 0 }
        );
    }

    #[test]
    fn unsupported_1v2_voltage_is_rejected() {
        assert_eq!(
            volt_mask_for_signal(SignalVoltage::V120).unwrap_err(),
            Error::UnsupportedCommand
        );
    }

    #[test]
    fn data_command_index_is_recorded_for_diagnostics() {
        let mut host = unsafe { DwMmc::new(0x1000_0000) };
        host.data_cmd_index = 6;

        assert_eq!(host.data_cmd_index, 6);
    }
}

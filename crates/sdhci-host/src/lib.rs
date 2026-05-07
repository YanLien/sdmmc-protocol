//! SDHCI host controller backend for the `sdmmc-protocol` driver crate.
//!
//! This crate ports the [SD Host Controller Standard Specification][sdhci]
//! v3.x register layout and PIO data path into a [`SdioHost`] implementation
//! that the [`sdmmc_protocol::sdio::SdioSdmmc`] driver can drive directly.
//!
//! # Scope
//!
//! - **Implemented**: PIO transfers, **ADMA2 (32-bit) transfers**, 1-bit /
//!   4-bit bus, default-speed and high-speed clocking, 32-bit response
//!   slots, 136-bit R2 reconstruction, software reset / clock setup.
//! - **Out of scope (for now)**: 64-bit ADMA2, 8-bit eMMC bus, HS200 /
//!   SDR50 / SDR104 clocking, voltage / signaling switch (CMD11), tuning
//!   (CMD19 / CMD21), eMMC-specific commands.
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
//! For ADMA2, wrap the controller in [`SdhciAdma2`] and supply a [`Dma`]
//! implementation plus a caller-owned [`Adma2Buffer`]:
//!
//! ```no_run
//! use embedded_hal::delay::DelayNs;
//! use sdhci_host::{Adma2Buffer, Dma, DmaDir, Sdhci, SdhciAdma2};
//! use sdmmc_protocol::sdio::SdioSdmmc;
//!
//! struct IdentityDma;
//! impl Dma for IdentityDma {
//!     fn map(&self, p: *const u8, _len: usize, _dir: DmaDir) -> u64 { p as u64 }
//!     fn before_dma(&self, _: *const u8, _: usize, _: DmaDir) {}
//!     fn after_dma(&self, _: *const u8, _: usize, _: DmaDir) {}
//! }
//!
//! let table = Adma2Buffer::new();
//!
//! # fn make_delay() -> impl DelayNs { struct N; impl DelayNs for N { fn delay_ns(&mut self, _: u32) {} } N }
//! let inner = unsafe { Sdhci::new(0xFE31_0000) };
//! let host = SdhciAdma2::new(inner, IdentityDma, &table);
//! let mut card = SdioSdmmc::new(host, make_delay());
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
mod dma;
mod host;
mod regs;

pub use dma::{ADMA2_DESC_COUNT, Adma2Buffer, Dma, DmaDir, SdhciAdma2};
pub use host::Sdhci;

use sdmmc_protocol::cmd::{Command, DataDirection};
use sdmmc_protocol::error::{Error, ErrorContext, Phase};
use sdmmc_protocol::response::Response;
use sdmmc_protocol::sdio::{BusWidth, ClockSpeed, SdioHost, SignalVoltage};

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
            ClockSpeed::Hs200 => 200_000_000,
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

        // External-clock mode: gate SD clock off, ask the platform CRU to
        // retune the reference clock, then bring SD clock back up at 1:1.
        if let Some(cb) = self.ext_clock {
            self.disable_sd_clock();
            cb(target_hz)?;
            return self.enable_clock_external();
        }

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
        // Plain PIO: never set the DMA bit in the transfer-mode register.
        self.use_dma = false;
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

    fn switch_voltage(&mut self, voltage: SignalVoltage) -> Result<(), Error> {
        // 1. Stop the SD clock so we don't drive the bus during the
        //    transition. Spec calls for ≥ 5 ms here; the controller's
        //    `1.8V Signaling Enable` bit toggles the IO domain
        //    immediately, so the wait is a soft requirement enforced by
        //    the platform delay (we don't have one here — bring-up code
        //    on the caller side should add one if needed).
        self.disable_sd_clock();

        // 2. Flip the voltage selector. 1.2 V isn't part of the SDHCI
        //    standard register — surface as Unsupported so the protocol
        //    layer falls back instead of silently doing the wrong thing.
        let mut ctrl2 = self.read_u16(REG_HOST_CONTROL2);
        match voltage {
            SignalVoltage::V330 => {
                ctrl2 &= !HOST_CTRL2_1V8_SIGNALING;
                self.set_power(POWER_330);
            }
            SignalVoltage::V180 => {
                ctrl2 |= HOST_CTRL2_1V8_SIGNALING;
                self.set_power(POWER_180);
            }
            SignalVoltage::V120 => return Err(Error::UnsupportedCommand),
        }
        self.write_u16(REG_HOST_CONTROL2, ctrl2);

        // 3. Bring the SD clock back on. The protocol layer's next
        //    `set_clock` call will pick the appropriate divider for
        //    whatever speed mode we're transitioning into.
        let cur = self.read_u16(REG_CLOCK_CONTROL);
        self.write_u16(REG_CLOCK_CONTROL, cur | CLOCK_SD_ENABLE);

        // 4. Sanity check: when entering 1.8 V the spec requires
        //    DAT[3:0] to be high after the switch (PRESENT_STATE bits
        //    20..23). We don't enforce this in the MVP because some
        //    QEMU models leave the bits dangling; real hardware
        //    integrators should add the check here.
        Ok(())
    }

    fn execute_tuning(&mut self, cmd_index: u8) -> Result<(), Error> {
        // Only CMD19 (SD UHS-I) and CMD21 (eMMC HS200) make sense here.
        // Reject anything else loudly so the protocol layer doesn't
        // accidentally tune for a non-tuning command.
        if cmd_index != 19 && cmd_index != 21 {
            return Err(Error::InvalidArgument);
        }

        // Block size for the tuning data phase: SD CMD19 always 64,
        // MMC CMD21 is 64 (4-bit) or 128 (8-bit). The host doesn't
        // know the bus width here without snooping HOST_CONTROL1; we
        // read it back to pick the right size.
        let block_size: u16 =
            if cmd_index == 21 && self.read_u8(REG_HOST_CONTROL1) & HOST_CTRL1_8BIT != 0 {
                128
            } else {
                64
            };

        // Pre-program the data registers per SDHCI v3 §3.7.7. The
        // controller issues the tuning command itself; we just hand it
        // the shape of the data phase.
        self.write_u16(REG_BLOCK_SIZE, block_size & 0x0FFF);
        self.write_u16(REG_BLOCK_COUNT, 1);
        self.write_u8(REG_TIMEOUT_CONTROL, 0x0E);
        // Direction = read, single block, DMA disabled.
        self.write_u16(
            REG_TRANSFER_MODE,
            XFER_MODE_BLOCK_COUNT_ENABLE | XFER_MODE_READ,
        );

        // 1. Set the Execute Tuning bit. The controller takes over and
        //    issues the tuning command repeatedly while sweeping its
        //    sampling clock; software just polls the bit until it
        //    self-clears, then checks Sampling Clock Select to know
        //    whether the sweep landed on a stable phase.
        let mut ctrl2 = self.read_u16(REG_HOST_CONTROL2);
        ctrl2 |= HOST_CTRL2_EXECUTE_TUNING;
        self.write_u16(REG_HOST_CONTROL2, ctrl2);

        // SDHCI spec caps the loop at 40 iterations × 5 ms each — a
        // worst case of 200 ms. We pick a conservative poll budget
        // around that.
        const TUNING_POLLS: u32 = 1_000_000;
        let mut last_status = 0u16;
        for _ in 0..TUNING_POLLS {
            last_status = self.read_u16(REG_HOST_CONTROL2);
            if last_status & HOST_CTRL2_EXECUTE_TUNING == 0 {
                // Controller's done. Sampling Clock Select tells us
                // whether the sweep produced a usable phase.
                if last_status & HOST_CTRL2_SAMPLING_CLOCK_SELECT != 0 {
                    return Ok(());
                }
                return Err(Error::BadResponse(ErrorContext::for_cmd(
                    Phase::Init,
                    cmd_index,
                )));
            }
            core::hint::spin_loop();
        }

        // Tuning didn't converge in our poll budget. Clear the bit so
        // the next attempt starts clean, and surface a timeout.
        let cleared = last_status & !HOST_CTRL2_EXECUTE_TUNING;
        self.write_u16(REG_HOST_CONTROL2, cleared);
        Err(Error::Timeout(ErrorContext::for_cmd(
            Phase::Init,
            cmd_index,
        )))
    }
}

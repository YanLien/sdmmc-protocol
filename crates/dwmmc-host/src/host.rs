//! `DwMmc`: register-level driver core for the Synopsys DesignWare
//! Mobile Storage Host Controller.
//!
//! This module owns the register block and implements reset, clock
//! programming, FIFO threshold setup, and bus-width selection. Higher-
//! level command issue lives in [`crate::command`]; PIO data transfer
//! lives in [`crate::data`]; the [`SdioHost`] wiring lives in
//! [`crate::lib`].
//!
//! [`SdioHost`]: sdmmc_protocol::sdio::SdioHost

use core::ptr::NonNull;

use sdmmc_protocol::error::{Error, ErrorContext, Phase};
use volatile::VolatilePtr;

use crate::regs::{
    BlkSiz, CType, ClkDiv, ClkEna, Cmd, RIntSts, RegisterBlock, RegisterBlockVolatileFieldAccess,
};

/// Default FIFO offset used by Rockchip DWC_mobile_storage variants
/// (RK3399, RK356x, RK35xx). Other SoCs may differ — pass a custom
/// offset to [`DwMmc::new_with_fifo_offset`].
pub const DEFAULT_FIFO_OFFSET: usize = 0x200;

/// Cached state for a pending data phase.
#[derive(Clone, Copy)]
pub(crate) struct PendingData {
    pub direction: sdmmc_protocol::DataDirection,
    pub block_size: u32,
    pub block_count: u32,
}

/// DesignWare Mobile Storage Host Controller backend.
///
/// Implements [`sdmmc_protocol::sdio::SdioHost`] using PIO over the
/// controller's data FIFO. ADMA / IDMAC paths are explicitly disabled
/// — internal DMAC and DMA-enable bits in the control register are
/// cleared during [`DwMmc::reset_and_init`].
///
/// # Safety
///
/// [`DwMmc::new`] is `unsafe` because the caller must hand over a
/// valid, exclusively-owned MMIO base for a DW_mshc-compatible
/// register block. Concurrent access to the same controller from
/// multiple `DwMmc` instances is undefined.
pub struct DwMmc {
    pub(crate) regs: VolatilePtr<'static, RegisterBlock>,
    pub(crate) base_addr: usize,
    pub(crate) fifo_offset: usize,
    pub(crate) ref_clock_hz: u32,
    pub(crate) pending_data: Option<PendingData>,
}

impl DwMmc {
    /// Construct a `DwMmc` over the given MMIO base, using the default
    /// FIFO offset (`0x200`).
    ///
    /// # Safety
    ///
    /// `base_addr` must point to a memory-mapped DW_mshc register file
    /// the caller has exclusive access to.
    pub unsafe fn new(base_addr: usize) -> Self {
        unsafe { Self::new_with_fifo_offset(base_addr, DEFAULT_FIFO_OFFSET) }
    }

    /// Construct a `DwMmc` with an explicit FIFO offset.
    ///
    /// Use this when porting to an SoC whose FIFO sits at a different
    /// offset than the default `0x200` (e.g. older Allwinner variants
    /// at `0x100`).
    ///
    /// # Safety
    ///
    /// Same contract as [`DwMmc::new`]; `fifo_offset` must match the
    /// hardware.
    pub unsafe fn new_with_fifo_offset(base_addr: usize, fifo_offset: usize) -> Self {
        let regs = unsafe { VolatilePtr::new(NonNull::new_unchecked(base_addr as *mut _)) };
        Self {
            regs,
            base_addr,
            fifo_offset,
            ref_clock_hz: 0,
            pending_data: None,
        }
    }

    /// Tell the driver the reference clock fed to the controller, in Hz.
    ///
    /// The clock divider in [`set_clock`](sdmmc_protocol::sdio::SdioHost::set_clock)
    /// is computed from this value: `divider = ceil(ref_clock_hz /
    /// (2 * target_hz))`. If the reference is left at `0` the driver
    /// falls back to a 1:1 passthrough (CLKDIV = 0) and assumes the
    /// platform CRU is doing all the rate scaling.
    pub fn set_reference_clock(&mut self, ref_clock_hz: u32) {
        self.ref_clock_hz = ref_clock_hz;
    }

    /// Bring the controller to a known state and arm it for card
    /// identification at 400 kHz.
    ///
    /// Call this once after construction. Performs:
    ///
    /// 1. Disable the SD clock and IDMAC paths so subsequent register
    ///    writes can't be misinterpreted by an in-flight transfer.
    /// 2. Issue a controller / FIFO / DMA reset and wait for the bits
    ///    to self-clear.
    /// 3. Mask all interrupts (we poll RINTSTS), and clear any pending
    ///    raw interrupt bits.
    /// 4. Program a low-speed clock divider suitable for ID mode and
    ///    enable the bus clock.
    pub fn reset_and_init(&mut self) -> Result<(), Error> {
        // Disable the bus clock during reset. Skip update-clock here —
        // the controller-reset below will gate everything anyway.
        self.regs.clkena().write(ClkEna::new());

        // Disable internal DMAC / DMA path: this driver is PIO-only.
        self.regs.ctrl().update(|r| {
            r.with_use_internal_dmac(false)
                .with_dma_enable(false)
                .with_int_enable(false)
        });

        // Reset CIU + FIFO + DMA. These bits self-clear on completion.
        self.regs.ctrl().update(|r| {
            r.with_controller_reset(true)
                .with_fifo_reset(true)
                .with_dma_reset(true)
        });
        self.wait_reset_clear()?;

        // Mask every interrupt; clear any leftover raw status.
        self.regs.intmask().write(0);
        self.clear_all_int_status();

        // Default to 1-bit bus until the protocol layer asks for wider.
        self.regs.ctype().write(CType::new());

        // Program the divider for 400 kHz (the SD spec ID-mode rate).
        self.program_clock(400_000)?;

        Ok(())
    }

    /// Wait for [`Ctrl::controller_reset`] / [`Ctrl::fifo_reset`] /
    /// [`Ctrl::dma_reset`] to all clear, indicating the reset finished.
    fn wait_reset_clear(&self) -> Result<(), Error> {
        for _ in 0..1_000_000 {
            let c = self.regs.ctrl().read();
            if !c.controller_reset() && !c.fifo_reset() && !c.dma_reset() {
                return Ok(());
            }
            core::hint::spin_loop();
        }
        Err(Error::Timeout(ErrorContext::new(Phase::Init)))
    }

    /// Re-program the bus clock to roughly `target_hz`.
    ///
    /// The DW_mshc clock path requires:
    ///   1. Disable CCLK_ENABLE and push the change with an
    ///      `update_clock_registers_only` command.
    ///   2. Write the new CLKDIV.
    ///   3. Push the divider change with another update-only command.
    ///   4. Re-enable CCLK_ENABLE and push it once more.
    ///
    /// Writing the CMD register without `start_cmd = 1` does
    /// nothing on this controller — start_cmd is what hands control
    /// to the CIU, even for a no-op clock-update sequence.
    pub fn program_clock(&mut self, target_hz: u32) -> Result<(), Error> {
        // 1. Gate the bus clock.
        self.regs.clkena().write(ClkEna::new());
        self.send_update_clock()?;

        // 2. Compute a divider. CLKDIV value `n` divides the
        //    reference by `2 * n` (n = 0 means bypass / 1:1).
        let div: u8 = if self.ref_clock_hz == 0 || target_hz == 0 {
            0
        } else {
            let raw = self.ref_clock_hz.div_ceil(2 * target_hz);
            // Saturate: divider field is 8 bits, max 0xFF.
            raw.min(0xFF) as u8
        };
        self.regs
            .clkdiv()
            .write(ClkDiv::new().with_clk_divider0(div));
        self.send_update_clock()?;

        // 3. Re-enable the bus clock for card 0. Bit 0 in
        //    `cclk_enable` controls card 0 — that's the only slot
        //    we drive in this MVP.
        self.regs.clkena().write(ClkEna::new().with_cclk_enable(1));
        self.send_update_clock()?;

        Ok(())
    }

    /// Issue a "no command, just push clock-related register changes
    /// to the CIU" sequence. Polls the [`Cmd::start_cmd`] bit until
    /// the controller acks the update.
    fn send_update_clock(&self) -> Result<(), Error> {
        // wait_prvdata_complete=true so we don't preempt an in-flight
        // data phase — same default we use for real commands.
        self.regs.cmd().write(
            Cmd::new()
                .with_start_cmd(true)
                .with_wait_prvdata_complete(true)
                .with_update_clock_registers_only(true),
        );
        for _ in 0..1_000_000 {
            if !self.regs.cmd().read().start_cmd() {
                return Ok(());
            }
            core::hint::spin_loop();
        }
        Err(Error::Timeout(ErrorContext::new(Phase::Init)))
    }

    /// Clear every bit in RINTSTS by writing it back (write-1-to-clear).
    pub(crate) fn clear_all_int_status(&self) {
        let cur = self.regs.rintsts().read();
        self.regs.rintsts().write(cur);
    }

    /// Set bus width. DW_mshc encodes width in CTYPE: bit 0 of `width4`
    /// = 4-bit, bit 0 of `width8` = 8-bit; both clear = 1-bit.
    pub(crate) fn set_card_type(&mut self, width: sdmmc_protocol::sdio::BusWidth) {
        use sdmmc_protocol::sdio::BusWidth;
        let ct = match width {
            BusWidth::Bit1 => CType::new(),
            BusWidth::Bit4 => CType::new().with_width4(1),
            BusWidth::Bit8 => CType::new().with_width8(1),
        };
        self.regs.ctype().write(ct);
    }

    /// Program block size + total byte count for the next data phase.
    pub(crate) fn program_data_phase(&self, block_size: u32, block_count: u32) {
        self.regs
            .blksiz()
            .write(BlkSiz::new().with_block_size(block_size as u16));
        self.regs.bytcnt().write(block_size * block_count);
    }

    /// Reset just the FIFO pointers. Useful after a data-phase error
    /// so the next transfer starts from a clean state.
    pub fn reset_fifo(&self) -> Result<(), Error> {
        self.regs.ctrl().update(|r| r.with_fifo_reset(true));
        for _ in 0..1_000_000 {
            if !self.regs.ctrl().read().fifo_reset() {
                return Ok(());
            }
            core::hint::spin_loop();
        }
        Err(Error::Timeout(ErrorContext::new(Phase::DataRead)))
    }

    /// Translate a non-zero `RIntSts.error()` into our protocol error
    /// type. `phase` and `cmd_index` give the caller's pipeline
    /// context.
    pub(crate) fn translate_int_error(&self, ints: RIntSts, phase: Phase, cmd_index: u8) -> Error {
        let ctx = ErrorContext::for_cmd(phase, cmd_index);
        if ints.response_timeout() || ints.data_read_timeout() || ints.host_timeout() {
            Error::Timeout(ctx)
        } else if ints.response_crc_error() || ints.data_crc_error() {
            Error::Crc(ctx)
        } else if ints.response_error() {
            Error::BadResponse(ctx)
        } else if matches!(phase, Phase::DataRead) {
            Error::ReadError(ctx)
        } else if matches!(phase, Phase::DataWrite) {
            Error::WriteError(ctx)
        } else {
            Error::BusError(ctx)
        }
    }

    /// Raw pointer at `base + fifo_offset`, used for 64-bit FIFO
    /// accesses in [`crate::data`].
    pub(crate) fn fifo_ptr(&self) -> *mut u64 {
        (self.base_addr + self.fifo_offset) as *mut u64
    }
}

unsafe impl Send for DwMmc {}
unsafe impl Sync for DwMmc {}

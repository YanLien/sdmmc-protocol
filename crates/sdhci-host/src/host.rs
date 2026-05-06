//! `Sdhci` core: MMIO accessors, reset, clock and bus-width setup.

use sdmmc_protocol::error::{Error, ErrorContext, Phase};

use crate::regs::*;

/// Cached state for a single pending data phase, populated by
/// `prepare_data_transfer` and consumed by the next `send_command` call.
#[derive(Clone, Copy)]
pub(crate) struct PendingData {
    pub direction: sdmmc_protocol::DataDirection,
    pub block_size: u32,
    pub block_count: u32,
}

/// Generic SD Host Controller (SDHCI) backend.
///
/// Owns the MMIO base address of one host controller instance and
/// implements [`sdmmc_protocol::sdio::SdioHost`] so that the protocol
/// driver in `sdmmc-protocol` can drive it. PIO data transfer only.
///
/// # Safety
///
/// `new` is `unsafe` because the caller must provide a valid, exclusive
/// MMIO base address for an SDHCI v3.x compatible controller. Concurrent
/// use of the same controller from multiple `Sdhci` instances is undefined.
pub struct Sdhci {
    base_addr: usize,
    pub(crate) pending_data: Option<PendingData>,
}

impl Sdhci {
    /// Construct a new Sdhci over the given MMIO base address.
    ///
    /// # Safety
    ///
    /// `base_addr` must point to a memory-mapped SDHCI v3.x register file
    /// that the caller has exclusive access to.
    pub unsafe fn new(base_addr: usize) -> Self {
        Self {
            base_addr,
            pending_data: None,
        }
    }

    /// Reset the controller (CMD line + DAT line + state) by writing the
    /// "Reset All" bit and waiting for it to clear.
    pub fn reset_all(&mut self) -> Result<(), Error> {
        self.reset_with_mask(RESET_ALL, Phase::Init)
    }

    /// Reset the CMD line state machine (clears any stuck CMD inhibit).
    pub fn reset_cmd(&mut self) -> Result<(), Error> {
        self.reset_with_mask(RESET_CMD, Phase::CommandSend)
    }

    /// Reset the DAT line state machine.
    pub fn reset_dat(&mut self) -> Result<(), Error> {
        self.reset_with_mask(RESET_DAT, Phase::DataRead)
    }

    fn reset_with_mask(&mut self, mask: u8, phase: Phase) -> Result<(), Error> {
        self.write_u8(REG_SOFTWARE_RESET, mask);
        for _ in 0..1000 {
            if self.read_u8(REG_SOFTWARE_RESET) & mask == 0 {
                return Ok(());
            }
            spin_loop();
        }
        Err(Error::Timeout(ErrorContext::new(phase)))
    }

    /// Bring the internal clock up. `base_clock_hz` is the controller's
    /// reference clock (read from Capabilities or supplied externally) and
    /// `target_hz` is the desired SD bus frequency.
    ///
    /// Uses the SDHCI v3.0 10-bit divided clock mode.
    pub fn enable_clock(&mut self, base_clock_hz: u32, target_hz: u32) -> Result<(), Error> {
        // 1. Disable SD clock so we can safely change the divider.
        self.write_u16(REG_CLOCK_CONTROL, 0);

        if target_hz == 0 {
            return Ok(());
        }

        // 2. Pick the smallest divider such that base/2N ≤ target. SDHCI
        //    v3.0 supports 10-bit divider in steps of 2 (so 2N ranges 2..1024).
        let mut div = 0u16;
        if base_clock_hz > target_hz {
            for n in 1..=0x3FF {
                if base_clock_hz / (2 * n as u32) <= target_hz {
                    div = n;
                    break;
                }
            }
        }

        // Encode divider: bits 15..8 hold low 8 bits, bits 7..6 hold the
        // upper 2 bits of the 10-bit divider for v3.0 compatible hosts.
        let clk_ctrl = ((div & 0xFF) << 8) | ((div & 0x300) >> 2) | CLOCK_INTERNAL_ENABLE;
        self.write_u16(REG_CLOCK_CONTROL, clk_ctrl);

        // 3. Wait for internal clock to stabilize.
        for _ in 0..1000 {
            if self.read_u16(REG_CLOCK_CONTROL) & CLOCK_INTERNAL_STABLE != 0 {
                let stable = self.read_u16(REG_CLOCK_CONTROL) | CLOCK_SD_ENABLE;
                self.write_u16(REG_CLOCK_CONTROL, stable);
                return Ok(());
            }
            spin_loop();
        }
        Err(Error::Timeout(ErrorContext::new(Phase::Init)))
    }

    /// Set bus power (e.g. 3.3 V) and the global power-on bit.
    pub fn set_power(&mut self, power_byte: u8) {
        self.write_u8(REG_POWER_CONTROL, power_byte | POWER_ON);
    }

    /// Enable normal + error interrupt status flags so command/data
    /// completion is observable via the status registers (signal-level
    /// IRQ delivery is NOT enabled — the driver polls).
    pub fn enable_interrupts(&mut self) {
        self.write_u16(REG_NORMAL_INT_STATUS_ENABLE, NORMAL_INT_CLEAR_ALL);
        self.write_u16(REG_ERROR_INT_STATUS_ENABLE, ERROR_INT_CLEAR_ALL);
        // Don't route to host CPU IRQ — leave Signal Enable cleared.
        self.write_u16(REG_NORMAL_INT_SIGNAL_ENABLE, 0);
        self.write_u16(REG_ERROR_INT_SIGNAL_ENABLE, 0);
    }

    /// Read the controller's base reference clock from Capabilities (Hz).
    pub fn base_clock_hz(&self) -> u32 {
        let caps_low = self.read_u32(REG_CAPABILITIES_LOW);
        // SDHCI v3: bits 15..8 contain "Base Clock Frequency" in MHz.
        // SDHCI v2: bits 13..8 contain it. Use the wider mask; QEMU
        // sdhci-pci reports a v2 layout but the result is still right.
        let mhz = (caps_low >> 8) & 0xFF;
        mhz.saturating_mul(1_000_000)
    }

    /// Read raw 32-bit response slot.
    pub(crate) fn response32(&self, slot: usize) -> u32 {
        let off = REG_RESPONSE0 + slot * 4;
        self.read_u32(off)
    }

    pub(crate) fn read_u32(&self, off: usize) -> u32 {
        unsafe { core::ptr::read_volatile((self.base_addr + off) as *const u32) }
    }

    pub(crate) fn write_u32(&self, off: usize, val: u32) {
        unsafe { core::ptr::write_volatile((self.base_addr + off) as *mut u32, val) }
    }

    pub(crate) fn read_u16(&self, off: usize) -> u16 {
        unsafe { core::ptr::read_volatile((self.base_addr + off) as *const u16) }
    }

    pub(crate) fn write_u16(&self, off: usize, val: u16) {
        unsafe { core::ptr::write_volatile((self.base_addr + off) as *mut u16, val) }
    }

    pub(crate) fn read_u8(&self, off: usize) -> u8 {
        unsafe { core::ptr::read_volatile((self.base_addr + off) as *const u8) }
    }

    pub(crate) fn write_u8(&self, off: usize, val: u8) {
        unsafe { core::ptr::write_volatile((self.base_addr + off) as *mut u8, val) }
    }
}

#[inline]
fn spin_loop() {
    core::hint::spin_loop();
}

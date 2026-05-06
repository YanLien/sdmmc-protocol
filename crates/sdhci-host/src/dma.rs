//! DMA glue for the SDHCI ADMA2 data path.
//!
//! The crate is `no_std` and refuses to assume an allocator, an MMU layout,
//! or a particular cache architecture. Callers wire those concerns up via
//! the [`Dma`] trait and a caller-owned [`Adma2Buffer`] descriptor scratch
//! region.
//!
//! ## Responsibilities split
//!
//! - **The host driver** builds the ADMA2 descriptor table inside the
//!   buffer the caller hands it, programs the controller, and waits on the
//!   transfer-complete IRQ.
//! - **The [`Dma`] impl** translates kernel/CPU pointers to the bus
//!   addresses the SDHCI sees, and performs whatever cache maintenance is
//!   needed before the device reads CPU-written memory and after the
//!   device writes CPU-read memory.
//!
//! That split keeps the SDHCI logic portable across hosted Linux (where
//! `Dma::map_*` typically calls `dma_map_single`), bare-metal coherent
//! systems (identity mapping, no cache ops), and bare-metal incoherent
//! systems (identity mapping + dcache flush/invalidate).

use core::cell::RefCell;

use sdmmc_protocol::cmd::{Command, DataDirection};
use sdmmc_protocol::error::{Error, ErrorContext, Phase};
use sdmmc_protocol::response::Response;
use sdmmc_protocol::sdio::{BusWidth, ClockSpeed, SdioHost};

use crate::host::{PendingData, Sdhci};

/// Direction of an outstanding DMA mapping. Mirrors `DataDirection` so the
/// host can hand it through to the [`Dma`] implementation without dragging
/// the protocol enum into platform code.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DmaDir {
    /// Memory will be filled by the device (card → host).
    FromDevice,
    /// Memory will be read by the device (host → card).
    ToDevice,
}

impl From<DataDirection> for DmaDir {
    fn from(d: DataDirection) -> Self {
        match d {
            DataDirection::Read => DmaDir::FromDevice,
            // `None` cannot reach the DMA path — `prepare_data_transfer`
            // gates that. Conservative default keeps the conversion total.
            DataDirection::Write | DataDirection::None => DmaDir::ToDevice,
        }
    }
}

/// Platform-supplied DMA mapping interface.
///
/// Implementations are expected to be cheap and side-effect-free except for
/// the cache maintenance noted on each method.
pub trait Dma {
    /// Translate a CPU virtual address inside `buf` to the bus address the
    /// SDHCI controller will see. On systems with an identity DMA mapping
    /// this is just `buf.as_ptr() as u64`; on systems with an IOMMU or a
    /// DMA offset it is whatever the platform's mapping layer returns.
    ///
    /// `dir` is provided so that platforms backed by `dma_map_single`-style
    /// APIs can call the right map flavour. Implementations that do not
    /// pin the mapping per-call should ignore it.
    fn map(&self, buf: *const u8, len: usize, dir: DmaDir) -> u64;

    /// Cache-maintenance hook called *before* the device reads
    /// host-written memory (writes) or *before* the device fills
    /// host-readable memory (reads).
    ///
    /// On coherent systems this is a no-op. On incoherent ARM/AArch64 this
    /// is typically `dcache clean` for `ToDevice` and `dcache invalidate`
    /// for `FromDevice`.
    fn before_dma(&self, buf: *const u8, len: usize, dir: DmaDir);

    /// Cache-maintenance hook called *after* the transfer completes.
    ///
    /// On coherent systems this is a no-op. On incoherent systems this is
    /// typically `dcache invalidate` after a `FromDevice` transfer so the
    /// CPU sees the device's writes.
    fn after_dma(&self, buf: *const u8, len: usize, dir: DmaDir);
}

/// 32-bit ADMA2 descriptor.
///
/// Layout (little-endian, per SDHCI v3.00 §1.13):
///
/// ```text
///   0      attr[15:0]   (Valid | End | Int | Act2 | Act1)
///   2      length[15:0] (0 means 64 KiB)
///   4      address[31:0]
/// ```
#[repr(C, align(4))]
#[derive(Clone, Copy, Default)]
pub(crate) struct Adma2Desc32 {
    attr: u16,
    length: u16,
    address: u32,
}

const ADMA2_ATTR_VALID: u16 = 1 << 0;
const ADMA2_ATTR_END: u16 = 1 << 1;
const _ADMA2_ATTR_INT: u16 = 1 << 2;
// act = 0b10 → "tran" (data transfer descriptor)
const ADMA2_ATTR_ACT_TRAN: u16 = 0b10 << 4;

/// Largest single ADMA2 transfer — the length field is 16 bits and `0`
/// is interpreted as 64 KiB, but we cap a hair below to keep the math
/// trivial and to leave room for hosts whose ADMA engine refuses
/// `length == 0` (some Synopsys MSHC variants).
const ADMA2_MAX_PER_DESC: usize = 65_528; // 64 KiB - 8B, multiple of 8

/// Caller-owned scratch region for the ADMA2 descriptor table.
///
/// Sized for a worst-case 64 KiB transfer split into 4 KiB chunks (16
/// descriptors), which is the SDMA boundary the controller falls back to
/// on page boundary crossings. Bumping this constant is the only thing
/// needed to support larger contiguous transfers.
pub const ADMA2_DESC_COUNT: usize = 16;

/// Storage for the ADMA2 descriptor table. Allocate one of these once per
/// host instance and hand it to [`SdhciAdma2::new`]. The buffer itself is
/// what the controller DMA-reads, so it must live in DMA-capable memory
/// and the [`Dma`] impl must produce a sensible bus address for it.
#[repr(C, align(64))]
pub struct Adma2Buffer {
    pub(crate) descs: RefCell<[Adma2Desc32; ADMA2_DESC_COUNT]>,
}

impl Adma2Buffer {
    pub const fn new() -> Self {
        Self {
            descs: RefCell::new(
                [Adma2Desc32 {
                    attr: 0,
                    length: 0,
                    address: 0,
                }; ADMA2_DESC_COUNT],
            ),
        }
    }
}

impl Default for Adma2Buffer {
    fn default() -> Self {
        Self::new()
    }
}

/// Build the ADMA2 descriptor table covering `[base, base+total_len)`.
///
/// `base` is the *bus* address the controller will use, already translated
/// by [`Dma::map`]. Returns the number of descriptors written or
/// [`Error::Misaligned`] if the buffer would not fit in
/// [`ADMA2_DESC_COUNT`] entries.
pub(crate) fn build_descriptors(
    table: &mut [Adma2Desc32; ADMA2_DESC_COUNT],
    base: u64,
    total_len: usize,
    phase: Phase,
) -> Result<usize, Error> {
    if total_len == 0 {
        return Err(Error::Misaligned);
    }
    if base >> 32 != 0 {
        // 32-bit ADMA2 only addresses the low 4 GiB. 64-bit ADMA2 needs a
        // different descriptor layout we don't ship yet — surface it as a
        // capability mismatch rather than truncating silently.
        return Err(Error::BadResponse(ErrorContext::new(phase)));
    }

    let mut remaining = total_len;
    let mut offset: u64 = 0;
    let mut written = 0usize;

    while remaining > 0 {
        if written >= ADMA2_DESC_COUNT {
            return Err(Error::Misaligned);
        }
        let chunk = remaining.min(ADMA2_MAX_PER_DESC);
        let is_last = chunk == remaining;
        let mut attr = ADMA2_ATTR_VALID | ADMA2_ATTR_ACT_TRAN;
        if is_last {
            attr |= ADMA2_ATTR_END;
        }
        table[written] = Adma2Desc32 {
            attr,
            length: chunk as u16,
            address: (base + offset) as u32,
        };
        written += 1;
        offset += chunk as u64;
        remaining -= chunk;
    }

    Ok(written)
}

/// SDHCI host backend that funnels data through ADMA2 instead of PIO.
///
/// `SdhciAdma2` borrows the underlying [`Sdhci`] for command issue, reset,
/// and clocking, but overrides the data-phase methods to program the
/// ADMA descriptor pointer and let the controller's DMA engine move the
/// payload. Callers supply a [`Dma`] implementation that translates CPU
/// pointers to bus addresses and performs the platform-specific cache
/// maintenance.
///
/// # Lifetime
///
/// The descriptor scratch lives in [`Adma2Buffer`] and is borrowed for the
/// life of the wrapper. Allocate one per controller alongside the
/// `Sdhci` itself; do not share it between controllers.
///
/// # Constraints
///
/// - 32-bit ADMA2 only. Bus addresses must fit in 32 bits.
/// - Buffers handed to [`SdioHost::read_data`] / [`SdioHost::write_data`]
///   must be naturally aligned to at least 4 bytes (the ADMA2 spec
///   requires the source/destination to be 32-bit aligned). Misaligned
///   buffers return [`Error::Misaligned`] without touching the hardware.
/// - The data length must equal `block_size * block_count`, which the
///   protocol layer already guarantees.
pub struct SdhciAdma2<'buf, D: Dma> {
    inner: Sdhci,
    dma: D,
    table: &'buf Adma2Buffer,
}

impl<'buf, D: Dma> SdhciAdma2<'buf, D> {
    /// Wrap an existing [`Sdhci`] in an ADMA2 data path.
    ///
    /// The caller is responsible for having reset and clocked the
    /// controller before any commands flow; reusing the [`Sdhci`] API for
    /// that is fine.
    pub fn new(inner: Sdhci, dma: D, table: &'buf Adma2Buffer) -> Self {
        Self { inner, dma, table }
    }

    /// Borrow the underlying PIO controller. Useful for one-off setup
    /// (e.g. `enable_clock`, `set_power`) where the DMA layer is
    /// irrelevant.
    pub fn raw(&mut self) -> &mut Sdhci {
        &mut self.inner
    }

    fn run_dma(&mut self, ptr: *const u8, len: usize, dir: DmaDir) -> Result<(), Error> {
        // 1. Translate + cache-maintain the payload buffer.
        let bus_addr = self.dma.map(ptr, len, dir);
        self.dma.before_dma(ptr, len, dir);

        // 2. Build the descriptor table and translate its address too.
        let descs_ptr = {
            let mut descs = self.table.descs.borrow_mut();
            let _written = build_descriptors(
                &mut descs,
                bus_addr,
                len,
                match dir {
                    DmaDir::FromDevice => Phase::DataRead,
                    DmaDir::ToDevice => Phase::DataWrite,
                },
            )?;
            descs.as_ptr() as *const u8
        };
        // Descriptor table itself is host-written, device-read.
        let desc_len = core::mem::size_of::<Adma2Desc32>() * ADMA2_DESC_COUNT;
        let desc_bus = self.dma.map(descs_ptr, desc_len, DmaDir::ToDevice);
        self.dma.before_dma(descs_ptr, desc_len, DmaDir::ToDevice);

        if desc_bus >> 32 != 0 {
            return Err(Error::BadResponse(ErrorContext::new(Phase::DataRead)));
        }

        // 3. Program the controller. `use_dma` was already set on the
        //    inner Sdhci by `prepare_data_transfer`, so the upcoming
        //    `issue_command` call will set TRANSFER_MODE.DMA_ENABLE.
        self.inner.select_adma2_32();
        self.inner.write_adma_addr(desc_bus as u32);
        Ok(())
    }
}

/// Last command index issued; tagged onto error contexts. We don't
/// snoop the actual command index here because `issue_command` is
/// already done by the time the DMA engine fires.
const DMA_PHASE_CMD: u8 = 0;

impl<'buf, D: Dma> SdioHost for SdhciAdma2<'buf, D> {
    fn send_command(&mut self, cmd: &Command) -> Result<Response, Error> {
        self.inner.issue_command(cmd)
    }

    fn read_data(&mut self, buf: &mut [u8], block_size: u32) -> Result<(), Error> {
        if block_size == 0 || (buf.len() as u32) % block_size != 0 {
            return Err(Error::Misaligned);
        }
        if buf.as_ptr() as usize & 0x3 != 0 {
            return Err(Error::Misaligned);
        }
        let len = buf.len();
        let ptr = buf.as_mut_ptr();
        self.run_dma(ptr, len, DmaDir::FromDevice)?;

        // The command was already issued and DMA started. Wait on the
        // controller's transfer-complete IRQ status flag (or an ADMA
        // error).
        self.inner
            .wait_data_complete_with_adma(DMA_PHASE_CMD, Phase::DataRead)?;

        // Flush/invalidate cache so the CPU sees what the device wrote.
        self.dma.after_dma(ptr, len, DmaDir::FromDevice);
        Ok(())
    }

    fn write_data(&mut self, buf: &[u8], block_size: u32) -> Result<(), Error> {
        if block_size == 0 || (buf.len() as u32) % block_size != 0 {
            return Err(Error::Misaligned);
        }
        if buf.as_ptr() as usize & 0x3 != 0 {
            return Err(Error::Misaligned);
        }
        let len = buf.len();
        let ptr = buf.as_ptr();
        self.run_dma(ptr, len, DmaDir::ToDevice)?;
        self.inner
            .wait_data_complete_with_adma(DMA_PHASE_CMD, Phase::DataWrite)?;
        self.dma.after_dma(ptr, len, DmaDir::ToDevice);
        Ok(())
    }

    fn set_bus_width(&mut self, width: BusWidth) -> Result<(), Error> {
        self.inner.set_bus_width(width)
    }

    fn set_clock(&mut self, speed: ClockSpeed) -> Result<(), Error> {
        self.inner.set_clock(speed)
    }

    fn set_block_count(&mut self, count: u32) -> Result<(), Error> {
        self.inner.set_block_count(count)
    }

    fn prepare_data_transfer(
        &mut self,
        direction: DataDirection,
        block_size: u32,
        block_count: u32,
    ) -> Result<(), Error> {
        // Same bookkeeping as the PIO impl, plus the DMA-mode flag so the
        // next issue_command flips TRANSFER_MODE.DMA_ENABLE.
        if direction.is_none() {
            self.inner.pending_data = None;
            self.inner.use_dma = false;
        } else {
            self.inner.pending_data = Some(PendingData {
                direction,
                block_size,
                block_count,
            });
            self.inner.use_dma = true;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn empty_table() -> [Adma2Desc32; ADMA2_DESC_COUNT] {
        [Adma2Desc32 {
            attr: 0,
            length: 0,
            address: 0,
        }; ADMA2_DESC_COUNT]
    }

    #[test]
    fn single_descriptor_for_small_buffer() {
        let mut table = empty_table();
        let n = build_descriptors(&mut table, 0x1000_0000, 512, Phase::DataRead).unwrap();
        assert_eq!(n, 1);
        assert_eq!(table[0].length, 512);
        assert_eq!(table[0].address, 0x1000_0000);
        // Valid + End + Tran action
        assert_eq!(
            table[0].attr,
            ADMA2_ATTR_VALID | ADMA2_ATTR_END | ADMA2_ATTR_ACT_TRAN
        );
    }

    #[test]
    fn splits_across_max_chunk() {
        let mut table = empty_table();
        let total = ADMA2_MAX_PER_DESC + 4096;
        let n = build_descriptors(&mut table, 0x2000_0000, total, Phase::DataRead).unwrap();
        assert_eq!(n, 2);
        assert_eq!(table[0].length as usize, ADMA2_MAX_PER_DESC);
        // first descriptor must NOT have END
        assert!(table[0].attr & ADMA2_ATTR_END == 0);
        // second descriptor covers the tail and has END
        assert_eq!(table[1].length, 4096);
        assert!(table[1].attr & ADMA2_ATTR_END != 0);
        assert_eq!(table[1].address, 0x2000_0000 + ADMA2_MAX_PER_DESC as u32);
    }

    #[test]
    fn rejects_64bit_bus_address() {
        let mut table = empty_table();
        let err =
            build_descriptors(&mut table, 0x1_0000_0000, 512, Phase::DataRead).unwrap_err();
        assert!(matches!(err, Error::BadResponse(_)));
    }

    #[test]
    fn rejects_zero_length() {
        let mut table = empty_table();
        let err = build_descriptors(&mut table, 0, 0, Phase::DataRead).unwrap_err();
        assert!(matches!(err, Error::Misaligned));
    }
}

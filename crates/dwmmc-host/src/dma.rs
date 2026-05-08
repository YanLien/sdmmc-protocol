use log::warn;
use sdmmc_protocol::{
    cmd::{CMD12, DataDirection, cmd17, cmd18},
    error::{Error, ErrorContext, Phase},
};

use crate::{
    host::{DwMmc, PendingData},
    regs::RegisterBlockVolatileFieldAccess,
};

const DESC_OWN: u32 = 1 << 31;
const DESC_CH: u32 = 1 << 4;
const DESC_FS: u32 = 1 << 3;
const DESC_LD: u32 = 1 << 2;
const DESC_DIC: u32 = 1 << 1;

const BMOD_SWR: u32 = 1 << 0;
const BMOD_FB: u32 = 1 << 1;
const BMOD_DE: u32 = 1 << 7;

const DMA_POLL_LIMIT: u32 = 8_000_000;
pub const IDMAC_DESC_ALIGN: usize = 16;
pub const IDMAC_DESC_SIZE: usize = core::mem::size_of::<IdmacDesc>();

#[repr(C, align(16))]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct IdmacDesc {
    des0: u32,
    des1: u32,
    des2: u32,
    des3: u32,
}

pub struct IdmacRead<'a, F> {
    pub buffer_dma: u64,
    pub desc_dma: u64,
    pub desc_count: usize,
    pub desc_virt: *mut u8,
    pub flush_desc: &'a mut F,
}

impl IdmacDesc {
    pub fn chained(buffer_dma: u32, len: u32, next_desc_dma: u32, first: bool, last: bool) -> Self {
        let mut des0 = DESC_OWN | DESC_CH | DESC_DIC;
        if first {
            des0 |= DESC_FS;
        }
        if last {
            des0 |= DESC_LD;
        }
        Self {
            des0,
            des1: len,
            des2: buffer_dma,
            des3: next_desc_dma,
        }
    }
}

impl DwMmc {
    pub fn dma_read_blocks<F>(
        &mut self,
        start_block: u32,
        block_count: u32,
        request: IdmacRead<'_, F>,
    ) -> Result<(), Error>
    where
        F: FnMut(*mut u8, usize),
    {
        if block_count == 0 {
            return Err(Error::InvalidArgument);
        }
        let byte_count = block_count.checked_mul(512).ok_or(Error::InvalidArgument)?;
        let transfer_end = request
            .buffer_dma
            .checked_add(byte_count as u64)
            .ok_or(Error::InvalidArgument)?;
        let desc_bytes = (block_count as usize)
            .checked_mul(IDMAC_DESC_SIZE)
            .ok_or(Error::InvalidArgument)?;
        let desc_end = request
            .desc_dma
            .checked_add(desc_bytes as u64)
            .ok_or(Error::InvalidArgument)?;
        if transfer_end > u32::MAX as u64 + 1
            || desc_end > u32::MAX as u64 + 1
            || request.desc_count < block_count as usize
            || request.desc_virt.is_null()
        {
            return Err(Error::InvalidArgument);
        }

        unsafe {
            let descs = request.desc_virt as *mut IdmacDesc;
            for index in 0..block_count as usize {
                let last = index + 1 == block_count as usize;
                let next = if last {
                    0
                } else {
                    (request.desc_dma as u32) + ((index + 1) * IDMAC_DESC_SIZE) as u32
                };
                descs.add(index).write_volatile(IdmacDesc::chained(
                    (request.buffer_dma as u32) + (index * 512) as u32,
                    512,
                    next,
                    index == 0,
                    last,
                ));
            }
        }
        (request.flush_desc)(request.desc_virt, desc_bytes);

        self.clear_all_int_status();
        self.program_data_phase(512, block_count);
        self.reset_dma()?;

        self.regs.dbaddr().write(request.desc_dma as u32);
        self.regs.ctrl().update(|r| {
            r.with_use_internal_dmac(true)
                .with_dma_enable(true)
                .with_int_enable(false)
        });
        self.regs.bmod().write(BMOD_FB | BMOD_DE);
        self.regs.pldmnd().write(1);

        self.pending_data = Some(PendingData {
            direction: DataDirection::Read,
            block_size: 512,
            block_count,
        });
        self.data_blocks_remaining = block_count;
        let cmd = if block_count == 1 {
            cmd17(start_block)
        } else {
            cmd18(start_block)
        };
        if let Err(err) = self.issue_command(&cmd) {
            self.disable_idmac();
            self.recover_after_idmac_read_error();
            self.clear_all_int_status();
            return Err(err);
        }

        let result = self.wait_dma_read_complete(cmd.cmd);
        if result.is_ok() && block_count > 1 {
            let _ = self.issue_command(&CMD12);
        }
        self.disable_idmac();
        if result.is_err() {
            self.recover_after_idmac_read_error();
        }
        self.clear_all_int_status();
        result
    }

    fn disable_idmac(&self) {
        self.regs.ctrl().update(|r| {
            r.with_use_internal_dmac(false)
                .with_dma_enable(false)
                .with_int_enable(false)
        });
        self.regs.bmod().write(0);
    }

    fn recover_after_idmac_read_error(&mut self) {
        let status = self.regs.status().read().into_bits();
        let rintsts = self.regs.rintsts().read();
        warn!(
            "dwmmc: IDMAC read error state rintsts={:#010x} status={:#010x} tcbcnt={} tbbcnt={}",
            rintsts.into_bits(),
            status,
            self.regs.tcbcnt().read(),
            self.regs.tbbcnt().read()
        );

        self.regs.ctrl().update(|r| r.with_abort_read_data(true));
        let _ = self.regs.ctrl().read();
        let _ = self.reset_fifo();
        let _ = self.reset_dma();
        self.regs.ctrl().update(|r| r.with_abort_read_data(false));
        self.pending_data = None;
        self.data_blocks_remaining = 0;
        self.data_cmd_index = 0;
    }

    fn reset_dma(&self) -> Result<(), Error> {
        self.regs.ctrl().update(|r| r.with_dma_reset(true));
        for _ in 0..DMA_POLL_LIMIT {
            if !self.regs.ctrl().read().dma_reset() {
                self.regs.bmod().write(BMOD_SWR);
                for _ in 0..DMA_POLL_LIMIT {
                    if self.regs.bmod().read() & BMOD_SWR == 0 {
                        return Ok(());
                    }
                    core::hint::spin_loop();
                }
                break;
            }
            core::hint::spin_loop();
        }
        Err(Error::Timeout(ErrorContext::new(Phase::DataRead)))
    }

    fn wait_dma_read_complete(&self, cmd_index: u8) -> Result<(), Error> {
        for _ in 0..DMA_POLL_LIMIT {
            let rintsts = self.regs.rintsts().read();
            if rintsts.error() {
                return Err(self.translate_int_error(rintsts, Phase::DataRead, cmd_index));
            }
            if rintsts.data_transfer_over() {
                return Ok(());
            }
            core::hint::spin_loop();
        }
        Err(Error::Timeout(ErrorContext::for_cmd(
            Phase::DataRead,
            cmd_index,
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_descriptor_sets_owned_chained_first_read_buffer() {
        let desc = IdmacDesc::chained(0x1234_5000, 512, 0x2000, true, false);

        assert_eq!(desc.des0, DESC_OWN | DESC_CH | DESC_FS | DESC_DIC);
        assert_eq!(desc.des1, 512);
        assert_eq!(desc.des2, 0x1234_5000);
        assert_eq!(desc.des3, 0x2000);
    }

    #[test]
    fn last_descriptor_sets_last_and_terminates_chain() {
        let desc = IdmacDesc::chained(0x1234_5200, 512, 0, false, true);

        assert_eq!(desc.des0, DESC_OWN | DESC_CH | DESC_LD | DESC_DIC);
        assert_eq!(desc.des1, 512);
        assert_eq!(desc.des2, 0x1234_5200);
        assert_eq!(desc.des3, 0);
    }
}

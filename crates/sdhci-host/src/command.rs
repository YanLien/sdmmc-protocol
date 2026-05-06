//! Command issue / response collection.
//!
//! Drives the SDHCI command pipeline: argument register → transfer-mode
//! shape (if data is present) → command register → poll the normal/error
//! interrupt status registers → harvest the response slot(s).
//!
//! All raise sites tag their phase with [`Phase::CommandSend`] /
//! [`Phase::ResponseWait`] so callers can pinpoint failures.

use sdmmc_protocol::cmd::Command;
use sdmmc_protocol::error::{Error, ErrorContext, Phase};
use sdmmc_protocol::response::{
    IfCondResponse, OcrResponse, R1Response, RcaResponse, Response, ResponseType,
};

use crate::host::Sdhci;
use crate::regs::*;

const POLL_LIMIT: u32 = 1_000_000;

impl Sdhci {
    /// Issue a single command. Caller must have populated `pending_data`
    /// (the `SdioHost::prepare_data_transfer` impl does this) when the
    /// command carries a data phase.
    pub fn issue_command(&mut self, cmd: &Command) -> Result<Response, Error> {
        let data = self.pending_data.take();

        // 1. Wait for the controller's own pipeline to drain.
        self.wait_inhibit(data.is_some(), cmd.cmd)?;

        // 2. Clear any leftover interrupt bits.
        self.write_u16(REG_NORMAL_INT_STATUS, NORMAL_INT_CLEAR_ALL);
        self.write_u16(REG_ERROR_INT_STATUS, ERROR_INT_CLEAR_ALL);

        // 3. Configure the data phase (block size + count + transfer mode).
        if let Some(d) = data {
            self.configure_data_phase(d.direction, d.block_size, d.block_count);
        } else {
            self.write_u16(REG_TRANSFER_MODE, 0);
        }

        // 4. Push the argument and command-register encoding.
        self.write_u32(REG_ARGUMENT, cmd.arg);
        let cmd_reg = encode_command(cmd, data.is_some())?;
        self.write_u16(REG_COMMAND, cmd_reg);

        // 5. Block until the response arrives (or the controller flags
        //    a CMD-line error).
        self.wait_for(NORMAL_INT_CMD_COMPLETE, ERROR_INT_CMD_LINE_MASK, cmd.cmd)?;

        // 6. Acknowledge command completion.
        self.write_u16(REG_NORMAL_INT_STATUS, NORMAL_INT_CMD_COMPLETE);

        decode_response(self, cmd.resp_type)
    }

    /// Block until the next data phase finishes (Transfer Complete) or
    /// the controller raises a DAT-line error.
    pub fn wait_data_complete(&self, cmd_index: u8) -> Result<(), Error> {
        self.wait_for(
            NORMAL_INT_XFER_COMPLETE,
            ERROR_INT_DATA_LINE_MASK,
            cmd_index,
        )?;
        self.write_u16(REG_NORMAL_INT_STATUS, NORMAL_INT_XFER_COMPLETE);
        Ok(())
    }

    fn wait_inhibit(&self, has_data: bool, cmd_index: u8) -> Result<(), Error> {
        let mask = if has_data {
            PRESENT_CMD_INHIBIT | PRESENT_DAT_INHIBIT
        } else {
            PRESENT_CMD_INHIBIT
        };
        for _ in 0..POLL_LIMIT {
            if self.read_u32(REG_PRESENT_STATE) & mask == 0 {
                return Ok(());
            }
            core::hint::spin_loop();
        }
        Err(Error::Timeout(ErrorContext::for_cmd(
            Phase::CommandSend,
            cmd_index,
        )))
    }

    fn wait_for(&self, success_mask: u16, error_mask: u16, cmd_index: u8) -> Result<(), Error> {
        for _ in 0..POLL_LIMIT {
            let status = self.read_u16(REG_NORMAL_INT_STATUS);
            if status & success_mask != 0 {
                return Ok(());
            }
            if status & NORMAL_INT_ERROR != 0 {
                return Err(self.translate_error(error_mask, cmd_index));
            }
            core::hint::spin_loop();
        }
        Err(Error::Timeout(ErrorContext::for_cmd(
            Phase::ResponseWait,
            cmd_index,
        )))
    }

    fn translate_error(&self, mask: u16, cmd_index: u8) -> Error {
        let err = self.read_u16(REG_ERROR_INT_STATUS) & mask;
        // Acknowledge so the next command starts from a clean slate.
        self.write_u16(REG_ERROR_INT_STATUS, ERROR_INT_CLEAR_ALL);
        let ctx = ErrorContext::for_cmd(Phase::ResponseWait, cmd_index);
        if err & (ERROR_INT_CMD_TIMEOUT | ERROR_INT_DATA_TIMEOUT) != 0 {
            Error::Timeout(ctx)
        } else if err & (ERROR_INT_CMD_CRC | ERROR_INT_DATA_CRC) != 0 {
            Error::Crc(ctx)
        } else if err & ERROR_INT_DATA_LINE_MASK != 0 {
            Error::ReadError(ctx)
        } else {
            Error::BadResponse(ctx)
        }
    }

    fn configure_data_phase(
        &mut self,
        direction: sdmmc_protocol::DataDirection,
        block_size: u32,
        block_count: u32,
    ) {
        // SDHCI block size register: bits 11..0 hold block length, bits
        // 14..12 hold the SDMA buffer boundary (we use 0 = 4 KiB).
        self.write_u16(REG_BLOCK_SIZE, (block_size as u16) & 0x0FFF);
        self.write_u16(REG_BLOCK_COUNT, block_count as u16);

        let mut mode = 0u16;
        if block_count > 1 {
            mode |= XFER_MODE_BLOCK_COUNT_ENABLE | XFER_MODE_MULTI_BLOCK | XFER_MODE_AUTO_CMD12;
        } else {
            mode |= XFER_MODE_BLOCK_COUNT_ENABLE;
        }
        if matches!(direction, sdmmc_protocol::DataDirection::Read) {
            mode |= XFER_MODE_READ;
        }
        // PIO mode → DMA bit cleared.
        self.write_u8(REG_TIMEOUT_CONTROL, 0x0E);
        self.write_u16(REG_TRANSFER_MODE, mode);
    }
}

fn encode_command(cmd: &Command, has_data: bool) -> Result<u16, Error> {
    let resp_bits: u16 = match cmd.resp_type {
        ResponseType::None => CMD_RESP_NONE,
        ResponseType::R1 | ResponseType::R5 | ResponseType::R6 | ResponseType::R7 => {
            CMD_RESP_LEN48 | CMD_CRC_CHECK | CMD_INDEX_CHECK
        }
        ResponseType::R1b => CMD_RESP_LEN48_BUSY | CMD_CRC_CHECK | CMD_INDEX_CHECK,
        ResponseType::R2 => CMD_RESP_LEN136 | CMD_CRC_CHECK,
        ResponseType::R3 | ResponseType::R4 => CMD_RESP_LEN48,
    };

    let data_bit = if has_data { CMD_DATA_PRESENT } else { 0 };
    let cmd_index = (cmd.cmd as u16) << 8;
    Ok(cmd_index | data_bit | resp_bits)
}

fn decode_response(host: &Sdhci, resp_type: ResponseType) -> Result<Response, Error> {
    Ok(match resp_type {
        ResponseType::None => Response::None,
        ResponseType::R1 | ResponseType::R1b => {
            Response::R1(R1Response::from_native_raw(host.response32(0))?)
        }
        ResponseType::R2 => Response::R2(read_r2(host)),
        ResponseType::R3 => Response::R3(OcrResponse::from_raw(host.response32(0))),
        ResponseType::R4 | ResponseType::R5 => {
            // SDIO IO commands aren't part of the MVP; surface them as
            // "bad response" rather than silently returning zeros.
            return Err(Error::BadResponse(ErrorContext::default()));
        }
        ResponseType::R6 => Response::R6(RcaResponse::from_raw(host.response32(0))),
        ResponseType::R7 => Response::R7(IfCondResponse::from_raw(host.response32(0))),
    })
}

/// Reconstruct the on-bus 128-bit R2 frame from the four 32-bit response
/// slots, then serialize it MSB-first into the 16-byte buffer that the
/// protocol layer's [`sdmmc_protocol::CsdResponse`] / `CidResponse`
/// parsers expect.
///
/// SDHCI strips the start/tr/reserved header (top 8 bits of the on-bus
/// frame) and the CRC7+end (bottom 8 bits), then stores `card_resp[127:8]`
/// shifted up by 8 across `REG_RESPONSE0..REG_RESPONSE3`. We undo the
/// shift the same way Linux's `sdhci_finish_command` does.
fn read_r2(host: &Sdhci) -> [u8; 16] {
    let raw0 = host.response32(0);
    let raw1 = host.response32(1);
    let raw2 = host.response32(2);
    let raw3 = host.response32(3);

    let words = [
        (raw3 << 8) | (raw2 >> 24),
        (raw2 << 8) | (raw1 >> 24),
        (raw1 << 8) | (raw0 >> 24),
        raw0 << 8,
    ];

    let mut bytes = [0u8; 16];
    for (i, word) in words.iter().enumerate() {
        bytes[i * 4..(i + 1) * 4].copy_from_slice(&word.to_be_bytes());
    }
    bytes
}

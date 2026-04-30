use crate::response::ResponseType;

/// SD/MMC command definitions
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub struct Command {
    pub cmd: u8,
    pub arg: u32,
    pub resp_type: ResponseType,
}

impl Command {
    pub const fn new(cmd: u8, arg: u32, resp_type: ResponseType) -> Self {
        Self {
            cmd,
            arg,
            resp_type,
        }
    }

    /// Command index (0–63)
    pub fn index(&self) -> u8 {
        self.cmd
    }

    /// 32-bit argument
    pub fn argument(&self) -> u32 {
        self.arg
    }

    /// Compute the 7-bit CRC for SPI mode transmission
    pub fn crc7(&self) -> u8 {
        let mut crc: u8 = 0;
        // The token is: 01 | cmd[5:0]
        let token: u8 = 0x40 | (self.cmd & 0x3F);
        crc = crc7_update(crc, token);
        for byte in self.arg.to_be_bytes() {
            crc = crc7_update(crc, byte);
        }
        (crc << 1) | 1 // shift left by 1 and set end bit
    }

    /// Build the 6-byte SPI command packet
    pub fn to_spi_bytes(&self) -> [u8; 6] {
        let crc = self.crc7();
        let token = 0x40 | (self.cmd & 0x3F);
        let arg = self.arg.to_be_bytes();
        [token, arg[0], arg[1], arg[2], arg[3], crc]
    }
}

fn crc7_update(crc: u8, byte: u8) -> u8 {
    let mut crc = crc;
    let mut data = byte;
    for _ in 0..8 {
        crc <<= 1;
        if (crc ^ data) & 0x80 != 0 {
            crc ^= 0x89;
        }
        data <<= 1;
    }
    crc
}

// ── Standard SD/MMC Commands ─────────────────────────────────────────

// ── Broadcast commands (bc: no response, bcr: response) ──

/// CMD0: GO_IDLE_STATE — Reset all cards to idle
pub const CMD0: Command = Command::new(0, 0, ResponseType::None);

/// CMD2: ALL_SEND_CID — Request CID from all cards
pub const CMD2: Command = Command::new(2, 0, ResponseType::R2);

/// CMD3: SEND_RELATIVE_ADDR (SD) or SET_RELATIVE_ADDR (MMC)
pub const CMD3_SD: Command = Command::new(3, 0, ResponseType::R6);
/// CMD3 MMC variant: arg contains the desired RCA
pub fn cmd3_mmc(rca: u16) -> Command {
    Command::new(3, (rca as u32) << 16, ResponseType::R1)
}

/// CMD4: SET_DSR — Program driver stage register
pub fn cmd4(dsr: u16) -> Command {
    Command::new(4, (dsr as u32) << 16, ResponseType::None)
}

/// CMD6: SWITCH_FUNC — Switch card function
pub fn cmd6(arg: u32) -> Command {
    Command::new(6, arg, ResponseType::R1)
}

/// CMD7: SELECT/DESELECT CARD
pub fn cmd7(rca: u16) -> Command {
    Command::new(7, (rca as u32) << 16, ResponseType::R1b)
}

/// CMD8: SEND_IF_COND — Send interface condition (SD)
pub fn cmd8(voltage: u8, check_pattern: u8) -> Command {
    let arg = ((voltage as u32) << 8) | check_pattern as u32;
    Command::new(8, arg, ResponseType::R7)
}

/// CMD9: SEND_CSD — Get CSD register
pub fn cmd9(rca: u16) -> Command {
    Command::new(9, (rca as u32) << 16, ResponseType::R2)
}

/// CMD10: SEND_CID — Get CID register
pub fn cmd10(rca: u16) -> Command {
    Command::new(10, (rca as u32) << 16, ResponseType::R2)
}

/// CMD12: STOP_TRANSMISSION — Stop read/write
pub const CMD12: Command = Command::new(12, 0, ResponseType::R1b);

/// CMD13: SEND_STATUS
pub fn cmd13(rca: u16) -> Command {
    Command::new(13, (rca as u32) << 16, ResponseType::R1)
}

/// CMD16: SET_BLOCKLEN
pub fn cmd16(block_len: u32) -> Command {
    Command::new(16, block_len, ResponseType::R1)
}

/// CMD17: READ_SINGLE_BLOCK
pub fn cmd17(addr: u32) -> Command {
    Command::new(17, addr, ResponseType::R1)
}

/// CMD18: READ_MULTIPLE_BLOCK
pub fn cmd18(addr: u32) -> Command {
    Command::new(18, addr, ResponseType::R1)
}

/// CMD24: WRITE_BLOCK
pub fn cmd24(addr: u32) -> Command {
    Command::new(24, addr, ResponseType::R1)
}

/// CMD25: WRITE_MULTIPLE_BLOCK
pub fn cmd25(addr: u32) -> Command {
    Command::new(25, addr, ResponseType::R1)
}

/// CMD32: ERASE_WR_BLK_START
pub fn cmd32(addr: u32) -> Command {
    Command::new(32, addr, ResponseType::R1)
}

/// CMD33: ERASE_WR_BLK_END
pub fn cmd33(addr: u32) -> Command {
    Command::new(33, addr, ResponseType::R1)
}

/// CMD38: ERASE
pub const CMD38: Command = Command::new(38, 0, ResponseType::R1b);

/// CMD41: SD_SEND_OP_COND — Send operating condition (SD only)
pub fn cmd41(hcs: bool, voltage_window: u32) -> Command {
    let arg = if hcs { 0x4000_0000 } else { 0 } | (voltage_window & 0x00FF_F800);
    Command::new(41, arg, ResponseType::R3)
}

/// CMD55: APP_CMD — Next command is application-specific
pub fn cmd55(rca: u16) -> Command {
    Command::new(55, (rca as u32) << 16, ResponseType::R1)
}

/// CMD58: READ_OCR — Read OCR register
pub const CMD58: Command = Command::new(58, 0, ResponseType::R3);

// ── MMC specific ──

/// CMD1: SEND_OP_COND (MMC)
pub fn cmd1(voltage_window: u32) -> Command {
    Command::new(1, voltage_window, ResponseType::R3)
}

// ── SDIO specific commands ──

/// CMD5: IO_SEND_OP_COND (SDIO)
pub const CMD5: Command = Command::new(5, 0, ResponseType::R4);

/// CMD52: IO_RW_DIRECT
pub fn cmd52(write: bool, function: u8, raw: bool, addr: u8, data: u8) -> Command {
    let arg = (write as u32) << 31
        | ((function as u32) & 0x7) << 28
        | (raw as u32) << 27
        | ((addr as u32) & 0x1FF) << 9
        | (data as u32);
    Command::new(52, arg, ResponseType::R5)
}

/// CMD53: IO_RW_EXTENDED
pub fn cmd53(
    write: bool,
    function: u8,
    block_mode: bool,
    addr: u32,
    op_code: bool,
    count: u16,
) -> Command {
    let arg = (write as u32) << 31
        | ((function as u32) & 0x7) << 28
        | (block_mode as u32) << 27
        | (addr & 0x1FF) << 9
        | (op_code as u32) << 26
        | (count as u32 & 0x1FF);
    Command::new(53, arg, ResponseType::R5)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cmd0_crc() {
        let bytes = CMD0.to_spi_bytes();
        // CMD0 with arg=0: 0x40 0x00 0x00 0x00 0x00, CRC should be 0x95
        assert_eq!(bytes[0], 0x40);
        assert_eq!(bytes[5], 0x95);
    }

    #[test]
    fn test_cmd8_spi_bytes() {
        let cmd = cmd8(0x01, 0xAA);
        let bytes = cmd.to_spi_bytes();
        assert_eq!(bytes[0], 0x48); // 0x40 | 8
        assert_eq!(bytes[1], 0x00);
        assert_eq!(bytes[2], 0x00);
        assert_eq!(bytes[3], 0x01);
        assert_eq!(bytes[4], 0xAA);
    }
}

use crate::error::{CardError, Error};

/// SD/MMC response types
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub enum ResponseType {
    /// No response
    None,
    /// R1: Standard response (48-bit)
    R1,
    /// R1b: R1 with busy signal
    R1b,
    /// R2: CID/CSD register (136-bit)
    R2,
    /// R3: OCR register (48-bit)
    R3,
    /// R4: SDIO OCR (48-bit)
    R4,
    /// R5: SDIO RW (48-bit)
    R5,
    /// R6: Published RCA (48-bit, SD)
    R6,
    /// R7: Card interface condition (48-bit)
    R7,
}

/// Parsed response from the card
#[derive(Debug, Clone, Copy)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub enum Response {
    None,
    R1(R1Response),
    R1b(R1Response),
    R2([u8; 16]),
    R3(OcrResponse),
    R4(SdioOcrResponse),
    R5(SdioRwResponse),
    R6(RcaResponse),
    R7(IfCondResponse),
}

/// R1: Standard response — contains status bits
#[derive(Debug, Clone, Copy)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub struct R1Response {
    pub raw: u32,
}

impl R1Response {
    pub fn from_raw(raw: u32) -> Result<Self, Error> {
        let r = Self { raw };
        if raw > 0xFF {
            let err_bits = ((raw >> 19) & 0x3F) as u8;
            if err_bits != 0 {
                return Err(Error::CardError(decode_card_error(err_bits)));
            }
        }
        Ok(r)
    }

    /// Card is in idle state
    pub fn idle(&self) -> bool {
        self.raw & (1 << 0) != 0
    }

    /// Erase reset
    pub fn erase_reset(&self) -> bool {
        self.raw & (1 << 1) != 0
    }

    /// Illegal command
    pub fn illegal_command(&self) -> bool {
        self.raw & (1 << 2) != 0
    }

    /// Command CRC failed
    pub fn command_crc_failed(&self) -> bool {
        self.raw & (1 << 3) != 0
    }

    /// Current state of the card state machine (bits 12:15)
    pub fn current_state(&self) -> CardState {
        match ((self.raw >> 9) & 0xF) as u8 {
            0 => CardState::Idle,
            1 => CardState::Ready,
            2 => CardState::Identification,
            3 => CardState::Standby,
            4 => CardState::Transfer,
            5 => CardState::SendingData,
            6 => CardState::ReceiveData,
            7 => CardState::Programming,
            8 => CardState::Disconnect,
            other => CardState::Reserved(other),
        }
    }

    /// Card is locked
    pub fn card_is_locked(&self) -> bool {
        self.raw & (1 << 19) != 0
    }
}

/// Card state machine states
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub enum CardState {
    Idle,
    Ready,
    Identification,
    Standby,
    Transfer,
    SendingData,
    ReceiveData,
    Programming,
    Disconnect,
    Reserved(u8),
}

/// OCR register (R3/CMD58 response)
#[derive(Debug, Clone, Copy)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub struct OcrResponse {
    pub raw: u32,
}

impl OcrResponse {
    pub fn from_raw(raw: u32) -> Self {
        Self { raw }
    }

    /// Card power up status — true if card has completed power-up
    pub fn card_powered_up(&self) -> bool {
        self.raw & (1 << 31) != 0
    }

    /// Card Capacity Status (CCS): true = SDHC/SDXC, false = SDSC
    pub fn ccs(&self) -> bool {
        self.raw & (1 << 30) != 0
    }

    /// Supported voltage range (bits 23:0)
    pub fn voltage_window(&self) -> u32 {
        self.raw & 0x00FF_FF00
    }

    /// Supports 3.5–3.6V
    pub fn vdd_35_36(&self) -> bool {
        self.raw & (1 << 23) != 0
    }

    /// Supports 3.4–3.5V
    pub fn vdd_34_35(&self) -> bool {
        self.raw & (1 << 22) != 0
    }

    /// Supports 3.3–3.4V
    pub fn vdd_33_34(&self) -> bool {
        self.raw & (1 << 21) != 0
    }

    /// Supports 3.2–3.3V
    pub fn vdd_32_33(&self) -> bool {
        self.raw & (1 << 20) != 0
    }

    /// Supports 2.7–3.6V (typical operating range)
    pub fn supports_2v7_to_3v6(&self) -> bool {
        self.raw & 0x00FF_8000 != 0
    }

    /// UHS-II supported
    pub fn uhs2(&self) -> bool {
        self.raw & (1 << 29) != 0
    }
}

/// R6: Published RCA response
#[derive(Debug, Clone, Copy)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub struct RcaResponse {
    pub raw: u32,
}

impl RcaResponse {
    pub fn from_raw(raw: u32) -> Self {
        Self { raw }
    }

    /// Relative card address (bits 31:16)
    pub fn rca(&self) -> u16 {
        ((self.raw >> 16) & 0xFFFF) as u16
    }

    /// Status bits (bits 15:0) — subset of R1 status
    pub fn status(&self) -> u16 {
        (self.raw & 0xFFFF) as u16
    }
}

/// R7: Interface condition response
#[derive(Debug, Clone, Copy)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub struct IfCondResponse {
    pub raw: u32,
}

impl IfCondResponse {
    pub fn from_raw(raw: u32) -> Self {
        Self { raw }
    }

    /// Supported voltage (bits 11:8)
    pub fn voltage(&self) -> u8 {
        ((self.raw >> 8) & 0xF) as u8
    }

    /// Echo-back check pattern (bits 7:0)
    pub fn check_pattern(&self) -> u8 {
        (self.raw & 0xFF) as u8
    }

    /// Verify response matches expected voltage and pattern
    pub fn verify(&self, voltage: u8, pattern: u8) -> bool {
        self.voltage() == voltage && self.check_pattern() == pattern
    }
}

/// SDIO OCR (R4/CMD5 response)
#[derive(Debug, Clone, Copy)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub struct SdioOcrResponse {
    pub raw: u32,
}

impl SdioOcrResponse {
    pub fn from_raw(raw: u32) -> Self {
        Self { raw }
    }

    /// Number of I/O functions (bits 27:28)
    pub fn io_functions(&self) -> u8 {
        ((self.raw >> 28) & 0x7) as u8
    }

    /// Memory present
    pub fn memory_present(&self) -> bool {
        self.raw & (1 << 27) != 0
    }

    /// I/O ready
    pub fn io_ready(&self) -> bool {
        self.raw & (1 << 31) != 0
    }
}

/// SDIO R5 response
#[derive(Debug, Clone, Copy)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub struct SdioRwResponse {
    pub raw: u32,
}

impl SdioRwResponse {
    pub fn from_raw(raw: u32) -> Self {
        Self { raw }
    }

    /// Read/write data (bits 7:0)
    pub fn data(&self) -> u8 {
        (self.raw & 0xFF) as u8
    }

    /// Response flags (bits 15:8)
    pub fn flags(&self) -> u8 {
        ((self.raw >> 8) & 0xFF) as u8
    }
}

fn decode_card_error(bits: u8) -> CardError {
    match bits {
        0b0000_0100 => CardError::EraseSequence,
        0b0000_1000 => CardError::CommandCrcFailed,
        0b0001_0000 => CardError::IllegalCommand,
        0b0010_0000 => CardError::CardEccFailed,
        0b0100_0000 => CardError::AddressError,
        0b0000_0000 => CardError::ControllerError,
        v => CardError::Unknown(v),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spi_r1_idle_uses_bit_zero() {
        let response = R1Response::from_raw(0x01).unwrap();
        assert!(response.idle());
    }

    #[test]
    fn spi_r1_illegal_command_uses_bit_two() {
        let response = R1Response::from_raw(0x04).unwrap();
        assert!(response.illegal_command());
    }
}

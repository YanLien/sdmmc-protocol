#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub enum Error {
    /// No response from card
    Timeout,
    /// CRC check failed
    Crc,
    /// Card is not responding or not inserted
    NoCard,
    /// Command not supported
    UnsupportedCommand,
    /// Bad response received
    BadResponse,
    /// Card returned an error in R1 response
    CardError(CardError),
    /// Write operation failed
    WriteError,
    /// Read operation failed
    ReadError,
    /// Misaligned address or length
    Misaligned,
    /// Invalid argument
    InvalidArgument,
    /// Card is locked
    CardLocked,
    /// Communication error on the bus
    BusError,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub enum CardError {
    /// A command was issued out of sequence
    IllegalCommand,
    /// CRC check of the last command failed
    CommandCrcFailed,
    /// Erase sequence error
    EraseSequence,
    /// Address alignment error
    AddressError,
    /// Card internal ECC error
    CardEccFailed,
    /// Generic controller error
    ControllerError,
    /// Unknown error bit set
    Unknown(u8),
}

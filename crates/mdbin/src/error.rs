use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    Truncated,
    VarintOverflow,
    NonMinimalVarint,
    CountTooLarge,
    FrameTooLarge,
    BadMagic,
    BadVersion,
    BadEnum,
    BadChannel,
    BadScale,
    BadUtf8,
    BadDecimal,
    BadHex,
    BadAddress,
    BadSignature,
    DecimalOverflow,
    NotSorted,
    /// The input is not one this codec can carry exactly, so the caller must
    /// forward it unchanged instead (see `Body::RawJson`).
    NotRepresentable,
    UnknownInstrument,
    MissingState,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{self:?}")
    }
}

impl std::error::Error for Error {}

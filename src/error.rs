//! Crate-local error type.
//!
//! Defined as a small std-only enum so the crate can be built with the
//! default `registry` feature off — i.e. without depending on
//! `oxideav-core` at all. When the `registry` feature is on (the default)
//! a `From<AvifError> for oxideav_core::Error` impl is enabled in
//! [`crate::registry`] so the `Decoder` trait surface still interoperates
//! cleanly.
//!
//! The variants mirror the subset of `oxideav_core::Error` that the
//! AVIF container parser + composition pipeline actually produces.

use core::fmt;

/// Crate-local error type for the AVIF parser + decoder pipeline.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AvifError {
    /// Bitstream / box layout / property was malformed.
    InvalidData(String),
    /// Bitstream was syntactically valid but uses a feature this crate
    /// does not implement yet.
    Unsupported(String),
    /// The input exceeds a caller-supplied bound
    /// ([`crate::DecodeOptions`]: dimensions, pixel count, byte
    /// length) — refused before any allocation.
    LimitExceeded(String),
    /// A read / write on a caller-supplied stream failed
    /// ([`crate::decode_from`] / [`crate::encode_to`]); carries the
    /// `std::io::Error`'s message.
    Io(String),
}

impl AvifError {
    /// Construct an [`AvifError::InvalidData`].
    pub fn invalid(msg: impl Into<String>) -> Self {
        Self::InvalidData(msg.into())
    }

    /// Construct an [`AvifError::Unsupported`].
    pub fn unsupported(msg: impl Into<String>) -> Self {
        Self::Unsupported(msg.into())
    }

    /// Construct an [`AvifError::LimitExceeded`].
    pub fn limit(msg: impl Into<String>) -> Self {
        Self::LimitExceeded(msg.into())
    }
}

impl fmt::Display for AvifError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidData(s) => write!(f, "invalid data: {s}"),
            Self::Unsupported(s) => write!(f, "unsupported: {s}"),
            Self::LimitExceeded(s) => write!(f, "limit exceeded: {s}"),
            Self::Io(s) => write!(f, "I/O error: {s}"),
        }
    }
}

impl std::error::Error for AvifError {}

impl From<std::io::Error> for AvifError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e.to_string())
    }
}

/// Bridge the container crate's errors. `InvalidData` / `Unsupported`
/// map one-to-one; a structural-limit refusal (`ResourceExhausted`) is
/// reported as malformed input, which is what this crate's own bounds
/// checks always said about hostile counts and sizes.
impl From<oxideav_heif::HeifError> for AvifError {
    fn from(e: oxideav_heif::HeifError) -> Self {
        match e {
            oxideav_heif::HeifError::InvalidData(s) => Self::InvalidData(s),
            oxideav_heif::HeifError::Unsupported(s) => Self::Unsupported(s),
            oxideav_heif::HeifError::ResourceExhausted(s) => Self::InvalidData(s),
            // An error class the container adds later is still a
            // refusal of the input from this crate's point of view.
            other => Self::InvalidData(other.to_string()),
        }
    }
}

/// Crate-local result alias used throughout the parser + composition
/// pipeline.
pub type Result<T> = core::result::Result<T, AvifError>;

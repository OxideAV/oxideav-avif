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
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AvifError {
    /// Bitstream / box layout / property was malformed.
    InvalidData(String),
    /// Bitstream was syntactically valid but uses a feature this crate
    /// does not implement yet.
    Unsupported(String),
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
}

impl fmt::Display for AvifError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidData(s) => write!(f, "invalid data: {s}"),
            Self::Unsupported(s) => write!(f, "unsupported: {s}"),
        }
    }
}

impl std::error::Error for AvifError {}

/// Crate-local result alias used throughout the parser + composition
/// pipeline.
pub type Result<T> = core::result::Result<T, AvifError>;

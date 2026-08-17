use std::fmt::Display;

/// Errors produced by carbonite schema tracing, serialization, and deserialization.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// The type's `Deserialize` impl could not be traced into a schema.
    #[error("cannot trace schema: {reason}")]
    Untraceable {
        /// Why tracing failed.
        reason: String,
    },
    /// Nesting exceeded the maximum depth. Recursive types are not supported.
    #[error("maximum nesting depth exceeded (recursive types are not supported)")]
    DepthLimitExceeded,
    /// A value did not match the schema it was serialized against.
    #[error("schema mismatch: expected {expected}, found {found}")]
    SchemaMismatch {
        /// What the schema called for.
        expected: String,
        /// What the value provided.
        found: String,
    },
    /// Input ended before a complete value was read.
    #[error("unexpected end of input")]
    UnexpectedEof,
    /// Input contained bytes beyond the encoded value.
    #[error("trailing bytes after value")]
    TrailingBytes,
    /// A string column contained invalid UTF-8.
    #[error("invalid utf-8 in string data")]
    InvalidUtf8,
    /// A varint was malformed or overflowed 64 bits.
    #[error("invalid varint")]
    InvalidVarint,
    /// A tag, presence byte, or char value was out of range.
    #[error("invalid {what} value {value}")]
    InvalidTag {
        /// What kind of tag was being decoded.
        what: &'static str,
        /// The out-of-range value.
        value: u64,
    },
    /// A row was serialized with fewer fields than the schema requires
    /// (e.g. via `#[serde(skip_serializing_if)]`); columnar rows must be complete.
    #[error(
        "incomplete row: every field must be serialized (skip_serializing_if is not supported)"
    )]
    IncompleteRow,
    /// The operation relies on a serde feature this format cannot support.
    #[error("unsupported: {0}")]
    Unsupported(&'static str),
    /// The input is structurally malformed.
    #[error("malformed input: {0}")]
    Malformed(&'static str),
    /// An error raised by a `Serialize` or `Deserialize` implementation.
    #[error("{0}")]
    Message(String),
}

/// Convenience alias used throughout the crate.
pub type Result<T, E = Error> = std::result::Result<T, E>;

impl serde::ser::Error for Error {
    fn custom<T: Display>(msg: T) -> Self {
        Error::Message(msg.to_string())
    }
}

impl serde::de::Error for Error {
    fn custom<T: Display>(msg: T) -> Self {
        Error::Message(msg.to_string())
    }
}

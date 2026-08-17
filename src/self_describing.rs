//! Self-describing framing: the schema prepended to the data blob.
//!
//! Frame layout: the magic bytes `CRBN`, a [`FORMAT_VERSION`] varint, a
//! varint-length-prefixed schema in its versioned binary encoding, then the
//! data blob for the remainder of the input.

use std::fmt;
use std::marker::PhantomData;

use serde::Serialize;
use serde::de::{Deserialize, DeserializeOwned};

use crate::columnar::{DeserializeColumns, SerializeColumns};
use crate::de::Deserializer;
use crate::error::{Error, Result};
use crate::schema::{Schema, SchemaNode};
use crate::ser::Serializer;
use crate::varint;

/// The four bytes every self-describing carbonite blob starts with.
pub const MAGIC: [u8; 4] = *b"CRBN";

/// Version of carbonite's self-describing frame.
///
/// carbonite commits to reading every frame version up to and including this
/// one. A frame declaring a newer version is rejected with
/// [`Error::UnsupportedVersion`] rather than misinterpreted. The schema
/// inside the frame carries its own
/// [`SCHEMA_VERSION`](crate::SCHEMA_VERSION).
pub const FORMAT_VERSION: u64 = 1;

/// Whether `input` begins with carbonite's self-describing magic bytes.
///
/// A cheap sniff for dispatching on file type; it says nothing about whether
/// the rest of the blob is well formed.
#[must_use]
pub fn is_self_describing(input: &[u8]) -> bool {
    input.starts_with(&MAGIC)
}

/// Reads the schema out of a self-describing blob without decoding the data,
/// returning it alongside the remaining data bytes.
///
/// Use this to inspect a file's shape, to dispatch on what it holds, or to
/// hand the writer's schema to
/// [`Deserializer::new_untraced`](crate::Deserializer::new_untraced). `T` is
/// only the type you intend to read as — no check against it happens here.
///
/// ```
/// # use serde::{Serialize, Deserialize};
/// #[derive(Serialize, Deserialize)]
/// struct Save { hp: u32 }
///
/// let bytes = carbonite::to_vec(&Save { hp: 90 })?;
/// let (schema, data) = carbonite::peek_schema::<Save>(&bytes)?;
/// assert_eq!(schema.to_string(), "struct `Save`");
/// assert!(!data.is_empty());
/// # Ok::<(), carbonite::Error>(())
/// ```
///
/// # Errors
///
/// Fails if the magic bytes are missing, the frame version is newer than
/// [`FORMAT_VERSION`], or the embedded schema is malformed.
pub fn peek_schema<T: ?Sized>(input: &[u8]) -> Result<(Schema<T>, &[u8])> {
    split(input)
}

/// Serializes values with the schema embedded in each output, so the bytes
/// alone are enough to deserialize later.
pub struct SelfDescribingSerializer<'s, T: ?Sized> {
    inner: Serializer<'s, T>,
    schema_bytes: Vec<u8>,
}

impl<T: ?Sized> fmt::Debug for SelfDescribingSerializer<'_, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SelfDescribingSerializer")
            .field("schema", &format_args!("{}", self.inner.schema()))
            .field("schema_bytes", &self.schema_bytes.len())
            .finish()
    }
}

impl<'s, T: ?Sized> SelfDescribingSerializer<'s, T> {
    /// Builds a self-describing serializer for `schema`, borrowed or owned —
    /// see [`Serializer::new`].
    #[must_use]
    pub fn new(schema: impl Into<std::borrow::Cow<'s, Schema<T>>>) -> Self {
        let inner = Serializer::new(schema);
        let schema_bytes = inner.schema().to_bytes();
        SelfDescribingSerializer {
            inner,
            schema_bytes,
        }
    }

    /// The schema this serializer embeds.
    #[must_use]
    pub fn schema(&self) -> &Schema<T> {
        self.inner.schema()
    }

    /// Serializes one value, prefixed by the frame header and schema.
    ///
    /// # Errors
    ///
    /// Same failure modes as [`Serializer::to_vec`].
    pub fn to_vec(&self, value: &T) -> Result<Vec<u8>>
    where
        T: Serialize,
    {
        self.frame(self.inner.to_vec(value)?)
    }

    /// Serializes one value through the monomorphized columnar fast path from
    /// `#[derive(Schema)]`, prefixed by the frame header and schema.
    ///
    /// Produces byte-identical output to [`Self::to_vec`]; prefer it whenever
    /// `T` carries the derive.
    ///
    /// # Errors
    ///
    /// Same failure modes as [`Serializer::to_vec_columns`].
    pub fn to_vec_columns(&self, value: &T) -> Result<Vec<u8>>
    where
        T: SerializeColumns,
    {
        self.frame(self.inner.to_vec_columns(value)?)
    }

    fn frame(&self, data: Vec<u8>) -> Result<Vec<u8>> {
        let mut out =
            Vec::with_capacity(MAGIC.len() + 2 + 10 + self.schema_bytes.len() + data.len());
        out.extend_from_slice(&MAGIC);
        varint::write(&mut out, FORMAT_VERSION);
        varint::write(&mut out, self.schema_bytes.len() as u64);
        out.extend_from_slice(&self.schema_bytes);
        out.extend_from_slice(&data);
        Ok(out)
    }
}

/// Deserializes self-describing blobs: the schema is read from the input
/// itself, so no out-of-band schema is needed.
///
/// Constructing one resolves `T`'s own schema a single time; each
/// [`from_slice`](Self::from_slice) then compares the blob's schema against
/// that cached copy to pick the fast positional path or the name-matched
/// evolution path.
pub struct SelfDescribingDeserializer<T: ?Sized> {
    /// `T`'s own schema, when it is known. `None` means always take the
    /// name-matched path.
    local: Option<SchemaNode>,
    _marker: PhantomData<fn() -> T>,
}

impl<T: ?Sized> fmt::Debug for SelfDescribingDeserializer<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SelfDescribingDeserializer")
            .field("knows_own_schema", &self.local.is_some())
            .finish()
    }
}

impl<T: DeserializeOwned> Default for SelfDescribingDeserializer<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T: DeserializeOwned> SelfDescribingDeserializer<T> {
    /// Builds a deserializer, tracing `T` once so later reads can take the
    /// fast path when a blob's schema matches.
    ///
    /// Types that cannot be traced fall back to the name-matched path rather
    /// than failing here; see [`Deserializer::new`].
    #[must_use]
    pub fn new() -> Self {
        SelfDescribingDeserializer {
            local: crate::trace::trace::<T>().ok(),
            _marker: PhantomData,
        }
    }
}

impl<T: ?Sized> SelfDescribingDeserializer<T> {
    /// Like [`Self::new`], but uses the compile-time schema from
    /// `#[derive(Schema)]` instead of tracing, so it also works for types
    /// that borrow from the input.
    #[must_use]
    pub fn new_static() -> Self
    where
        T: crate::StaticSchema,
    {
        SelfDescribingDeserializer {
            local: Some(T::schema_node()),
            _marker: PhantomData,
        }
    }

    /// Like [`Self::new`], but never resolves `T`'s schema; every read takes
    /// the name-matched path. Use for borrowing types that don't implement
    /// [`StaticSchema`](crate::StaticSchema).
    #[must_use]
    pub fn new_untraced() -> Self {
        SelfDescribingDeserializer {
            local: None,
            _marker: PhantomData,
        }
    }

    /// Deserializes one value from a self-describing blob.
    ///
    /// # Errors
    ///
    /// Fails on a bad frame (magic/version), a malformed schema, or any of
    /// the data-decoding failures of [`Deserializer::from_slice`].
    pub fn from_slice<'de>(&self, input: &'de [u8]) -> Result<T>
    where
        T: Deserialize<'de> + Sized,
    {
        self.deserializer(input)
            .and_then(|(de, data)| de.from_slice(data))
    }

    /// Deserializes one value through the monomorphized columnar fast path
    /// from `#[derive(Schema)]`.
    ///
    /// Requires the blob's schema to equal the type's own; for older or
    /// foreign schemas use [`Self::from_slice`], which handles evolution.
    ///
    /// # Errors
    ///
    /// As [`Self::from_slice`], plus [`Error::SchemaMismatch`] when the
    /// blob's schema differs from the type's.
    pub fn from_slice_columns<'de>(&self, input: &'de [u8]) -> Result<T>
    where
        T: DeserializeColumns<'de>,
    {
        self.deserializer(input)
            .and_then(|(de, data)| de.from_slice_columns(data))
    }

    fn deserializer<'de>(&self, input: &'de [u8]) -> Result<(Deserializer<'_, T>, &'de [u8])> {
        let (schema, data) = split::<T>(input)?;
        let fast = self.local.as_ref() == Some(schema.node());
        Ok((Deserializer::build(schema.into(), fast), data))
    }
}

fn split<T: ?Sized>(input: &[u8]) -> Result<(Schema<T>, &[u8])> {
    let rest = input
        .strip_prefix(MAGIC.as_slice())
        .ok_or(Error::Malformed("missing carbonite magic bytes"))?;
    let (version, used) = varint::read(rest)?;
    if version == 0 {
        return Err(Error::Malformed("invalid frame version"));
    }
    if version > FORMAT_VERSION {
        return Err(Error::UnsupportedVersion {
            what: "frame",
            found: version,
            supported: FORMAT_VERSION,
        });
    }
    let rest = &rest[used..];
    let (schema_len, used) = varint::read(rest)?;
    let rest = &rest[used..];
    let schema_len =
        usize::try_from(schema_len).map_err(|_| Error::Malformed("length overflows usize"))?;
    if rest.len() < schema_len {
        return Err(Error::UnexpectedEof);
    }
    let (schema_bytes, data) = rest.split_at(schema_len);
    Ok((Schema::from_bytes(schema_bytes)?, data))
}

//! Self-describing framing: the schema prepended to the data blob.
//!
//! Frame layout: the magic bytes `CRBN`, a format-version varint, a
//! varint-length-prefixed schema in its stable binary encoding, then the data
//! blob for the remainder of the input.

use std::marker::PhantomData;

use serde::Serialize;
use serde::de::{Deserialize, DeserializeOwned};

use crate::de::Deserializer;
use crate::error::{Error, Result};
use crate::schema::Schema;
use crate::ser::Serializer;
use crate::varint;

pub(crate) const MAGIC: [u8; 4] = *b"CRBN";
pub(crate) const FORMAT_VERSION: u64 = 1;

/// Serializes values with the schema embedded in each output, so the bytes
/// alone are enough to deserialize later.
pub struct SelfDescribingSerializer<'s, T: ?Sized> {
    inner: Serializer<'s, T>,
    schema_bytes: Vec<u8>,
}

impl<'s, T: ?Sized> SelfDescribingSerializer<'s, T> {
    /// Builds a self-describing serializer for `schema`.
    #[must_use]
    pub fn new(schema: &'s Schema<T>) -> Self {
        SelfDescribingSerializer {
            inner: Serializer::new(schema),
            schema_bytes: schema.to_bytes(),
        }
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
        let data = self.inner.to_vec(value)?;
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
pub struct SelfDescribingDeserializer<T: ?Sized>(PhantomData<fn() -> T>);

impl<T> SelfDescribingDeserializer<T> {
    /// Deserializes one value from a self-describing blob.
    ///
    /// # Errors
    ///
    /// Fails on a bad frame (magic/version), a malformed schema, or any of
    /// the data-decoding failures of [`Deserializer::from_slice`].
    pub fn from_slice(input: &[u8]) -> Result<T>
    where
        T: DeserializeOwned,
    {
        let (schema, data) = split(input)?;
        Deserializer::new(schema).from_slice(data)
    }

    /// Like [`Self::from_slice`], but uses the compile-time schema from
    /// `#[derive(Schema)]` for the fast-path check instead of tracing, so it
    /// also works for types that borrow from the input.
    ///
    /// # Errors
    ///
    /// See [`Self::from_slice`].
    pub fn from_slice_static<'de>(input: &'de [u8]) -> Result<T>
    where
        T: crate::StaticSchema + Deserialize<'de>,
    {
        let (schema, data) = split(input)?;
        Deserializer::new_static(schema).from_slice(data)
    }

    /// Like [`Self::from_slice`], but never traces `T`; use for borrowing
    /// types that don't implement [`StaticSchema`](crate::StaticSchema).
    ///
    /// # Errors
    ///
    /// See [`Self::from_slice`].
    pub fn from_slice_untraced<'de>(input: &'de [u8]) -> Result<T>
    where
        T: Deserialize<'de>,
    {
        let (schema, data) = split(input)?;
        Deserializer::new_untraced(schema).from_slice(data)
    }
}

fn split<T: ?Sized>(input: &[u8]) -> Result<(Schema<T>, &[u8])> {
    let rest = input
        .strip_prefix(MAGIC.as_slice())
        .ok_or(Error::Malformed("missing carbonite magic bytes"))?;
    let (version, used) = varint::read(rest)?;
    if version != FORMAT_VERSION {
        return Err(Error::Malformed("unsupported format version"));
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

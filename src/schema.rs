//! Schema types: a serializable description of how a value is laid out.

use std::fmt;
use std::marker::PhantomData;

use serde::de::{DeserializeOwned, Visitor};
use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::varint;

/// Maximum nesting depth for schemas, guarding both the tracer (recursive
/// types) and schema decoding (malicious input).
pub(crate) const MAX_DEPTH: usize = 256;

/// Version of carbonite's schema encoding, written as the first byte of
/// [`Schema::to_bytes`].
///
/// carbonite commits to reading every schema version up to and including this
/// one, so a `Schema` written by an older release stays readable. A schema
/// declaring a *newer* version is rejected with
/// [`Error::UnsupportedVersion`] rather than misinterpreted.
///
/// Data blobs carry no version of their own: a blob is only readable with its
/// schema in hand, so its encoding is versioned by the schema that accompanies
/// it. This constant therefore covers the **blob** encoding too — any change
/// to how data (not just schemas) reaches the wire bumps it.
pub const SCHEMA_VERSION: u64 = 1;

/// A fixed-width primitive in the serde data model.
#[doc(hidden)]
#[non_exhaustive]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Primitive {
    /// `bool`, one byte per value (`0` or `1`).
    Bool,
    /// `i8`.
    I8,
    /// `i16`, little-endian.
    I16,
    /// `i32`, little-endian.
    I32,
    /// `i64`, little-endian.
    I64,
    /// `i128`, little-endian.
    I128,
    /// `u8`.
    U8,
    /// `u16`, little-endian.
    U16,
    /// `u32`, little-endian.
    U32,
    /// `u64`, little-endian.
    U64,
    /// `u128`, little-endian.
    U128,
    /// `f32`, little-endian IEEE 754.
    F32,
    /// `f64`, little-endian IEEE 754.
    F64,
    /// `char`, encoded as a little-endian `u32` scalar value.
    Char,
}

impl Primitive {
    /// Encoded width in bytes of one value.
    pub(crate) fn width(self) -> usize {
        match self {
            Primitive::Bool | Primitive::I8 | Primitive::U8 => 1,
            Primitive::I16 | Primitive::U16 => 2,
            Primitive::I32 | Primitive::U32 | Primitive::F32 | Primitive::Char => 4,
            Primitive::I64 | Primitive::U64 | Primitive::F64 => 8,
            Primitive::I128 | Primitive::U128 => 16,
        }
    }

    pub(crate) fn name(self) -> &'static str {
        match self {
            Primitive::Bool => "bool",
            Primitive::I8 => "i8",
            Primitive::I16 => "i16",
            Primitive::I32 => "i32",
            Primitive::I64 => "i64",
            Primitive::I128 => "i128",
            Primitive::U8 => "u8",
            Primitive::U16 => "u16",
            Primitive::U32 => "u32",
            Primitive::U64 => "u64",
            Primitive::U128 => "u128",
            Primitive::F32 => "f32",
            Primitive::F64 => "f64",
            Primitive::Char => "char",
        }
    }

    /// Stable wire tag. Must never change once released.
    fn tag(self) -> u8 {
        match self {
            Primitive::Bool => 0,
            Primitive::I8 => 1,
            Primitive::I16 => 2,
            Primitive::I32 => 3,
            Primitive::I64 => 4,
            Primitive::I128 => 5,
            Primitive::U8 => 6,
            Primitive::U16 => 7,
            Primitive::U32 => 8,
            Primitive::U64 => 9,
            Primitive::U128 => 10,
            Primitive::F32 => 11,
            Primitive::F64 => 12,
            Primitive::Char => 13,
        }
    }

    fn from_tag(tag: u8) -> Option<Self> {
        Some(match tag {
            0 => Primitive::Bool,
            1 => Primitive::I8,
            2 => Primitive::I16,
            3 => Primitive::I32,
            4 => Primitive::I64,
            5 => Primitive::I128,
            6 => Primitive::U8,
            7 => Primitive::U16,
            8 => Primitive::U32,
            9 => Primitive::U64,
            10 => Primitive::U128,
            11 => Primitive::F32,
            12 => Primitive::F64,
            13 => Primitive::Char,
            _ => return None,
        })
    }
}

/// An untyped description of how a value is laid out, mirroring the serde
/// data model.
///
/// This is a carbonite implementation detail. It has to be reachable because
/// `#[derive(Schema)]` constructs one, but it is not part of the supported
/// surface: variants may be added, and the type is `#[non_exhaustive]`.
/// Work with [`Schema<T>`] instead — it is what the public API accepts and
/// what [`Schema::to_bytes`] versions.
#[doc(hidden)]
#[non_exhaustive]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum SchemaNode {
    /// A fixed-width primitive.
    Primitive(Primitive),
    /// A UTF-8 string: a varint length column plus a byte-data column.
    String,
    /// A byte array: a varint length column plus a byte-data column.
    Bytes,
    /// `()`. Occupies no columns.
    Unit,
    /// `Option<T>`: a presence-byte column; the inner value's columns only
    /// receive entries for `Some` values (dense encoding).
    Option(Box<SchemaNode>),
    /// A named unit struct. Occupies no columns.
    UnitStruct {
        /// Type name.
        name: String,
    },
    /// A named newtype struct; laid out exactly as its inner value.
    NewtypeStruct {
        /// Type name.
        name: String,
        /// The wrapped value.
        inner: Box<SchemaNode>,
    },
    /// A variable-length sequence: a varint length column, with element
    /// values flattened into the element's columns.
    Seq(Box<SchemaNode>),
    /// A fixed-length heterogeneous tuple; each element gets its own columns.
    Tuple(Vec<SchemaNode>),
    /// A named tuple struct.
    TupleStruct {
        /// Type name.
        name: String,
        /// Field schemas in declaration order.
        fields: Vec<SchemaNode>,
    },
    /// A map: a varint length column, with keys and values flattened into
    /// their own columns.
    Map {
        /// Key schema.
        key: Box<SchemaNode>,
        /// Value schema.
        value: Box<SchemaNode>,
    },
    /// A named struct with named fields; each field gets its own columns.
    Struct {
        /// Type name.
        name: String,
        /// `(field name, field schema)` in declaration order.
        fields: Vec<(String, SchemaNode)>,
    },
    /// An enum: a varint tag column (the variant index in this list); each
    /// variant's payload columns only receive entries for rows of that
    /// variant (dense encoding).
    Enum {
        /// Type name.
        name: String,
        /// `(variant name, variant shape)` in declaration order.
        variants: Vec<(String, VariantNode)>,
    },
    /// A per-row-deduplicated value ([`Shared`](crate::Shared) /
    /// [`SharedArc`](crate::SharedArc)): a varint key column (one entry per
    /// occurrence); the inner value's columns form a dictionary with one
    /// entry per unique object, in first-occurrence order.
    Shared(Box<SchemaNode>),
}

/// The shape of one enum variant.
///
/// A carbonite implementation detail; see [`SchemaNode`].
#[doc(hidden)]
#[non_exhaustive]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum VariantNode {
    /// A variant with no payload.
    Unit,
    /// A variant wrapping a single value.
    Newtype(Box<SchemaNode>),
    /// A tuple variant.
    Tuple(Vec<SchemaNode>),
    /// A struct variant with named fields.
    Struct(Vec<(String, SchemaNode)>),
}

impl SchemaNode {
    /// A short human-readable description, used in error messages and by
    /// [`Schema`]'s `Display` impl.
    pub(crate) fn describe(&self) -> String {
        match self {
            SchemaNode::Primitive(p) => p.name().to_owned(),
            SchemaNode::String => "string".to_owned(),
            SchemaNode::Bytes => "bytes".to_owned(),
            SchemaNode::Unit => "unit".to_owned(),
            SchemaNode::Option(inner) => format!("option<{}>", inner.describe()),
            SchemaNode::UnitStruct { name } => format!("unit struct `{name}`"),
            SchemaNode::NewtypeStruct { name, .. } => format!("newtype struct `{name}`"),
            SchemaNode::Seq(_) => "sequence".to_owned(),
            SchemaNode::Tuple(fields) => format!("{}-tuple", fields.len()),
            SchemaNode::TupleStruct { name, .. } => format!("tuple struct `{name}`"),
            SchemaNode::Map { .. } => "map".to_owned(),
            SchemaNode::Struct { name, .. } => format!("struct `{name}`"),
            SchemaNode::Enum { name, .. } => format!("enum `{name}`"),
            SchemaNode::Shared(inner) => format!("shared<{}>", inner.describe()),
        }
    }

    /// Encodes this schema in carbonite's versioned binary form.
    #[doc(hidden)]
    #[must_use]
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::new();
        varint::write(&mut out, SCHEMA_VERSION);
        self.encode_into(&mut out);
        out
    }

    /// Decodes a schema from its versioned binary form.
    ///
    /// # Errors
    ///
    /// Returns an error if the input declares a newer [`SCHEMA_VERSION`] than
    /// this build supports, is truncated, contains unknown tags, nests deeper
    /// than the supported limit, or has trailing bytes.
    #[doc(hidden)]
    pub fn from_bytes(bytes: &[u8]) -> Result<Self> {
        let (version, used) = varint::read(bytes)?;
        if version == 0 {
            return Err(Error::Malformed("invalid schema version"));
        }
        if version > SCHEMA_VERSION {
            return Err(Error::UnsupportedVersion {
                what: "schema",
                found: version,
                supported: SCHEMA_VERSION,
            });
        }
        let mut pos = used;
        let node = Self::decode(bytes, &mut pos, 0)?;
        if pos != bytes.len() {
            return Err(Error::TrailingBytes);
        }
        Ok(node)
    }

    fn encode_into(&self, out: &mut Vec<u8>) {
        match self {
            SchemaNode::Primitive(p) => out.push(p.tag()),
            SchemaNode::String => out.push(14),
            SchemaNode::Bytes => out.push(15),
            SchemaNode::Unit => out.push(16),
            SchemaNode::Option(inner) => {
                out.push(17);
                inner.encode_into(out);
            }
            SchemaNode::UnitStruct { name } => {
                out.push(18);
                encode_str(out, name);
            }
            SchemaNode::NewtypeStruct { name, inner } => {
                out.push(19);
                encode_str(out, name);
                inner.encode_into(out);
            }
            SchemaNode::Seq(elem) => {
                out.push(20);
                elem.encode_into(out);
            }
            SchemaNode::Tuple(fields) => {
                out.push(21);
                varint::write(out, fields.len() as u64);
                for field in fields {
                    field.encode_into(out);
                }
            }
            SchemaNode::TupleStruct { name, fields } => {
                out.push(22);
                encode_str(out, name);
                varint::write(out, fields.len() as u64);
                for field in fields {
                    field.encode_into(out);
                }
            }
            SchemaNode::Map { key, value } => {
                out.push(23);
                key.encode_into(out);
                value.encode_into(out);
            }
            SchemaNode::Struct { name, fields } => {
                out.push(24);
                encode_str(out, name);
                varint::write(out, fields.len() as u64);
                for (field_name, field) in fields {
                    encode_str(out, field_name);
                    field.encode_into(out);
                }
            }
            SchemaNode::Enum { name, variants } => {
                out.push(25);
                encode_str(out, name);
                varint::write(out, variants.len() as u64);
                for (variant_name, variant) in variants {
                    encode_str(out, variant_name);
                    variant.encode_into(out);
                }
            }
            SchemaNode::Shared(inner) => {
                out.push(26);
                inner.encode_into(out);
            }
        }
    }

    fn decode(buf: &[u8], pos: &mut usize, depth: usize) -> Result<Self> {
        if depth >= MAX_DEPTH {
            return Err(Error::DepthLimitExceeded);
        }
        let tag = take_byte(buf, pos)?;
        if let Some(p) = Primitive::from_tag(tag) {
            return Ok(SchemaNode::Primitive(p));
        }
        Ok(match tag {
            14 => SchemaNode::String,
            15 => SchemaNode::Bytes,
            16 => SchemaNode::Unit,
            17 => SchemaNode::Option(Box::new(Self::decode(buf, pos, depth + 1)?)),
            18 => SchemaNode::UnitStruct {
                name: decode_str(buf, pos)?,
            },
            19 => SchemaNode::NewtypeStruct {
                name: decode_str(buf, pos)?,
                inner: Box::new(Self::decode(buf, pos, depth + 1)?),
            },
            20 => SchemaNode::Seq(Box::new(Self::decode(buf, pos, depth + 1)?)),
            21 => {
                let len = decode_len(buf, pos)?;
                let mut fields = Vec::with_capacity(len.min(64));
                for _ in 0..len {
                    fields.push(Self::decode(buf, pos, depth + 1)?);
                }
                SchemaNode::Tuple(fields)
            }
            22 => {
                let name = decode_str(buf, pos)?;
                let len = decode_len(buf, pos)?;
                let mut fields = Vec::with_capacity(len.min(64));
                for _ in 0..len {
                    fields.push(Self::decode(buf, pos, depth + 1)?);
                }
                SchemaNode::TupleStruct { name, fields }
            }
            23 => SchemaNode::Map {
                key: Box::new(Self::decode(buf, pos, depth + 1)?),
                value: Box::new(Self::decode(buf, pos, depth + 1)?),
            },
            24 => {
                let name = decode_str(buf, pos)?;
                let len = decode_len(buf, pos)?;
                let mut fields = Vec::with_capacity(len.min(64));
                for _ in 0..len {
                    let field_name = decode_str(buf, pos)?;
                    fields.push((field_name, Self::decode(buf, pos, depth + 1)?));
                }
                SchemaNode::Struct { name, fields }
            }
            25 => {
                let name = decode_str(buf, pos)?;
                let len = decode_len(buf, pos)?;
                let mut variants = Vec::with_capacity(len.min(64));
                for _ in 0..len {
                    let variant_name = decode_str(buf, pos)?;
                    variants.push((variant_name, VariantNode::decode(buf, pos, depth + 1)?));
                }
                SchemaNode::Enum { name, variants }
            }
            26 => SchemaNode::Shared(Box::new(Self::decode(buf, pos, depth + 1)?)),
            _ => return Err(Error::Malformed("unknown schema node tag")),
        })
    }
}

impl VariantNode {
    fn encode_into(&self, out: &mut Vec<u8>) {
        match self {
            VariantNode::Unit => out.push(0),
            VariantNode::Newtype(inner) => {
                out.push(1);
                inner.encode_into(out);
            }
            VariantNode::Tuple(fields) => {
                out.push(2);
                varint::write(out, fields.len() as u64);
                for field in fields {
                    field.encode_into(out);
                }
            }
            VariantNode::Struct(fields) => {
                out.push(3);
                varint::write(out, fields.len() as u64);
                for (field_name, field) in fields {
                    encode_str(out, field_name);
                    field.encode_into(out);
                }
            }
        }
    }

    fn decode(buf: &[u8], pos: &mut usize, depth: usize) -> Result<Self> {
        if depth >= MAX_DEPTH {
            return Err(Error::DepthLimitExceeded);
        }
        Ok(match take_byte(buf, pos)? {
            0 => VariantNode::Unit,
            1 => VariantNode::Newtype(Box::new(SchemaNode::decode(buf, pos, depth + 1)?)),
            2 => {
                let len = decode_len(buf, pos)?;
                let mut fields = Vec::with_capacity(len.min(64));
                for _ in 0..len {
                    fields.push(SchemaNode::decode(buf, pos, depth + 1)?);
                }
                VariantNode::Tuple(fields)
            }
            3 => {
                let len = decode_len(buf, pos)?;
                let mut fields = Vec::with_capacity(len.min(64));
                for _ in 0..len {
                    let field_name = decode_str(buf, pos)?;
                    fields.push((field_name, SchemaNode::decode(buf, pos, depth + 1)?));
                }
                VariantNode::Struct(fields)
            }
            _ => return Err(Error::Malformed("unknown variant node tag")),
        })
    }
}

fn encode_str(out: &mut Vec<u8>, s: &str) {
    varint::write(out, s.len() as u64);
    out.extend_from_slice(s.as_bytes());
}

fn take_byte(buf: &[u8], pos: &mut usize) -> Result<u8> {
    let byte = *buf.get(*pos).ok_or(Error::UnexpectedEof)?;
    *pos += 1;
    Ok(byte)
}

fn decode_len(buf: &[u8], pos: &mut usize) -> Result<usize> {
    let (value, used) = varint::read(&buf[*pos..])?;
    *pos += used;
    usize::try_from(value).map_err(|_| Error::Malformed("length overflows usize"))
}

fn decode_str(buf: &[u8], pos: &mut usize) -> Result<String> {
    let len = decode_len(buf, pos)?;
    let end = pos.checked_add(len).ok_or(Error::UnexpectedEof)?;
    let bytes = buf.get(*pos..end).ok_or(Error::UnexpectedEof)?;
    *pos = end;
    String::from_utf8(bytes.to_vec()).map_err(|_| Error::InvalidUtf8)
}

/// A description of how values of type `T` are serialized.
///
/// Obtain one with [`Schema::new`], which discovers the schema by tracing
/// `T`'s [`Deserialize`] implementation — plain `#[derive(Serialize,
/// Deserialize)]` types work as-is, with no extra derive — or with
/// [`StaticSchema::schema`](crate::StaticSchema::schema) when `T` carries
/// `#[derive(Schema)]`.
///
/// A `Schema` can be shipped: [`Schema::to_bytes`] produces a versioned
/// binary form that [`Schema::from_bytes`] reads back, so it can be sent once
/// over a network and reused for any number of data blobs, or prepended to a
/// file. Its `Serialize`/`Deserialize` impls carry that same versioned form,
/// so a schema routed through another serde format stays version-tagged.
///
/// The schema tree itself is a carbonite implementation detail. `Schema`
/// exposes what you can rely on: equality (does this blob match my type?),
/// `Display` (what shape is this?), and the byte form.
pub struct Schema<T: ?Sized> {
    root: SchemaNode,
    _marker: PhantomData<fn() -> T>,
}

impl<T> Schema<T> {
    /// Discovers the schema for `T` by tracing its `Deserialize` impl.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Untraceable`] for types that require a
    /// self-describing format (untagged/internally tagged enums,
    /// `#[serde(flatten)]`, `deserialize_any`), and
    /// [`Error::DepthLimitExceeded`] for recursive types.
    pub fn new() -> Result<Self>
    where
        T: DeserializeOwned,
    {
        Ok(Self::from_node(crate::trace::trace::<T>()?))
    }
}

impl<T: ?Sized> Schema<T> {
    /// Wraps an untyped schema tree as the schema for `T`.
    ///
    /// The caller asserts that the tree matches how `T` serializes (or an
    /// older, field-compatible version of it). Part of carbonite's internal
    /// surface; see [`SchemaNode`].
    #[doc(hidden)]
    #[must_use]
    pub fn from_node(root: SchemaNode) -> Self {
        Schema {
            root,
            _marker: PhantomData,
        }
    }

    /// The untyped schema tree. Internal; see [`SchemaNode`].
    #[doc(hidden)]
    #[must_use]
    pub fn node(&self) -> &SchemaNode {
        &self.root
    }

    /// Consumes the schema, returning the untyped tree. Internal; see
    /// [`SchemaNode`].
    #[doc(hidden)]
    #[must_use]
    pub fn into_node(self) -> SchemaNode {
        self.root
    }

    /// Reinterprets this schema as the schema for a different type.
    ///
    /// Use this to read a blob written by an older version of a type into its
    /// current version: the schema describes the *writer*, and the reader
    /// reconciles it against `U` by field name. The caller asserts nothing —
    /// any mismatch surfaces as an ordinary deserialization error.
    #[must_use]
    pub fn cast<U: ?Sized>(self) -> Schema<U> {
        Schema {
            root: self.root,
            _marker: PhantomData,
        }
    }

    /// Encodes the schema in carbonite's versioned binary form, beginning
    /// with [`SCHEMA_VERSION`].
    #[must_use]
    pub fn to_bytes(&self) -> Vec<u8> {
        self.root.to_bytes()
    }

    /// Decodes a schema from the form produced by [`Schema::to_bytes`].
    ///
    /// # Errors
    ///
    /// [`Error::UnsupportedVersion`] if the bytes were written by a newer
    /// carbonite, and [`Error::Malformed`] / [`Error::UnexpectedEof`] /
    /// [`Error::DepthLimitExceeded`] / [`Error::TrailingBytes`] on damaged or
    /// hostile input.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self> {
        Ok(Self::from_node(SchemaNode::from_bytes(bytes)?))
    }
}

impl<T: ?Sized> Serialize for Schema<T> {
    /// Serializes the versioned byte form, so a schema routed through another
    /// serde format carries the same version marker as [`Schema::to_bytes`].
    fn serialize<S: serde::Serializer>(
        &self,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        serializer.serialize_bytes(&self.to_bytes())
    }
}

impl<'de, T: ?Sized> Deserialize<'de> for Schema<T> {
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        struct BytesVisitor;

        impl<'de> Visitor<'de> for BytesVisitor {
            type Value = Vec<u8>;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a carbonite schema in its binary form")
            }

            fn visit_bytes<E: serde::de::Error>(self, v: &[u8]) -> std::result::Result<Vec<u8>, E> {
                Ok(v.to_vec())
            }

            fn visit_byte_buf<E: serde::de::Error>(
                self,
                v: Vec<u8>,
            ) -> std::result::Result<Vec<u8>, E> {
                Ok(v)
            }

            fn visit_str<E: serde::de::Error>(self, v: &str) -> std::result::Result<Vec<u8>, E> {
                Ok(v.as_bytes().to_vec())
            }

            // Human-readable formats render bytes as a sequence of numbers.
            fn visit_seq<A: serde::de::SeqAccess<'de>>(
                self,
                mut seq: A,
            ) -> std::result::Result<Vec<u8>, A::Error> {
                let mut out = Vec::with_capacity(seq.size_hint().unwrap_or(0).min(4096));
                while let Some(byte) = seq.next_element::<u8>()? {
                    out.push(byte);
                }
                Ok(out)
            }
        }

        let bytes = deserializer.deserialize_byte_buf(BytesVisitor)?;
        Schema::from_bytes(&bytes).map_err(serde::de::Error::custom)
    }
}

impl<T: ?Sized> fmt::Display for Schema<T> {
    /// A short description of the schema's shape, e.g. ``struct `Star` ``.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.root.describe())
    }
}

impl<T: ?Sized> Clone for Schema<T> {
    fn clone(&self) -> Self {
        Schema {
            root: self.root.clone(),
            _marker: PhantomData,
        }
    }
}

/// [`Serializer`](crate::Serializer) and [`Deserializer`](crate::Deserializer)
/// accept a schema by value or by reference through these, so a schema built
/// once can back any number of engines, while one parsed from the wire can be
/// handed over outright.
impl<'s, T: ?Sized> From<Schema<T>> for std::borrow::Cow<'s, Schema<T>> {
    fn from(schema: Schema<T>) -> Self {
        std::borrow::Cow::Owned(schema)
    }
}

impl<'s, T: ?Sized> From<&'s Schema<T>> for std::borrow::Cow<'s, Schema<T>> {
    fn from(schema: &'s Schema<T>) -> Self {
        std::borrow::Cow::Borrowed(schema)
    }
}

impl<T: ?Sized> fmt::Debug for Schema<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("Schema").field(&self.root).finish()
    }
}

impl<T: ?Sized> PartialEq for Schema<T> {
    fn eq(&self, other: &Self) -> bool {
        self.root == other.root
    }
}

impl<T: ?Sized> Eq for Schema<T> {}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> SchemaNode {
        SchemaNode::Struct {
            name: "Save".to_owned(),
            fields: vec![
                ("id".to_owned(), SchemaNode::Primitive(Primitive::U32)),
                ("name".to_owned(), SchemaNode::String),
                (
                    "tags".to_owned(),
                    SchemaNode::Seq(Box::new(SchemaNode::Option(Box::new(SchemaNode::Enum {
                        name: "Tag".to_owned(),
                        variants: vec![
                            ("None".to_owned(), VariantNode::Unit),
                            (
                                "Pair".to_owned(),
                                VariantNode::Tuple(vec![
                                    SchemaNode::Primitive(Primitive::F64),
                                    SchemaNode::Bytes,
                                ]),
                            ),
                            (
                                "At".to_owned(),
                                VariantNode::Struct(vec![(
                                    "pos".to_owned(),
                                    SchemaNode::Tuple(vec![
                                        SchemaNode::Primitive(Primitive::I128),
                                        SchemaNode::Primitive(Primitive::Char),
                                    ]),
                                )]),
                            ),
                        ],
                    })))),
                ),
                (
                    "lookup".to_owned(),
                    SchemaNode::Map {
                        key: Box::new(SchemaNode::String),
                        value: Box::new(SchemaNode::NewtypeStruct {
                            name: "Meters".to_owned(),
                            inner: Box::new(SchemaNode::Primitive(Primitive::F32)),
                        }),
                    },
                ),
                (
                    "marker".to_owned(),
                    SchemaNode::UnitStruct {
                        name: "Marker".to_owned(),
                    },
                ),
                ("unit".to_owned(), SchemaNode::Unit),
                (
                    "mesh".to_owned(),
                    SchemaNode::Shared(Box::new(SchemaNode::String)),
                ),
            ],
        }
    }

    #[test]
    fn binary_encoding_round_trips() {
        let node = sample();
        let bytes = node.to_bytes();
        assert_eq!(SchemaNode::from_bytes(&bytes).unwrap(), node);
    }

    #[test]
    fn rejects_trailing_bytes() {
        let mut bytes = sample().to_bytes();
        bytes.push(0);
        assert!(matches!(
            SchemaNode::from_bytes(&bytes),
            Err(Error::TrailingBytes)
        ));
    }

    #[test]
    fn rejects_unknown_tags() {
        assert!(matches!(
            SchemaNode::from_bytes(&[SCHEMA_VERSION as u8, 0xfe]),
            Err(Error::Malformed(_))
        ));
    }

    #[test]
    fn rejects_deep_nesting() {
        // A long chain of Option tags with no terminal leaf.
        let mut bytes = vec![SCHEMA_VERSION as u8];
        bytes.extend(std::iter::repeat_n(17u8, MAX_DEPTH + 1));
        assert!(matches!(
            SchemaNode::from_bytes(&bytes),
            Err(Error::DepthLimitExceeded)
        ));
    }

    #[test]
    fn encoding_starts_with_the_schema_version() {
        let bytes = sample().to_bytes();
        assert_eq!(varint::read(&bytes).unwrap(), (SCHEMA_VERSION, 1));
    }

    #[test]
    fn rejects_a_newer_schema_version() {
        let mut bytes = Vec::new();
        varint::write(&mut bytes, SCHEMA_VERSION + 1);
        sample().encode_into(&mut bytes);
        assert!(matches!(
            SchemaNode::from_bytes(&bytes),
            Err(Error::UnsupportedVersion {
                what: "schema",
                found,
                supported: SCHEMA_VERSION,
            }) if found == SCHEMA_VERSION + 1
        ));
    }

    #[test]
    fn schema_serde_round_trips_through_a_text_format() {
        let schema = Schema::<()>::from_node(sample());
        let json = serde_json::to_string(&schema).unwrap();
        let back: Schema<()> = serde_json::from_str(&json).unwrap();
        assert_eq!(back, schema);
    }

    #[test]
    fn schema_serde_rejects_a_newer_version() {
        let mut bytes = Vec::new();
        varint::write(&mut bytes, SCHEMA_VERSION + 1);
        sample().encode_into(&mut bytes);
        let json = serde_json::to_string(&bytes).unwrap();
        assert!(serde_json::from_str::<Schema<()>>(&json).is_err());
    }

    #[test]
    fn display_describes_the_shape() {
        assert_eq!(
            Schema::<()>::from_node(sample()).to_string(),
            "struct `Save`"
        );
    }
}

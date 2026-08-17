//! Columnar deserialization: a [`serde::Deserializer`] driven by the *file's*
//! schema, reading with one cursor per column.
//!
//! When the file schema is identical to the type's own (traced) schema,
//! structs decode positionally (`visit_seq`) with zero name matching. When
//! they differ — an older or newer file — structs decode as maps keyed by the
//! file's field names, so serde's usual evolution machinery applies:
//! unknown fields are ignored (their columns are skipped), missing fields use
//! `#[serde(default)]` or report `missing field`, and `#[serde(alias)]`
//! works.

use serde::de::value::StrDeserializer;
use serde::de::{
    self, Deserialize, DeserializeOwned, DeserializeSeed, EnumAccess, MapAccess, SeqAccess,
    VariantAccess, Visitor,
};

use crate::error::{Error, Result};
use crate::layout::{LNode, Layout};
use crate::schema::{Primitive, Schema, SchemaNode, VariantNode};
use crate::varint;

/// A reading position within one column's bytes.
struct Cursor<'de> {
    buf: &'de [u8],
    pos: usize,
}

impl<'de> Cursor<'de> {
    fn take(&mut self, n: usize) -> Result<&'de [u8]> {
        let end = self.pos.checked_add(n).ok_or(Error::UnexpectedEof)?;
        let slice = self.buf.get(self.pos..end).ok_or(Error::UnexpectedEof)?;
        self.pos = end;
        Ok(slice)
    }

    fn fixed<const N: usize>(&mut self) -> Result<[u8; N]> {
        let slice = self.take(N)?;
        Ok(<[u8; N]>::try_from(slice).expect("take returned exactly N bytes"))
    }

    fn byte(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }

    fn varint(&mut self) -> Result<u64> {
        let (value, used) = varint::read(&self.buf[self.pos..])?;
        self.pos += used;
        Ok(value)
    }

    fn varint_len(&mut self) -> Result<usize> {
        usize::try_from(self.varint()?).map_err(|_| Error::Malformed("length overflows usize"))
    }

    fn at_end(&self) -> bool {
        self.pos == self.buf.len()
    }
}

/// Deserializes values of type `T` from blobs written with a given schema.
///
/// The schema passed in is the **writer's** schema — possibly from an older
/// version of `T`. One `Deserializer` can decode any number of blobs.
pub struct Deserializer<T: ?Sized> {
    schema: Schema<T>,
    layout: Layout,
    fast: bool,
}

impl<T> Deserializer<T> {
    /// Builds a deserializer, tracing `T` to detect whether the fast
    /// positional path can be used (it can whenever the writer's schema is
    /// identical to `T`'s own).
    #[must_use]
    pub fn new(schema: Schema<T>) -> Self
    where
        T: DeserializeOwned,
    {
        let fast = crate::trace::trace::<T>().is_ok_and(|local| local == *schema.node());
        Self::build(schema, fast)
    }
}

impl<T: ?Sized> Deserializer<T> {
    /// Like [`Self::new`], but uses the compile-time schema from
    /// `#[derive(Schema)]` for the fast-path check instead of tracing.
    /// Unlike [`Self::new`], this also works for types that borrow from the
    /// input.
    #[must_use]
    pub fn new_static(schema: Schema<T>) -> Self
    where
        T: crate::StaticSchema,
    {
        let fast = T::schema_node() == *schema.node();
        Self::build(schema, fast)
    }

    /// Builds a deserializer without tracing `T`; always uses the name-matched
    /// path. Use this for types that borrow from the input (`&str` fields)
    /// and don't implement [`StaticSchema`](crate::StaticSchema).
    #[must_use]
    pub fn new_untraced(schema: Schema<T>) -> Self {
        Self::build(schema, false)
    }

    fn build(schema: Schema<T>, fast: bool) -> Self {
        let layout = Layout::new(schema.node());
        Deserializer {
            schema,
            layout,
            fast,
        }
    }

    /// The writer's schema.
    #[must_use]
    pub fn schema(&self) -> &Schema<T> {
        &self.schema
    }

    /// Deserializes a single-value blob (as produced by
    /// [`Serializer::to_vec`](crate::Serializer::to_vec)).
    ///
    /// # Errors
    ///
    /// Fails on malformed input, on schema/type mismatches serde cannot
    /// reconcile, or if the blob holds more than one row (use [`Self::rows`]).
    pub fn from_slice<'de>(&self, input: &'de [u8]) -> Result<T>
    where
        T: Deserialize<'de> + Sized,
    {
        let mut rows = self.rows(input)?;
        if rows.remaining() != 1 {
            return Err(Error::Malformed("expected a single-value blob"));
        }
        rows.next().expect("one row remains")
    }

    /// Opens a blob and iterates its rows (as produced by
    /// [`Batch`](crate::Batch)).
    ///
    /// # Errors
    ///
    /// Fails if the header is malformed or the column layout does not match
    /// the schema.
    pub fn rows<'a, 'de>(&'a self, input: &'de [u8]) -> Result<Rows<'a, 'de, T>> {
        let mut header = Cursor { buf: input, pos: 0 };
        let row_count = header.varint()?;
        let column_count = header.varint()?;
        if column_count != self.layout.columns as u64 {
            return Err(Error::Malformed("column count does not match schema"));
        }
        let mut lengths = Vec::with_capacity(self.layout.columns);
        for _ in 0..self.layout.columns {
            lengths.push(header.varint_len()?);
        }
        let mut cursors = Vec::with_capacity(lengths.len());
        for length in lengths {
            cursors.push(Cursor {
                buf: header.take(length)?,
                pos: 0,
            });
        }
        if !header.at_end() {
            return Err(Error::TrailingBytes);
        }
        Ok(Rows {
            de: self,
            cursors,
            remaining: row_count,
        })
    }
}

/// Iterator over the rows of one blob.
///
/// After an error, iteration fuses (yields `None`); a decoding error mid-blob
/// leaves the column cursors in an unspecified position.
pub struct Rows<'a, 'de, T: ?Sized> {
    de: &'a Deserializer<T>,
    cursors: Vec<Cursor<'de>>,
    remaining: u64,
}

impl<T: ?Sized> Rows<'_, '_, T> {
    /// Rows not yet read.
    #[must_use]
    pub fn remaining(&self) -> u64 {
        self.remaining
    }
}

impl<'de, T> Iterator for Rows<'_, 'de, T>
where
    T: Deserialize<'de>,
{
    type Item = Result<T>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.remaining == 0 {
            return None;
        }
        self.remaining -= 1;
        let result = T::deserialize(ValueDeserializer {
            node: self.de.schema.node(),
            lnode: &self.de.layout.root,
            cursors: &mut self.cursors,
            fast: self.de.fast,
        })
        .and_then(|value| {
            // The last row must consume every column exactly.
            if self.remaining == 0 && !self.cursors.iter().all(Cursor::at_end) {
                return Err(Error::TrailingBytes);
            }
            Ok(value)
        });
        if result.is_err() {
            self.remaining = 0;
        }
        Some(result)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let n = usize::try_from(self.remaining).ok();
        (n.unwrap_or(usize::MAX), n)
    }
}

fn key_deserializer(key: &str) -> StrDeserializer<'_, Error> {
    StrDeserializer::new(key)
}

// ---------------------------------------------------------------------------
// The serde::Deserializer implementation.
// ---------------------------------------------------------------------------

/// Deserializes one value position, driven entirely by the file schema.
struct ValueDeserializer<'s, 'c, 'de> {
    node: &'s SchemaNode,
    lnode: &'s LNode,
    cursors: &'c mut Vec<Cursor<'de>>,
    fast: bool,
}

impl<'s, 'de> ValueDeserializer<'s, '_, 'de> {
    /// Reads whatever the file schema says is next and hands it to the
    /// visitor. Because the schema is in hand, this format is effectively
    /// self-describing at read time.
    fn dispatch<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value> {
        match (self.node, self.lnode) {
            (SchemaNode::Primitive(p), LNode::Primitive(col)) => {
                let cursor = &mut self.cursors[*col];
                match p {
                    Primitive::Bool => match cursor.byte()? {
                        0 => visitor.visit_bool(false),
                        1 => visitor.visit_bool(true),
                        other => Err(Error::InvalidTag {
                            what: "bool",
                            value: u64::from(other),
                        }),
                    },
                    Primitive::I8 => visitor.visit_i8(i8::from_le_bytes(cursor.fixed()?)),
                    Primitive::I16 => visitor.visit_i16(i16::from_le_bytes(cursor.fixed()?)),
                    Primitive::I32 => visitor.visit_i32(i32::from_le_bytes(cursor.fixed()?)),
                    Primitive::I64 => visitor.visit_i64(i64::from_le_bytes(cursor.fixed()?)),
                    Primitive::I128 => visitor.visit_i128(i128::from_le_bytes(cursor.fixed()?)),
                    Primitive::U8 => visitor.visit_u8(cursor.byte()?),
                    Primitive::U16 => visitor.visit_u16(u16::from_le_bytes(cursor.fixed()?)),
                    Primitive::U32 => visitor.visit_u32(u32::from_le_bytes(cursor.fixed()?)),
                    Primitive::U64 => visitor.visit_u64(u64::from_le_bytes(cursor.fixed()?)),
                    Primitive::U128 => visitor.visit_u128(u128::from_le_bytes(cursor.fixed()?)),
                    Primitive::F32 => visitor.visit_f32(f32::from_le_bytes(cursor.fixed()?)),
                    Primitive::F64 => visitor.visit_f64(f64::from_le_bytes(cursor.fixed()?)),
                    Primitive::Char => {
                        let scalar = u32::from_le_bytes(cursor.fixed()?);
                        let c = char::from_u32(scalar).ok_or(Error::InvalidTag {
                            what: "char",
                            value: u64::from(scalar),
                        })?;
                        visitor.visit_char(c)
                    }
                }
            }
            (SchemaNode::String, LNode::Str { len, data }) => {
                let n = self.cursors[*len].varint_len()?;
                let bytes = self.cursors[*data].take(n)?;
                let s = std::str::from_utf8(bytes).map_err(|_| Error::InvalidUtf8)?;
                visitor.visit_borrowed_str(s)
            }
            (SchemaNode::Bytes, LNode::Str { len, data }) => {
                let n = self.cursors[*len].varint_len()?;
                visitor.visit_borrowed_bytes(self.cursors[*data].take(n)?)
            }
            (SchemaNode::Unit | SchemaNode::UnitStruct { .. }, LNode::Unit) => visitor.visit_unit(),
            (SchemaNode::NewtypeStruct { inner, .. }, LNode::Newtype(linner)) => visitor
                .visit_newtype_struct(ValueDeserializer {
                    node: inner,
                    lnode: linner,
                    cursors: self.cursors,
                    fast: self.fast,
                }),
            (SchemaNode::Option(inner), LNode::Option { tag, inner: linner }) => {
                match self.cursors[*tag].byte()? {
                    0 => visitor.visit_none(),
                    1 => visitor.visit_some(ValueDeserializer {
                        node: inner,
                        lnode: linner,
                        cursors: self.cursors,
                        fast: self.fast,
                    }),
                    other => Err(Error::InvalidTag {
                        what: "presence",
                        value: u64::from(other),
                    }),
                }
            }
            (SchemaNode::Seq(elem), LNode::Seq { len, elem: lelem }) => {
                let remaining = self.cursors[*len].varint()?;
                visitor.visit_seq(ColSeq {
                    node: elem,
                    lnode: lelem,
                    cursors: self.cursors,
                    remaining,
                    fast: self.fast,
                })
            }
            (SchemaNode::Tuple(fields), LNode::Product(lfields)) => visitor.visit_seq(FieldsSeq {
                fields: FieldList::Plain(fields),
                lfields,
                cursors: self.cursors,
                index: 0,
                fast: self.fast,
            }),
            (SchemaNode::TupleStruct { fields, .. }, LNode::Product(lfields)) => {
                visitor.visit_seq(FieldsSeq {
                    fields: FieldList::Plain(fields),
                    lfields,
                    cursors: self.cursors,
                    index: 0,
                    fast: self.fast,
                })
            }
            (
                SchemaNode::Map { key, value },
                LNode::Map {
                    len,
                    key: lkey,
                    value: lvalue,
                },
            ) => {
                let remaining = self.cursors[*len].varint()?;
                visitor.visit_map(ColMap {
                    key,
                    lkey,
                    value,
                    lvalue,
                    cursors: self.cursors,
                    remaining,
                    awaiting_value: false,
                    fast: self.fast,
                })
            }
            (SchemaNode::Struct { fields, .. }, LNode::Product(lfields)) => {
                if self.fast {
                    visitor.visit_seq(FieldsSeq {
                        fields: FieldList::Named(fields),
                        lfields,
                        cursors: self.cursors,
                        index: 0,
                        fast: self.fast,
                    })
                } else {
                    visitor.visit_map(StructMap {
                        fields,
                        lfields,
                        cursors: self.cursors,
                        index: 0,
                        fast: self.fast,
                    })
                }
            }
            (
                SchemaNode::Enum { variants, .. },
                LNode::Enum {
                    tag,
                    variants: lvariants,
                },
            ) => {
                let raw = self.cursors[*tag].varint()?;
                let index = usize::try_from(raw)
                    .ok()
                    .filter(|i| *i < variants.len())
                    .ok_or(Error::InvalidTag {
                        what: "enum variant",
                        value: raw,
                    })?;
                let (name, shape) = &variants[index];
                visitor.visit_enum(ColEnum {
                    name,
                    shape,
                    lnode: &lvariants[index],
                    cursors: self.cursors,
                    fast: self.fast,
                })
            }
            _ => unreachable!("layout was built from this schema"),
        }
    }

    /// Advances every cursor past one value of `node` without materializing
    /// it. Used for fields the reading type does not know.
    fn skip(node: &SchemaNode, lnode: &LNode, cursors: &mut [Cursor<'de>]) -> Result<()> {
        match (node, lnode) {
            (SchemaNode::Primitive(p), LNode::Primitive(col)) => {
                cursors[*col].take(p.width())?;
            }
            (SchemaNode::String | SchemaNode::Bytes, LNode::Str { len, data }) => {
                let n = cursors[*len].varint_len()?;
                cursors[*data].take(n)?;
            }
            (SchemaNode::Unit | SchemaNode::UnitStruct { .. }, LNode::Unit) => {}
            (SchemaNode::NewtypeStruct { inner, .. }, LNode::Newtype(linner)) => {
                Self::skip(inner, linner, cursors)?;
            }
            (SchemaNode::Option(inner), LNode::Option { tag, inner: linner }) => {
                match cursors[*tag].byte()? {
                    0 => {}
                    1 => Self::skip(inner, linner, cursors)?,
                    other => {
                        return Err(Error::InvalidTag {
                            what: "presence",
                            value: u64::from(other),
                        });
                    }
                }
            }
            (SchemaNode::Seq(elem), LNode::Seq { len, elem: lelem }) => {
                let n = cursors[*len].varint()?;
                for _ in 0..n {
                    Self::skip(elem, lelem, cursors)?;
                }
            }
            (
                SchemaNode::Map { key, value },
                LNode::Map {
                    len,
                    key: lkey,
                    value: lvalue,
                },
            ) => {
                let n = cursors[*len].varint()?;
                for _ in 0..n {
                    Self::skip(key, lkey, cursors)?;
                    Self::skip(value, lvalue, cursors)?;
                }
            }
            (
                SchemaNode::Tuple(fields) | SchemaNode::TupleStruct { fields, .. },
                LNode::Product(lfields),
            ) => {
                for (field, lfield) in fields.iter().zip(lfields) {
                    Self::skip(field, lfield, cursors)?;
                }
            }
            (SchemaNode::Struct { fields, .. }, LNode::Product(lfields)) => {
                for ((_, field), lfield) in fields.iter().zip(lfields) {
                    Self::skip(field, lfield, cursors)?;
                }
            }
            (
                SchemaNode::Enum { variants, .. },
                LNode::Enum {
                    tag,
                    variants: lvariants,
                },
            ) => {
                let raw = cursors[*tag].varint()?;
                let index = usize::try_from(raw)
                    .ok()
                    .filter(|i| *i < variants.len())
                    .ok_or(Error::InvalidTag {
                        what: "enum variant",
                        value: raw,
                    })?;
                Self::skip_variant(&variants[index].1, &lvariants[index], cursors)?;
            }
            _ => unreachable!("layout was built from this schema"),
        }
        Ok(())
    }

    fn skip_variant(shape: &VariantNode, lnode: &LNode, cursors: &mut [Cursor<'de>]) -> Result<()> {
        match (shape, lnode) {
            (VariantNode::Unit, LNode::Unit) => Ok(()),
            (VariantNode::Newtype(inner), LNode::Newtype(linner)) => {
                Self::skip(inner, linner, cursors)
            }
            (VariantNode::Tuple(fields), LNode::Product(lfields)) => {
                for (field, lfield) in fields.iter().zip(lfields) {
                    Self::skip(field, lfield, cursors)?;
                }
                Ok(())
            }
            (VariantNode::Struct(fields), LNode::Product(lfields)) => {
                for ((_, field), lfield) in fields.iter().zip(lfields) {
                    Self::skip(field, lfield, cursors)?;
                }
                Ok(())
            }
            _ => unreachable!("layout was built from this schema"),
        }
    }
}

impl<'de> de::Deserializer<'de> for ValueDeserializer<'_, '_, 'de> {
    type Error = Error;

    fn deserialize_any<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value> {
        self.dispatch(visitor)
    }

    fn deserialize_option<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value> {
        if matches!(self.node, SchemaNode::Option(_)) {
            self.dispatch(visitor)
        } else {
            // The file has a bare value where the type now expects an Option:
            // treat it as present, mirroring JSON's non-null semantics. This
            // makes wrapping a field in Option a compatible change.
            visitor.visit_some(self)
        }
    }

    fn deserialize_ignored_any<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value> {
        Self::skip(self.node, self.lnode, self.cursors)?;
        visitor.visit_unit()
    }

    serde::forward_to_deserialize_any! {
        bool i8 i16 i32 i64 i128 u8 u16 u32 u64 u128 f32 f64 char str string
        bytes byte_buf unit unit_struct newtype_struct seq tuple tuple_struct
        map struct enum identifier
    }

    fn is_human_readable(&self) -> bool {
        false
    }
}

// ---------------------------------------------------------------------------
// Access objects.
// ---------------------------------------------------------------------------

/// Sequence elements, length-driven.
struct ColSeq<'s, 'c, 'de> {
    node: &'s SchemaNode,
    lnode: &'s LNode,
    cursors: &'c mut Vec<Cursor<'de>>,
    remaining: u64,
    fast: bool,
}

impl<'de> SeqAccess<'de> for ColSeq<'_, '_, 'de> {
    type Error = Error;

    fn next_element_seed<S: DeserializeSeed<'de>>(&mut self, seed: S) -> Result<Option<S::Value>> {
        if self.remaining == 0 {
            return Ok(None);
        }
        self.remaining -= 1;
        seed.deserialize(ValueDeserializer {
            node: self.node,
            lnode: self.lnode,
            cursors: self.cursors,
            fast: self.fast,
        })
        .map(Some)
    }

    fn size_hint(&self) -> Option<usize> {
        usize::try_from(self.remaining).ok()
    }
}

enum FieldList<'s> {
    Plain(&'s [SchemaNode]),
    Named(&'s [(String, SchemaNode)]),
}

impl<'s> FieldList<'s> {
    fn get(&self, index: usize) -> Option<&'s SchemaNode> {
        match self {
            FieldList::Plain(fields) => fields.get(index),
            FieldList::Named(fields) => fields.get(index).map(|(_, field)| field),
        }
    }

    fn len(&self) -> usize {
        match self {
            FieldList::Plain(fields) => fields.len(),
            FieldList::Named(fields) => fields.len(),
        }
    }
}

/// Positional fields: tuples, tuple structs/variants, and fast-path structs.
struct FieldsSeq<'s, 'c, 'de> {
    fields: FieldList<'s>,
    lfields: &'s [LNode],
    cursors: &'c mut Vec<Cursor<'de>>,
    index: usize,
    fast: bool,
}

impl<'de> SeqAccess<'de> for FieldsSeq<'_, '_, 'de> {
    type Error = Error;

    fn next_element_seed<S: DeserializeSeed<'de>>(&mut self, seed: S) -> Result<Option<S::Value>> {
        let Some((node, lnode)) = self
            .fields
            .get(self.index)
            .zip(self.lfields.get(self.index))
        else {
            return Ok(None);
        };
        self.index += 1;
        seed.deserialize(ValueDeserializer {
            node,
            lnode,
            cursors: self.cursors,
            fast: self.fast,
        })
        .map(Some)
    }

    fn size_hint(&self) -> Option<usize> {
        Some(self.fields.len() - self.index)
    }
}

/// Name-matched struct fields (the evolution path): keys come from the file
/// schema; the visitor matches them against the type's fields.
struct StructMap<'s, 'c, 'de> {
    fields: &'s [(String, SchemaNode)],
    lfields: &'s [LNode],
    cursors: &'c mut Vec<Cursor<'de>>,
    index: usize,
    fast: bool,
}

impl<'de> MapAccess<'de> for StructMap<'_, '_, 'de> {
    type Error = Error;

    fn next_key_seed<S: DeserializeSeed<'de>>(&mut self, seed: S) -> Result<Option<S::Value>> {
        let Some((name, _)) = self.fields.get(self.index) else {
            return Ok(None);
        };
        seed.deserialize(key_deserializer(name)).map(Some)
    }

    fn next_value_seed<S: DeserializeSeed<'de>>(&mut self, seed: S) -> Result<S::Value> {
        let (node, lnode) = self
            .fields
            .get(self.index)
            .map(|(_, field)| field)
            .zip(self.lfields.get(self.index))
            .expect("next_value_seed called after next_key_seed returned a key");
        self.index += 1;
        seed.deserialize(ValueDeserializer {
            node,
            lnode,
            cursors: self.cursors,
            fast: self.fast,
        })
    }

    fn size_hint(&self) -> Option<usize> {
        Some(self.fields.len() - self.index)
    }
}

/// Map entries, length-driven.
struct ColMap<'s, 'c, 'de> {
    key: &'s SchemaNode,
    lkey: &'s LNode,
    value: &'s SchemaNode,
    lvalue: &'s LNode,
    cursors: &'c mut Vec<Cursor<'de>>,
    remaining: u64,
    awaiting_value: bool,
    fast: bool,
}

impl<'de> MapAccess<'de> for ColMap<'_, '_, 'de> {
    type Error = Error;

    fn next_key_seed<S: DeserializeSeed<'de>>(&mut self, seed: S) -> Result<Option<S::Value>> {
        if self.remaining == 0 {
            return Ok(None);
        }
        self.remaining -= 1;
        self.awaiting_value = true;
        seed.deserialize(ValueDeserializer {
            node: self.key,
            lnode: self.lkey,
            cursors: self.cursors,
            fast: self.fast,
        })
        .map(Some)
    }

    fn next_value_seed<S: DeserializeSeed<'de>>(&mut self, seed: S) -> Result<S::Value> {
        debug_assert!(self.awaiting_value, "value requested before key");
        self.awaiting_value = false;
        seed.deserialize(ValueDeserializer {
            node: self.value,
            lnode: self.lvalue,
            cursors: self.cursors,
            fast: self.fast,
        })
    }

    fn size_hint(&self) -> Option<usize> {
        usize::try_from(self.remaining).ok()
    }
}

/// One enum value: hands the visitor the file's variant name, then decodes
/// the payload per the file's shape for that variant.
struct ColEnum<'s, 'c, 'de> {
    name: &'s str,
    shape: &'s VariantNode,
    lnode: &'s LNode,
    cursors: &'c mut Vec<Cursor<'de>>,
    fast: bool,
}

impl<'de> EnumAccess<'de> for ColEnum<'_, '_, 'de> {
    type Error = Error;
    type Variant = Self;

    fn variant_seed<S: DeserializeSeed<'de>>(self, seed: S) -> Result<(S::Value, Self::Variant)> {
        let value = seed.deserialize(key_deserializer(self.name))?;
        Ok((value, self))
    }
}

impl<'de> VariantAccess<'de> for ColEnum<'_, '_, 'de> {
    type Error = Error;

    fn unit_variant(self) -> Result<()> {
        match self.shape {
            VariantNode::Unit => Ok(()),
            other => Err(Error::SchemaMismatch {
                expected: variant_shape_name(other).to_owned(),
                found: format!("unit variant `{}`", self.name),
            }),
        }
    }

    fn newtype_variant_seed<S: DeserializeSeed<'de>>(self, seed: S) -> Result<S::Value> {
        match (self.shape, self.lnode) {
            (VariantNode::Newtype(inner), LNode::Newtype(linner)) => {
                seed.deserialize(ValueDeserializer {
                    node: inner,
                    lnode: linner,
                    cursors: self.cursors,
                    fast: self.fast,
                })
            }
            (other, _) => Err(Error::SchemaMismatch {
                expected: variant_shape_name(other).to_owned(),
                found: format!("newtype variant `{}`", self.name),
            }),
        }
    }

    fn tuple_variant<V: Visitor<'de>>(self, _len: usize, visitor: V) -> Result<V::Value> {
        match (self.shape, self.lnode) {
            (VariantNode::Tuple(fields), LNode::Product(lfields)) => visitor.visit_seq(FieldsSeq {
                fields: FieldList::Plain(fields),
                lfields,
                cursors: self.cursors,
                index: 0,
                fast: self.fast,
            }),
            (other, _) => Err(Error::SchemaMismatch {
                expected: variant_shape_name(other).to_owned(),
                found: format!("tuple variant `{}`", self.name),
            }),
        }
    }

    fn struct_variant<V: Visitor<'de>>(
        self,
        _fields: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value> {
        match (self.shape, self.lnode) {
            (VariantNode::Struct(fields), LNode::Product(lfields)) => {
                if self.fast {
                    visitor.visit_seq(FieldsSeq {
                        fields: FieldList::Named(fields),
                        lfields,
                        cursors: self.cursors,
                        index: 0,
                        fast: self.fast,
                    })
                } else {
                    visitor.visit_map(StructMap {
                        fields,
                        lfields,
                        cursors: self.cursors,
                        index: 0,
                        fast: self.fast,
                    })
                }
            }
            (other, _) => Err(Error::SchemaMismatch {
                expected: variant_shape_name(other).to_owned(),
                found: format!("struct variant `{}`", self.name),
            }),
        }
    }
}

fn variant_shape_name(shape: &VariantNode) -> &'static str {
    match shape {
        VariantNode::Unit => "unit variant",
        VariantNode::Newtype(_) => "newtype variant",
        VariantNode::Tuple(_) => "tuple variant",
        VariantNode::Struct(_) => "struct variant",
    }
}

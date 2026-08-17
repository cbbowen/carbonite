//! Columnar serialization: a [`serde::Serializer`] that walks the schema in
//! lockstep with the value and routes every leaf into its column buffer.

use std::marker::PhantomData;

use serde::ser::{self, Serialize};

use crate::error::{Error, Result};
use crate::layout::{ColId, LNode, Layout};
use crate::schema::{Primitive, Schema, SchemaNode, VariantNode};
use crate::varint;

/// Serializes values of type `T` according to a [`Schema<T>`].
///
/// One `Serializer` can produce any number of blobs; the schema is borrowed,
/// never re-traced.
pub struct Serializer<'s, T: ?Sized> {
    schema: &'s Schema<T>,
    layout: Layout,
}

impl<'s, T: ?Sized> Serializer<'s, T> {
    /// Builds a serializer for `schema`.
    #[must_use]
    pub fn new(schema: &'s Schema<T>) -> Self {
        Serializer {
            schema,
            layout: Layout::new(schema.node()),
        }
    }

    /// The schema this serializer writes.
    #[must_use]
    pub fn schema(&self) -> &'s Schema<T> {
        self.schema
    }

    /// Serializes a single value into a standalone data blob.
    ///
    /// # Errors
    ///
    /// Fails if the value does not match the schema, or if its `Serialize`
    /// impl skips fields (`skip_serializing_if`) or reports an error.
    pub fn to_vec(&self, value: &T) -> Result<Vec<u8>>
    where
        T: Serialize,
    {
        let mut batch = self.batch();
        batch.push(value)?;
        Ok(batch.finish())
    }

    /// Starts a batch: many values serialized against one schema into one
    /// blob, sharing columns.
    pub fn batch(&self) -> Batch<'_, T> {
        Batch {
            root_node: self.schema.node(),
            root_lnode: &self.layout.root,
            columns: vec![Vec::new(); self.layout.columns],
            rollback: Vec::with_capacity(self.layout.columns),
            rows: 0,
            _marker: PhantomData,
        }
    }
}

/// Accumulates rows of `T` into shared columns; [`Batch::finish`] produces
/// the blob.
#[must_use]
pub struct Batch<'a, T: ?Sized> {
    root_node: &'a SchemaNode,
    root_lnode: &'a LNode,
    columns: Vec<Vec<u8>>,
    rollback: Vec<usize>,
    rows: u64,
    _marker: PhantomData<fn(&T)>,
}

impl<T: ?Sized> Batch<'_, T> {
    /// Appends one value as a row.
    ///
    /// On error the batch is left unchanged, so a failed push can be retried
    /// or skipped.
    ///
    /// # Errors
    ///
    /// Same failure modes as [`Serializer::to_vec`].
    pub fn push(&mut self, value: &T) -> Result<()>
    where
        T: Serialize,
    {
        self.rollback.clear();
        self.rollback.extend(self.columns.iter().map(Vec::len));
        let result = value.serialize(ValueSerializer {
            node: self.root_node,
            lnode: self.root_lnode,
            columns: &mut self.columns,
        });
        match result {
            Ok(()) => {
                self.rows += 1;
                Ok(())
            }
            Err(err) => {
                for (column, &len) in self.columns.iter_mut().zip(&self.rollback) {
                    column.truncate(len);
                }
                Err(err)
            }
        }
    }

    /// Number of rows pushed so far.
    #[must_use]
    pub fn rows(&self) -> u64 {
        self.rows
    }

    /// Assembles the blob: row count, column count, column byte lengths,
    /// then the column bytes back to back.
    #[must_use]
    pub fn finish(self) -> Vec<u8> {
        let data_len: usize = self.columns.iter().map(Vec::len).sum();
        let mut out = Vec::with_capacity(data_len + 2 + 5 * self.columns.len());
        varint::write(&mut out, self.rows);
        varint::write(&mut out, self.columns.len() as u64);
        for column in &self.columns {
            varint::write(&mut out, column.len() as u64);
        }
        for column in &self.columns {
            out.extend_from_slice(column);
        }
        out
    }
}

// ---------------------------------------------------------------------------
// The serde::Serializer implementation.
// ---------------------------------------------------------------------------

/// Serializes one value position: a schema node, its layout node, and the
/// shared column buffers.
struct ValueSerializer<'a, 'c> {
    node: &'a SchemaNode,
    lnode: &'a LNode,
    columns: &'c mut Vec<Vec<u8>>,
}

impl ValueSerializer<'_, '_> {
    fn mismatch(&self, found: impl Into<String>) -> Error {
        Error::SchemaMismatch {
            expected: self.node.describe(),
            found: found.into(),
        }
    }

    /// Looks up an enum variant by name (using the serde-provided index as a
    /// first guess) and writes its tag.
    fn enter_variant<'a>(
        &mut self,
        variants: &'a [(String, VariantNode)],
        lvariants: &'a [LNode],
        tag_col: ColId,
        index: u32,
        name: &str,
    ) -> Result<(&'a VariantNode, &'a LNode)> {
        let guess = index as usize;
        let position = match variants.get(guess) {
            Some((variant_name, _)) if variant_name == name => guess,
            _ => variants
                .iter()
                .position(|(variant_name, _)| variant_name == name)
                .ok_or_else(|| self.mismatch(format!("variant `{name}`")))?,
        };
        varint::write(&mut self.columns[tag_col], position as u64);
        Ok((&variants[position].1, &lvariants[position]))
    }
}

macro_rules! serialize_primitive {
    ($method:ident, $ty:ty, $prim:ident) => {
        fn $method(self, v: $ty) -> Result<()> {
            match (self.node, self.lnode) {
                (SchemaNode::Primitive(Primitive::$prim), LNode::Primitive(col)) => {
                    self.columns[*col].extend_from_slice(&v.to_le_bytes());
                    Ok(())
                }
                _ => Err(self.mismatch(stringify!($ty))),
            }
        }
    };
}

impl<'a, 'c> ser::Serializer for ValueSerializer<'a, 'c> {
    type Ok = ();
    type Error = Error;
    type SerializeSeq = SeqSerializer<'a, 'c>;
    type SerializeTuple = ProductSerializer<'a, 'c>;
    type SerializeTupleStruct = ProductSerializer<'a, 'c>;
    type SerializeTupleVariant = ProductSerializer<'a, 'c>;
    type SerializeMap = MapSerializer<'a, 'c>;
    type SerializeStruct = StructSerializer<'a, 'c>;
    type SerializeStructVariant = StructSerializer<'a, 'c>;

    fn serialize_bool(self, v: bool) -> Result<()> {
        match (self.node, self.lnode) {
            (SchemaNode::Primitive(Primitive::Bool), LNode::Primitive(col)) => {
                self.columns[*col].push(u8::from(v));
                Ok(())
            }
            _ => Err(self.mismatch("bool")),
        }
    }

    serialize_primitive!(serialize_i8, i8, I8);
    serialize_primitive!(serialize_i16, i16, I16);
    serialize_primitive!(serialize_i32, i32, I32);
    serialize_primitive!(serialize_i64, i64, I64);
    serialize_primitive!(serialize_i128, i128, I128);
    serialize_primitive!(serialize_u8, u8, U8);
    serialize_primitive!(serialize_u16, u16, U16);
    serialize_primitive!(serialize_u32, u32, U32);
    serialize_primitive!(serialize_u64, u64, U64);
    serialize_primitive!(serialize_u128, u128, U128);
    serialize_primitive!(serialize_f32, f32, F32);
    serialize_primitive!(serialize_f64, f64, F64);

    fn serialize_char(self, v: char) -> Result<()> {
        match (self.node, self.lnode) {
            (SchemaNode::Primitive(Primitive::Char), LNode::Primitive(col)) => {
                self.columns[*col].extend_from_slice(&(v as u32).to_le_bytes());
                Ok(())
            }
            _ => Err(self.mismatch("char")),
        }
    }

    fn serialize_str(self, v: &str) -> Result<()> {
        match (self.node, self.lnode) {
            (SchemaNode::String, LNode::Str { len, data }) => {
                varint::write(&mut self.columns[*len], v.len() as u64);
                self.columns[*data].extend_from_slice(v.as_bytes());
                Ok(())
            }
            _ => Err(self.mismatch("string")),
        }
    }

    fn serialize_bytes(self, v: &[u8]) -> Result<()> {
        match (self.node, self.lnode) {
            (SchemaNode::Bytes, LNode::Str { len, data }) => {
                varint::write(&mut self.columns[*len], v.len() as u64);
                self.columns[*data].extend_from_slice(v);
                Ok(())
            }
            _ => Err(self.mismatch("bytes")),
        }
    }

    fn serialize_none(self) -> Result<()> {
        match (self.node, self.lnode) {
            (SchemaNode::Option(_), LNode::Option { tag, .. }) => {
                self.columns[*tag].push(0);
                Ok(())
            }
            _ => Err(self.mismatch("option")),
        }
    }

    fn serialize_some<V: ?Sized + Serialize>(self, value: &V) -> Result<()> {
        match (self.node, self.lnode) {
            (SchemaNode::Option(inner), LNode::Option { tag, inner: linner }) => {
                self.columns[*tag].push(1);
                value.serialize(ValueSerializer {
                    node: inner,
                    lnode: linner,
                    columns: self.columns,
                })
            }
            _ => Err(self.mismatch("option")),
        }
    }

    fn serialize_unit(self) -> Result<()> {
        match self.node {
            SchemaNode::Unit => Ok(()),
            _ => Err(self.mismatch("unit")),
        }
    }

    fn serialize_unit_struct(self, name: &'static str) -> Result<()> {
        match self.node {
            SchemaNode::UnitStruct { name: schema_name } if schema_name == name => Ok(()),
            _ => Err(self.mismatch(format!("unit struct `{name}`"))),
        }
    }

    fn serialize_unit_variant(
        mut self,
        _name: &'static str,
        variant_index: u32,
        variant: &'static str,
    ) -> Result<()> {
        match (self.node, self.lnode) {
            (
                SchemaNode::Enum { variants, .. },
                LNode::Enum {
                    tag,
                    variants: lvariants,
                },
            ) => {
                let (shape, _) =
                    self.enter_variant(variants, lvariants, *tag, variant_index, variant)?;
                match shape {
                    VariantNode::Unit => Ok(()),
                    _ => Err(Error::SchemaMismatch {
                        expected: format!("payload for variant `{variant}`"),
                        found: "unit variant".to_owned(),
                    }),
                }
            }
            _ => Err(self.mismatch(format!("variant `{variant}`"))),
        }
    }

    fn serialize_newtype_struct<V: ?Sized + Serialize>(
        self,
        name: &'static str,
        value: &V,
    ) -> Result<()> {
        match (self.node, self.lnode) {
            (
                SchemaNode::NewtypeStruct {
                    name: schema_name,
                    inner,
                },
                LNode::Newtype(linner),
            ) if schema_name == name => value.serialize(ValueSerializer {
                node: inner,
                lnode: linner,
                columns: self.columns,
            }),
            _ => Err(self.mismatch(format!("newtype struct `{name}`"))),
        }
    }

    fn serialize_newtype_variant<V: ?Sized + Serialize>(
        mut self,
        _name: &'static str,
        variant_index: u32,
        variant: &'static str,
        value: &V,
    ) -> Result<()> {
        match (self.node, self.lnode) {
            (
                SchemaNode::Enum { variants, .. },
                LNode::Enum {
                    tag,
                    variants: lvariants,
                },
            ) => {
                let (shape, lshape) =
                    self.enter_variant(variants, lvariants, *tag, variant_index, variant)?;
                match (shape, lshape) {
                    (VariantNode::Newtype(inner), LNode::Newtype(linner)) => {
                        value.serialize(ValueSerializer {
                            node: inner,
                            lnode: linner,
                            columns: self.columns,
                        })
                    }
                    _ => Err(Error::SchemaMismatch {
                        expected: format!("shape of variant `{variant}`"),
                        found: "newtype variant".to_owned(),
                    }),
                }
            }
            _ => Err(self.mismatch(format!("variant `{variant}`"))),
        }
    }

    fn serialize_seq(self, _len: Option<usize>) -> Result<Self::SerializeSeq> {
        match (self.node, self.lnode) {
            (SchemaNode::Seq(elem), LNode::Seq { len, elem: lelem }) => Ok(SeqSerializer {
                elem,
                lelem,
                len_col: *len,
                columns: self.columns,
                count: 0,
            }),
            _ => Err(self.mismatch("sequence")),
        }
    }

    fn serialize_tuple(self, len: usize) -> Result<Self::SerializeTuple> {
        match (self.node, self.lnode) {
            (SchemaNode::Tuple(fields), LNode::Product(lfields)) if fields.len() == len => {
                Ok(ProductSerializer {
                    fields,
                    lfields,
                    columns: self.columns,
                    index: 0,
                })
            }
            _ => Err(self.mismatch(format!("{len}-tuple"))),
        }
    }

    fn serialize_tuple_struct(
        self,
        name: &'static str,
        len: usize,
    ) -> Result<Self::SerializeTupleStruct> {
        match (self.node, self.lnode) {
            (
                SchemaNode::TupleStruct {
                    name: schema_name,
                    fields,
                },
                LNode::Product(lfields),
            ) if schema_name == name && fields.len() == len => Ok(ProductSerializer {
                fields,
                lfields,
                columns: self.columns,
                index: 0,
            }),
            _ => Err(self.mismatch(format!("tuple struct `{name}`"))),
        }
    }

    fn serialize_tuple_variant(
        mut self,
        _name: &'static str,
        variant_index: u32,
        variant: &'static str,
        len: usize,
    ) -> Result<Self::SerializeTupleVariant> {
        match (self.node, self.lnode) {
            (
                SchemaNode::Enum { variants, .. },
                LNode::Enum {
                    tag,
                    variants: lvariants,
                },
            ) => {
                let (shape, lshape) =
                    self.enter_variant(variants, lvariants, *tag, variant_index, variant)?;
                match (shape, lshape) {
                    (VariantNode::Tuple(fields), LNode::Product(lfields))
                        if fields.len() == len =>
                    {
                        Ok(ProductSerializer {
                            fields,
                            lfields,
                            columns: self.columns,
                            index: 0,
                        })
                    }
                    _ => Err(Error::SchemaMismatch {
                        expected: format!("shape of variant `{variant}`"),
                        found: format!("{len}-tuple variant"),
                    }),
                }
            }
            _ => Err(self.mismatch(format!("variant `{variant}`"))),
        }
    }

    fn serialize_map(self, _len: Option<usize>) -> Result<Self::SerializeMap> {
        match (self.node, self.lnode) {
            (
                SchemaNode::Map { key, value },
                LNode::Map {
                    len,
                    key: lkey,
                    value: lvalue,
                },
            ) => Ok(MapSerializer {
                key,
                lkey,
                value,
                lvalue,
                len_col: *len,
                columns: self.columns,
                count: 0,
                awaiting_value: false,
            }),
            _ => Err(self.mismatch("map")),
        }
    }

    fn serialize_struct(self, name: &'static str, len: usize) -> Result<Self::SerializeStruct> {
        match (self.node, self.lnode) {
            (
                SchemaNode::Struct {
                    name: schema_name,
                    fields,
                },
                LNode::Product(lfields),
            ) if schema_name == name => {
                // `len` may be smaller than the schema when fields are
                // conditionally skipped; end() reports IncompleteRow then.
                let _ = len;
                Ok(StructSerializer {
                    fields,
                    lfields,
                    columns: self.columns,
                    index: 0,
                })
            }
            _ => Err(self.mismatch(format!("struct `{name}`"))),
        }
    }

    fn serialize_struct_variant(
        mut self,
        _name: &'static str,
        variant_index: u32,
        variant: &'static str,
        _len: usize,
    ) -> Result<Self::SerializeStructVariant> {
        match (self.node, self.lnode) {
            (
                SchemaNode::Enum { variants, .. },
                LNode::Enum {
                    tag,
                    variants: lvariants,
                },
            ) => {
                let (shape, lshape) =
                    self.enter_variant(variants, lvariants, *tag, variant_index, variant)?;
                match (shape, lshape) {
                    (VariantNode::Struct(fields), LNode::Product(lfields)) => {
                        Ok(StructSerializer {
                            fields,
                            lfields,
                            columns: self.columns,
                            index: 0,
                        })
                    }
                    _ => Err(Error::SchemaMismatch {
                        expected: format!("shape of variant `{variant}`"),
                        found: "struct variant".to_owned(),
                    }),
                }
            }
            _ => Err(self.mismatch(format!("variant `{variant}`"))),
        }
    }

    fn is_human_readable(&self) -> bool {
        false
    }
}

// ---------------------------------------------------------------------------
// Compound serializers.
// ---------------------------------------------------------------------------

pub struct SeqSerializer<'a, 'c> {
    elem: &'a SchemaNode,
    lelem: &'a LNode,
    len_col: ColId,
    columns: &'c mut Vec<Vec<u8>>,
    count: u64,
}

impl ser::SerializeSeq for SeqSerializer<'_, '_> {
    type Ok = ();
    type Error = Error;

    fn serialize_element<V: ?Sized + Serialize>(&mut self, value: &V) -> Result<()> {
        value.serialize(ValueSerializer {
            node: self.elem,
            lnode: self.lelem,
            columns: self.columns,
        })?;
        self.count += 1;
        Ok(())
    }

    fn end(self) -> Result<()> {
        varint::write(&mut self.columns[self.len_col], self.count);
        Ok(())
    }
}

/// Positional fields: tuples, tuple structs, and tuple variants.
pub struct ProductSerializer<'a, 'c> {
    fields: &'a [SchemaNode],
    lfields: &'a [LNode],
    columns: &'c mut Vec<Vec<u8>>,
    index: usize,
}

impl ProductSerializer<'_, '_> {
    fn serialize_next<V: ?Sized + Serialize>(&mut self, value: &V) -> Result<()> {
        let (node, lnode) = self
            .fields
            .get(self.index)
            .zip(self.lfields.get(self.index))
            .ok_or(Error::IncompleteRow)?;
        value.serialize(ValueSerializer {
            node,
            lnode,
            columns: self.columns,
        })?;
        self.index += 1;
        Ok(())
    }

    fn finish(self) -> Result<()> {
        if self.index == self.fields.len() {
            Ok(())
        } else {
            Err(Error::IncompleteRow)
        }
    }
}

impl ser::SerializeTuple for ProductSerializer<'_, '_> {
    type Ok = ();
    type Error = Error;

    fn serialize_element<V: ?Sized + Serialize>(&mut self, value: &V) -> Result<()> {
        self.serialize_next(value)
    }

    fn end(self) -> Result<()> {
        self.finish()
    }
}

impl ser::SerializeTupleStruct for ProductSerializer<'_, '_> {
    type Ok = ();
    type Error = Error;

    fn serialize_field<V: ?Sized + Serialize>(&mut self, value: &V) -> Result<()> {
        self.serialize_next(value)
    }

    fn end(self) -> Result<()> {
        self.finish()
    }
}

impl ser::SerializeTupleVariant for ProductSerializer<'_, '_> {
    type Ok = ();
    type Error = Error;

    fn serialize_field<V: ?Sized + Serialize>(&mut self, value: &V) -> Result<()> {
        self.serialize_next(value)
    }

    fn end(self) -> Result<()> {
        self.finish()
    }
}

/// Named fields: structs and struct variants. Field order must match the
/// schema (serde derives always emit declaration order).
pub struct StructSerializer<'a, 'c> {
    fields: &'a [(String, SchemaNode)],
    lfields: &'a [LNode],
    columns: &'c mut Vec<Vec<u8>>,
    index: usize,
}

impl StructSerializer<'_, '_> {
    fn serialize_named<V: ?Sized + Serialize>(
        &mut self,
        key: &'static str,
        value: &V,
    ) -> Result<()> {
        let Some(((name, node), lnode)) = self
            .fields
            .get(self.index)
            .zip(self.lfields.get(self.index))
        else {
            return Err(Error::SchemaMismatch {
                expected: "end of struct".to_owned(),
                found: format!("field `{key}`"),
            });
        };
        if name != key {
            return Err(Error::SchemaMismatch {
                expected: format!("field `{name}`"),
                found: format!("field `{key}`"),
            });
        }
        value.serialize(ValueSerializer {
            node,
            lnode,
            columns: self.columns,
        })?;
        self.index += 1;
        Ok(())
    }

    fn finish(self) -> Result<()> {
        if self.index == self.fields.len() {
            Ok(())
        } else {
            Err(Error::IncompleteRow)
        }
    }
}

impl ser::SerializeStruct for StructSerializer<'_, '_> {
    type Ok = ();
    type Error = Error;

    fn serialize_field<V: ?Sized + Serialize>(
        &mut self,
        key: &'static str,
        value: &V,
    ) -> Result<()> {
        self.serialize_named(key, value)
    }

    fn skip_field(&mut self, _key: &'static str) -> Result<()> {
        Err(Error::IncompleteRow)
    }

    fn end(self) -> Result<()> {
        self.finish()
    }
}

impl ser::SerializeStructVariant for StructSerializer<'_, '_> {
    type Ok = ();
    type Error = Error;

    fn serialize_field<V: ?Sized + Serialize>(
        &mut self,
        key: &'static str,
        value: &V,
    ) -> Result<()> {
        self.serialize_named(key, value)
    }

    fn skip_field(&mut self, _key: &'static str) -> Result<()> {
        Err(Error::IncompleteRow)
    }

    fn end(self) -> Result<()> {
        self.finish()
    }
}

pub struct MapSerializer<'a, 'c> {
    key: &'a SchemaNode,
    lkey: &'a LNode,
    value: &'a SchemaNode,
    lvalue: &'a LNode,
    len_col: ColId,
    columns: &'c mut Vec<Vec<u8>>,
    count: u64,
    awaiting_value: bool,
}

impl ser::SerializeMap for MapSerializer<'_, '_> {
    type Ok = ();
    type Error = Error;

    fn serialize_key<V: ?Sized + Serialize>(&mut self, key: &V) -> Result<()> {
        if self.awaiting_value {
            return Err(Error::Message(
                "map key serialized twice in a row".to_owned(),
            ));
        }
        key.serialize(ValueSerializer {
            node: self.key,
            lnode: self.lkey,
            columns: self.columns,
        })?;
        self.awaiting_value = true;
        Ok(())
    }

    fn serialize_value<V: ?Sized + Serialize>(&mut self, value: &V) -> Result<()> {
        if !self.awaiting_value {
            return Err(Error::Message(
                "map value serialized without a key".to_owned(),
            ));
        }
        value.serialize(ValueSerializer {
            node: self.value,
            lnode: self.lvalue,
            columns: self.columns,
        })?;
        self.awaiting_value = false;
        self.count += 1;
        Ok(())
    }

    fn end(self) -> Result<()> {
        if self.awaiting_value {
            return Err(Error::Message("map ended with a dangling key".to_owned()));
        }
        varint::write(&mut self.columns[self.len_col], self.count);
        Ok(())
    }
}

//! Column layout: assigns every leaf in a schema its column indices.
//!
//! Columns are numbered by a pre-order depth-first walk of the schema tree —
//! a node's own columns come before its children's. The serializer and
//! deserializer walk a `SchemaNode` and its `LNode` in lockstep, so `LNode`
//! only stores column ids and structure; names live in the schema.

use crate::schema::{SchemaNode, VariantNode};

pub(crate) type ColId = usize;

#[derive(Debug)]
pub(crate) struct Layout {
    pub(crate) root: LNode,
    /// Total number of columns.
    pub(crate) columns: usize,
}

#[derive(Debug)]
pub(crate) enum LNode {
    /// One fixed-width column.
    Primitive(ColId),
    /// String/Bytes: a varint length column plus a byte-data column.
    Str {
        len: ColId,
        data: ColId,
    },
    /// Unit and unit structs: no columns.
    Unit,
    /// Newtype struct and newtype variant payloads.
    Newtype(Box<LNode>),
    Option {
        tag: ColId,
        inner: Box<LNode>,
    },
    Seq {
        len: ColId,
        elem: Box<LNode>,
        /// Columns occupied by one element. Zero means an element consumes no
        /// bytes, so a claimed length cannot be validated against the input.
        elem_columns: usize,
    },
    Map {
        len: ColId,
        key: Box<LNode>,
        value: Box<LNode>,
        /// Columns occupied by one key/value pair. See `Seq::elem_columns`.
        entry_columns: usize,
    },
    /// Tuple, tuple struct, struct, tuple variant, or struct variant fields.
    Product(Vec<LNode>),
    /// Tag column plus one layout per variant payload.
    Enum {
        tag: ColId,
        variants: Vec<LNode>,
    },
    /// Shared value: key column plus the dictionary's payload columns.
    Shared {
        key: ColId,
        inner: Box<LNode>,
    },
}

impl Layout {
    pub(crate) fn new(schema: &SchemaNode) -> Self {
        let mut next = 0;
        let root = build(schema, &mut next);
        Layout {
            root,
            columns: next,
        }
    }
}

fn alloc(next: &mut usize) -> ColId {
    let id = *next;
    *next += 1;
    id
}

fn build(node: &SchemaNode, next: &mut usize) -> LNode {
    match node {
        SchemaNode::Primitive(_) => LNode::Primitive(alloc(next)),
        SchemaNode::String | SchemaNode::Bytes => LNode::Str {
            len: alloc(next),
            data: alloc(next),
        },
        SchemaNode::Unit | SchemaNode::UnitStruct { .. } => LNode::Unit,
        SchemaNode::NewtypeStruct { inner, .. } => LNode::Newtype(Box::new(build(inner, next))),
        SchemaNode::Option(inner) => LNode::Option {
            tag: alloc(next),
            inner: Box::new(build(inner, next)),
        },
        SchemaNode::Seq(elem) => {
            let len = alloc(next);
            let before = *next;
            let lelem = Box::new(build(elem, next));
            LNode::Seq {
                len,
                elem: lelem,
                elem_columns: *next - before,
            }
        }
        SchemaNode::Map { key, value } => {
            let len = alloc(next);
            let before = *next;
            let lkey = Box::new(build(key, next));
            let lvalue = Box::new(build(value, next));
            LNode::Map {
                len,
                key: lkey,
                value: lvalue,
                entry_columns: *next - before,
            }
        }
        SchemaNode::Tuple(fields) | SchemaNode::TupleStruct { fields, .. } => {
            LNode::Product(fields.iter().map(|field| build(field, next)).collect())
        }
        SchemaNode::Struct { fields, .. } => {
            LNode::Product(fields.iter().map(|(_, field)| build(field, next)).collect())
        }
        SchemaNode::Enum { variants, .. } => {
            let tag = alloc(next);
            let variants = variants
                .iter()
                .map(|(_, variant)| build_variant(variant, next))
                .collect();
            LNode::Enum { tag, variants }
        }
        SchemaNode::Shared(inner) => LNode::Shared {
            key: alloc(next),
            inner: Box::new(build(inner, next)),
        },
    }
}

fn build_variant(variant: &VariantNode, next: &mut usize) -> LNode {
    match variant {
        VariantNode::Unit => LNode::Unit,
        VariantNode::Newtype(inner) => LNode::Newtype(Box::new(build(inner, next))),
        VariantNode::Tuple(fields) => {
            LNode::Product(fields.iter().map(|field| build(field, next)).collect())
        }
        VariantNode::Struct(fields) => {
            LNode::Product(fields.iter().map(|(_, field)| build(field, next)).collect())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::Primitive;

    #[test]
    fn assigns_columns_depth_first() {
        // Vec<(u32, f32)>: seq length column, then one column per tuple field.
        let schema = SchemaNode::Seq(Box::new(SchemaNode::Tuple(vec![
            SchemaNode::Primitive(Primitive::U32),
            SchemaNode::Primitive(Primitive::F32),
        ])));
        let layout = Layout::new(&schema);
        assert_eq!(layout.columns, 3);
        let LNode::Seq {
            len,
            elem,
            elem_columns,
        } = &layout.root
        else {
            panic!("expected seq layout");
        };
        assert_eq!(*len, 0);
        assert_eq!(*elem_columns, 2);
        let LNode::Product(fields) = &**elem else {
            panic!("expected product layout");
        };
        assert!(matches!(fields[0], LNode::Primitive(1)));
        assert!(matches!(fields[1], LNode::Primitive(2)));
    }
}

//! Column layout: the schema tree annotated with column assignments.
//!
//! Columns are numbered by a pre-order depth-first walk of the schema tree —
//! a node's own columns come before its children's. [`Layout::new`] fuses the
//! schema and the numbering into one [`Node`] tree carrying everything the
//! engines need (structure, names, column ids), so the serializer,
//! deserializer, and compat sampler each walk a single tree — no parallel
//! schema/layout walk whose pairing could drift.

use crate::schema::{Primitive, SchemaNode, VariantNode};

pub(crate) type ColId = usize;

#[derive(Debug)]
pub(crate) struct Layout {
    pub(crate) root: Node,
    /// Total number of columns.
    pub(crate) columns: usize,
}

/// One value position: its wire structure, the names the schema recorded for
/// it, and the columns it owns.
#[derive(Debug)]
pub(crate) enum Node {
    /// One fixed-width column.
    Primitive(Primitive, ColId),
    /// String (`bytes: false`) or byte array (`bytes: true`): a varint
    /// length column plus a byte-data column.
    Str {
        bytes: bool,
        len: ColId,
        data: ColId,
    },
    /// `()` (`name: None`) or a unit struct (`name: Some`). No columns.
    Unit {
        name: Option<String>,
    },
    /// A newtype struct; laid out exactly as its inner value.
    Newtype {
        name: String,
        inner: Box<Node>,
    },
    Option {
        tag: ColId,
        inner: Box<Node>,
    },
    Seq {
        len: ColId,
        elem: Box<Node>,
        /// Columns occupied by one element. Zero means an element consumes no
        /// bytes, so a claimed length cannot be validated against the input.
        elem_columns: usize,
    },
    Map {
        len: ColId,
        key: Box<Node>,
        value: Box<Node>,
        /// Columns occupied by one key/value pair. See `Seq::elem_columns`.
        entry_columns: usize,
    },
    /// A bare tuple (`name: None`) or a tuple struct (`name: Some`).
    Tuple {
        name: Option<String>,
        fields: Vec<Node>,
    },
    /// A struct with named fields.
    Struct {
        name: String,
        fields: Vec<(String, Node)>,
    },
    /// Tag column plus one annotated variant per schema variant.
    Enum {
        name: String,
        tag: ColId,
        variants: Vec<Variant>,
    },
    /// Shared value: key column plus the dictionary's payload columns.
    Shared {
        key: ColId,
        inner: Box<Node>,
    },
}

#[derive(Debug)]
pub(crate) struct Variant {
    pub(crate) name: String,
    pub(crate) kind: VariantKind,
}

#[derive(Debug)]
pub(crate) enum VariantKind {
    Unit,
    Newtype(Box<Node>),
    Tuple(Vec<Node>),
    Struct(Vec<(String, Node)>),
}

impl Node {
    /// A short human-readable description, matching what
    /// [`SchemaNode::describe`] says for the node this was built from. Used in
    /// error messages.
    pub(crate) fn describe(&self) -> String {
        match self {
            Node::Primitive(p, _) => p.name().to_owned(),
            Node::Str { bytes: false, .. } => "string".to_owned(),
            Node::Str { bytes: true, .. } => "bytes".to_owned(),
            Node::Unit { name: None } => "unit".to_owned(),
            Node::Unit { name: Some(name) } => format!("unit struct `{name}`"),
            Node::Newtype { name, .. } => format!("newtype struct `{name}`"),
            Node::Option { inner, .. } => format!("option<{}>", inner.describe()),
            Node::Seq { .. } => "sequence".to_owned(),
            Node::Map { .. } => "map".to_owned(),
            Node::Tuple { name: None, fields } => format!("{}-tuple", fields.len()),
            Node::Tuple {
                name: Some(name), ..
            } => format!("tuple struct `{name}`"),
            Node::Struct { name, .. } => format!("struct `{name}`"),
            Node::Enum { name, .. } => format!("enum `{name}`"),
            Node::Shared { inner, .. } => format!("shared<{}>", inner.describe()),
        }
    }
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

fn build(node: &SchemaNode, next: &mut usize) -> Node {
    match node {
        SchemaNode::Primitive(p) => Node::Primitive(*p, alloc(next)),
        SchemaNode::String => Node::Str {
            bytes: false,
            len: alloc(next),
            data: alloc(next),
        },
        SchemaNode::Bytes => Node::Str {
            bytes: true,
            len: alloc(next),
            data: alloc(next),
        },
        SchemaNode::Unit => Node::Unit { name: None },
        SchemaNode::UnitStruct { name } => Node::Unit {
            name: Some(name.clone()),
        },
        SchemaNode::NewtypeStruct { name, inner } => Node::Newtype {
            name: name.clone(),
            inner: Box::new(build(inner, next)),
        },
        SchemaNode::Option(inner) => Node::Option {
            tag: alloc(next),
            inner: Box::new(build(inner, next)),
        },
        SchemaNode::Seq(elem) => {
            let len = alloc(next);
            let before = *next;
            let elem = Box::new(build(elem, next));
            Node::Seq {
                len,
                elem,
                elem_columns: *next - before,
            }
        }
        SchemaNode::Map { key, value } => {
            let len = alloc(next);
            let before = *next;
            let key = Box::new(build(key, next));
            let value = Box::new(build(value, next));
            Node::Map {
                len,
                key,
                value,
                entry_columns: *next - before,
            }
        }
        SchemaNode::Tuple(fields) => Node::Tuple {
            name: None,
            fields: fields.iter().map(|field| build(field, next)).collect(),
        },
        SchemaNode::TupleStruct { name, fields } => Node::Tuple {
            name: Some(name.clone()),
            fields: fields.iter().map(|field| build(field, next)).collect(),
        },
        SchemaNode::Struct { name, fields } => Node::Struct {
            name: name.clone(),
            fields: fields
                .iter()
                .map(|(field_name, field)| (field_name.clone(), build(field, next)))
                .collect(),
        },
        SchemaNode::Enum { name, variants } => {
            let tag = alloc(next);
            let variants = variants
                .iter()
                .map(|(variant_name, variant)| Variant {
                    name: variant_name.clone(),
                    kind: build_variant(variant, next),
                })
                .collect();
            Node::Enum {
                name: name.clone(),
                tag,
                variants,
            }
        }
        SchemaNode::Shared(inner) => Node::Shared {
            key: alloc(next),
            inner: Box::new(build(inner, next)),
        },
    }
}

fn build_variant(variant: &VariantNode, next: &mut usize) -> VariantKind {
    match variant {
        VariantNode::Unit => VariantKind::Unit,
        VariantNode::Newtype(inner) => VariantKind::Newtype(Box::new(build(inner, next))),
        VariantNode::Tuple(fields) => {
            VariantKind::Tuple(fields.iter().map(|field| build(field, next)).collect())
        }
        VariantNode::Struct(fields) => VariantKind::Struct(
            fields
                .iter()
                .map(|(field_name, field)| (field_name.clone(), build(field, next)))
                .collect(),
        ),
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
        let Node::Seq {
            len,
            elem,
            elem_columns,
        } = &layout.root
        else {
            panic!("expected seq layout");
        };
        assert_eq!(*len, 0);
        assert_eq!(*elem_columns, 2);
        let Node::Tuple { name: None, fields } = &**elem else {
            panic!("expected tuple layout");
        };
        assert!(matches!(fields[0], Node::Primitive(Primitive::U32, 1)));
        assert!(matches!(fields[1], Node::Primitive(Primitive::F32, 2)));
    }

    /// The annotated tree must describe itself exactly as the schema it was
    /// built from does, since both feed the same error messages.
    #[test]
    fn descriptions_match_the_schema() {
        for schema in [
            SchemaNode::Primitive(Primitive::I64),
            SchemaNode::String,
            SchemaNode::Bytes,
            SchemaNode::Unit,
            SchemaNode::UnitStruct {
                name: "Marker".to_owned(),
            },
            SchemaNode::NewtypeStruct {
                name: "Meters".to_owned(),
                inner: Box::new(SchemaNode::Primitive(Primitive::F64)),
            },
            SchemaNode::Option(Box::new(SchemaNode::String)),
            SchemaNode::Seq(Box::new(SchemaNode::Primitive(Primitive::U8))),
            SchemaNode::Tuple(vec![SchemaNode::Unit, SchemaNode::Unit]),
            SchemaNode::TupleStruct {
                name: "Pair".to_owned(),
                fields: vec![SchemaNode::Unit],
            },
            SchemaNode::Map {
                key: Box::new(SchemaNode::String),
                value: Box::new(SchemaNode::Unit),
            },
            SchemaNode::Struct {
                name: "Save".to_owned(),
                fields: vec![],
            },
            SchemaNode::Enum {
                name: "State".to_owned(),
                variants: vec![],
            },
            SchemaNode::Shared(Box::new(SchemaNode::String)),
        ] {
            assert_eq!(Layout::new(&schema).root.describe(), schema.describe());
        }
    }
}

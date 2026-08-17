//! Schema discovery by tracing a type's `Deserialize` implementation.
//!
//! [`trace`] drives `T::deserialize` with a recording [`serde::Deserializer`]
//! that feeds synthetic values and notes every request the type makes. Enums
//! are discovered over multiple passes: each pass steers every enum it
//! encounters toward its first unexplored variant (or toward a subtree that
//! still contains one), and passes repeat until no unexplored variants remain.
//!
//! Tracing assumes deserialization is *data-independent*: the shape a type
//! requests must not depend on the values it reads. Types that call
//! `deserialize_any` (untagged/internally tagged enums, `#[serde(flatten)]`,
//! `serde_json::Value`) cannot be traced — nor could they be encoded by a
//! non-self-describing format in the first place.

use serde::de::value::StrDeserializer;
use serde::de::{
    self, DeserializeOwned, DeserializeSeed, EnumAccess, MapAccess, SeqAccess, VariantAccess,
    Visitor,
};

use crate::error::{Error, Result};
use crate::schema::{MAX_DEPTH, Primitive, SchemaNode, VariantNode};

/// Traces `T`'s `Deserialize` impl into a schema tree.
pub(crate) fn trace<T: DeserializeOwned>() -> Result<SchemaNode> {
    let mut merged: Option<TNode> = None;
    let mut passes = 0usize;
    loop {
        let mut out = None;
        T::deserialize(Tracer {
            out: &mut out,
            prior: merged.as_ref(),
            depth: 0,
        })
        .map_err(annotate)?;
        let pass = out
            .ok_or_else(|| untraceable("the Deserialize impl returned without consuming input"))?;
        let next = match merged.take() {
            None => pass,
            Some(prev) => merge(prev, pass)?,
        };
        let done = !node_has_unexplored(&next);
        passes += 1;
        let bound = count_variants(&next) + 1;
        merged = Some(next);
        if done {
            break;
        }
        if passes > bound {
            return Err(untraceable(
                "enum variant exploration did not converge; deserialization appears to be data-dependent",
            ));
        }
    }
    Ok(merged.expect("set in loop").into_schema())
}

fn untraceable(reason: impl Into<String>) -> Error {
    Error::Untraceable {
        reason: reason.into(),
    }
}

/// serde hands the format one entry per `#[serde(alias)]` alongside the real
/// name, in a single flat list, with nothing marking which entry is which. The
/// schema has to record the name the type *writes*, and that name cannot be
/// picked out of the list, so an aliased type is untraceable rather than
/// traceable-but-wrong.
///
/// `#[derive(Schema)]` reads the attributes directly and is unaffected, so it
/// is the way to give such a type a schema.
fn aliased_fields(name: &'static str) -> Error {
    untraceable(format!(
        "`{name}` has a field with #[serde(alias = \"...\")]. serde reports aliases and the real \
         field name in one list, so tracing cannot tell which name the type writes. Put \
         #[derive(carbonite::Schema)] on `{name}` and use StaticSchema::schema() instead of \
         Schema::new()"
    ))
}

/// The variant-level counterpart of [`aliased_fields`].
fn aliased_variants(name: &'static str) -> Error {
    untraceable(format!(
        "`{name}` has a variant with #[serde(alias = \"...\")]. serde reports aliases and the real \
         variant name in one list, so tracing cannot tell which name the type writes. Put \
         #[derive(carbonite::Schema)] on `{name}` and use StaticSchema::schema() instead of \
         Schema::new()"
    ))
}

/// Wraps errors raised *by the type* (e.g. validation of synthetic values)
/// so the failure mode is clear.
fn annotate(err: Error) -> Error {
    match err {
        e @ (Error::Untraceable { .. } | Error::DepthLimitExceeded) => e,
        other => untraceable(format!(
            "the type's Deserialize impl rejected the synthetic trace values: {other}"
        )),
    }
}

// ---------------------------------------------------------------------------
// Trace tree: SchemaNode plus "unexplored variant" holes.
// ---------------------------------------------------------------------------

/// Like [`SchemaNode`], but enum variant shapes may still be unexplored, and
/// names borrow the `&'static str`s serde derive hands out.
#[derive(Debug, PartialEq)]
enum TNode {
    Primitive(Primitive),
    String,
    Bytes,
    Unit,
    Option(Box<TNode>),
    UnitStruct(&'static str),
    NewtypeStruct(&'static str, Box<TNode>),
    Seq(Box<TNode>),
    Tuple(Vec<TNode>),
    TupleStruct(&'static str, Vec<TNode>),
    Map(Box<TNode>, Box<TNode>),
    Struct(&'static str, Vec<(&'static str, TNode)>),
    Enum(&'static str, Vec<(&'static str, Option<TVariant>)>),
    Shared(Box<TNode>),
}

#[derive(Debug, PartialEq)]
enum TVariant {
    Unit,
    Newtype(Box<TNode>),
    Tuple(Vec<TNode>),
    Struct(Vec<(&'static str, TNode)>),
}

impl TNode {
    fn into_schema(self) -> SchemaNode {
        match self {
            TNode::Primitive(p) => SchemaNode::Primitive(p),
            TNode::String => SchemaNode::String,
            TNode::Bytes => SchemaNode::Bytes,
            TNode::Unit => SchemaNode::Unit,
            TNode::Option(inner) => SchemaNode::Option(Box::new(inner.into_schema())),
            TNode::UnitStruct(name) => SchemaNode::UnitStruct {
                name: name.to_owned(),
            },
            TNode::NewtypeStruct(name, inner) => SchemaNode::NewtypeStruct {
                name: name.to_owned(),
                inner: Box::new(inner.into_schema()),
            },
            TNode::Seq(elem) => SchemaNode::Seq(Box::new(elem.into_schema())),
            TNode::Tuple(fields) => {
                SchemaNode::Tuple(fields.into_iter().map(TNode::into_schema).collect())
            }
            TNode::TupleStruct(name, fields) => SchemaNode::TupleStruct {
                name: name.to_owned(),
                fields: fields.into_iter().map(TNode::into_schema).collect(),
            },
            TNode::Map(key, value) => SchemaNode::Map {
                key: Box::new(key.into_schema()),
                value: Box::new(value.into_schema()),
            },
            TNode::Struct(name, fields) => SchemaNode::Struct {
                name: name.to_owned(),
                fields: fields
                    .into_iter()
                    .map(|(field_name, field)| (field_name.to_owned(), field.into_schema()))
                    .collect(),
            },
            TNode::Enum(name, variants) => SchemaNode::Enum {
                name: name.to_owned(),
                variants: variants
                    .into_iter()
                    .map(|(variant_name, shape)| {
                        let shape = shape.expect("all variants explored after convergence");
                        (variant_name.to_owned(), shape.into_schema())
                    })
                    .collect(),
            },
            TNode::Shared(inner) => SchemaNode::Shared(Box::new(inner.into_schema())),
        }
    }
}

impl TVariant {
    fn into_schema(self) -> VariantNode {
        match self {
            TVariant::Unit => VariantNode::Unit,
            TVariant::Newtype(inner) => VariantNode::Newtype(Box::new(inner.into_schema())),
            TVariant::Tuple(fields) => {
                VariantNode::Tuple(fields.into_iter().map(TNode::into_schema).collect())
            }
            TVariant::Struct(fields) => VariantNode::Struct(
                fields
                    .into_iter()
                    .map(|(field_name, field)| (field_name.to_owned(), field.into_schema()))
                    .collect(),
            ),
        }
    }
}

fn node_has_unexplored(node: &TNode) -> bool {
    match node {
        TNode::Primitive(_) | TNode::String | TNode::Bytes | TNode::Unit | TNode::UnitStruct(_) => {
            false
        }
        TNode::Option(inner)
        | TNode::NewtypeStruct(_, inner)
        | TNode::Seq(inner)
        | TNode::Shared(inner) => node_has_unexplored(inner),
        TNode::Tuple(fields) | TNode::TupleStruct(_, fields) => {
            fields.iter().any(node_has_unexplored)
        }
        TNode::Map(key, value) => node_has_unexplored(key) || node_has_unexplored(value),
        TNode::Struct(_, fields) => fields.iter().any(|(_, field)| node_has_unexplored(field)),
        TNode::Enum(_, variants) => variants
            .iter()
            .any(|(_, shape)| shape.as_ref().is_none_or(variant_has_unexplored)),
    }
}

fn variant_has_unexplored(variant: &TVariant) -> bool {
    match variant {
        TVariant::Unit => false,
        TVariant::Newtype(inner) => node_has_unexplored(inner),
        TVariant::Tuple(fields) => fields.iter().any(node_has_unexplored),
        TVariant::Struct(fields) => fields.iter().any(|(_, field)| node_has_unexplored(field)),
    }
}

/// Total enum variants known so far; bounds the number of tracing passes.
fn count_variants(node: &TNode) -> usize {
    match node {
        TNode::Primitive(_) | TNode::String | TNode::Bytes | TNode::Unit | TNode::UnitStruct(_) => {
            0
        }
        TNode::Option(inner)
        | TNode::NewtypeStruct(_, inner)
        | TNode::Seq(inner)
        | TNode::Shared(inner) => count_variants(inner),
        TNode::Tuple(fields) | TNode::TupleStruct(_, fields) => {
            fields.iter().map(count_variants).sum()
        }
        TNode::Map(key, value) => count_variants(key) + count_variants(value),
        TNode::Struct(_, fields) => fields.iter().map(|(_, field)| count_variants(field)).sum(),
        TNode::Enum(_, variants) => {
            variants.len()
                + variants
                    .iter()
                    .filter_map(|(_, shape)| shape.as_ref())
                    .map(|shape| match shape {
                        TVariant::Unit => 0,
                        TVariant::Newtype(inner) => count_variants(inner),
                        TVariant::Tuple(fields) => fields.iter().map(count_variants).sum(),
                        TVariant::Struct(fields) => {
                            fields.iter().map(|(_, field)| count_variants(field)).sum()
                        }
                    })
                    .sum::<usize>()
        }
    }
}

fn mismatch_err() -> Error {
    untraceable(
        "the Deserialize impl requested different shapes on different passes; \
         data-dependent deserialization cannot be traced",
    )
}

/// Unifies two passes' trees. Everything must agree structurally; enum
/// variant shapes fill in wherever either side explored them.
fn merge(a: TNode, b: TNode) -> Result<TNode> {
    Ok(match (a, b) {
        (TNode::Primitive(x), TNode::Primitive(y)) if x == y => TNode::Primitive(x),
        (TNode::String, TNode::String) => TNode::String,
        (TNode::Bytes, TNode::Bytes) => TNode::Bytes,
        (TNode::Unit, TNode::Unit) => TNode::Unit,
        (TNode::Option(x), TNode::Option(y)) => TNode::Option(Box::new(merge(*x, *y)?)),
        (TNode::UnitStruct(n), TNode::UnitStruct(m)) if n == m => TNode::UnitStruct(n),
        (TNode::NewtypeStruct(n, x), TNode::NewtypeStruct(m, y)) if n == m => {
            TNode::NewtypeStruct(n, Box::new(merge(*x, *y)?))
        }
        (TNode::Seq(x), TNode::Seq(y)) => TNode::Seq(Box::new(merge(*x, *y)?)),
        (TNode::Shared(x), TNode::Shared(y)) => TNode::Shared(Box::new(merge(*x, *y)?)),
        (TNode::Tuple(xs), TNode::Tuple(ys)) if xs.len() == ys.len() => {
            TNode::Tuple(merge_all(xs, ys)?)
        }
        (TNode::TupleStruct(n, xs), TNode::TupleStruct(m, ys))
            if n == m && xs.len() == ys.len() =>
        {
            TNode::TupleStruct(n, merge_all(xs, ys)?)
        }
        (TNode::Map(kx, vx), TNode::Map(ky, vy)) => {
            TNode::Map(Box::new(merge(*kx, *ky)?), Box::new(merge(*vx, *vy)?))
        }
        (TNode::Struct(n, xs), TNode::Struct(m, ys)) if n == m && xs.len() == ys.len() => {
            let fields = xs
                .into_iter()
                .zip(ys)
                .map(|((fx, x), (fy, y))| {
                    if fx != fy {
                        return Err(mismatch_err());
                    }
                    Ok((fx, merge(x, y)?))
                })
                .collect::<Result<Vec<_>>>()?;
            TNode::Struct(n, fields)
        }
        (TNode::Enum(n, xs), TNode::Enum(m, ys)) if n == m && xs.len() == ys.len() => {
            let variants = xs
                .into_iter()
                .zip(ys)
                .map(|((vx, sx), (vy, sy))| {
                    if vx != vy {
                        return Err(mismatch_err());
                    }
                    let shape = match (sx, sy) {
                        (None, shape) | (shape, None) => shape,
                        (Some(x), Some(y)) => Some(merge_variant(x, y)?),
                    };
                    Ok((vx, shape))
                })
                .collect::<Result<Vec<_>>>()?;
            TNode::Enum(n, variants)
        }
        _ => return Err(mismatch_err()),
    })
}

fn merge_all(xs: Vec<TNode>, ys: Vec<TNode>) -> Result<Vec<TNode>> {
    xs.into_iter().zip(ys).map(|(x, y)| merge(x, y)).collect()
}

fn merge_variant(a: TVariant, b: TVariant) -> Result<TVariant> {
    Ok(match (a, b) {
        (TVariant::Unit, TVariant::Unit) => TVariant::Unit,
        (TVariant::Newtype(x), TVariant::Newtype(y)) => TVariant::Newtype(Box::new(merge(*x, *y)?)),
        (TVariant::Tuple(xs), TVariant::Tuple(ys)) if xs.len() == ys.len() => {
            TVariant::Tuple(merge_all(xs, ys)?)
        }
        (TVariant::Struct(xs), TVariant::Struct(ys)) if xs.len() == ys.len() => {
            let fields = xs
                .into_iter()
                .zip(ys)
                .map(|((fx, x), (fy, y))| {
                    if fx != fy {
                        return Err(mismatch_err());
                    }
                    Ok((fx, merge(x, y)?))
                })
                .collect::<Result<Vec<_>>>()?;
            TVariant::Struct(fields)
        }
        _ => return Err(mismatch_err()),
    })
}

// ---------------------------------------------------------------------------
// Prior-tree navigation (steering state from previous passes).
// ---------------------------------------------------------------------------

#[derive(Clone, Copy)]
enum Priors<'p> {
    None,
    One(&'p TNode),
    List(&'p [TNode]),
    Named(&'p [(&'static str, TNode)]),
}

impl<'p> Priors<'p> {
    fn get(self, index: usize) -> Option<&'p TNode> {
        match self {
            Priors::None => None,
            Priors::One(node) => (index == 0).then_some(node),
            Priors::List(nodes) => nodes.get(index),
            Priors::Named(fields) => fields.get(index).map(|(_, node)| node),
        }
    }
}

/// Picks which variant this pass should explore at an enum position.
fn choose_variant(prior: Option<&TNode>, variant_count: usize) -> usize {
    let Some(TNode::Enum(_, variants)) = prior else {
        return 0;
    };
    if variants.len() != variant_count {
        return 0;
    }
    variants
        .iter()
        .position(|(_, shape)| shape.is_none())
        .or_else(|| {
            variants
                .iter()
                .position(|(_, shape)| shape.as_ref().is_some_and(variant_has_unexplored))
        })
        .unwrap_or(0)
}

fn prior_variant(prior: Option<&TNode>, index: usize) -> Option<&TVariant> {
    match prior {
        Some(TNode::Enum(_, variants)) => variants.get(index)?.1.as_ref(),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// The tracer itself.
// ---------------------------------------------------------------------------

struct Tracer<'a, 'p> {
    /// Where this value position's discovered node is recorded.
    out: &'a mut Option<TNode>,
    /// The merged tree from previous passes at this position, for steering.
    prior: Option<&'p TNode>,
    depth: usize,
}

impl Tracer<'_, '_> {
    fn deeper(&self) -> Result<usize> {
        let depth = self.depth + 1;
        if depth > MAX_DEPTH {
            Err(Error::DepthLimitExceeded)
        } else {
            Ok(depth)
        }
    }
}

/// Runs a nested trace for one child value position.
fn subtrace<'de, S>(seed: S, prior: Option<&TNode>, depth: usize) -> Result<(S::Value, TNode)>
where
    S: DeserializeSeed<'de>,
{
    let mut out = None;
    let value = seed.deserialize(Tracer {
        out: &mut out,
        prior,
        depth,
    })?;
    let node =
        out.ok_or_else(|| untraceable("a Deserialize impl returned without consuming input"))?;
    Ok((value, node))
}

fn key_deserializer(key: &str) -> StrDeserializer<'_, Error> {
    StrDeserializer::new(key)
}

macro_rules! trace_primitive {
    ($method:ident, $prim:ident, $visit:ident, $value:expr) => {
        fn $method<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value> {
            *self.out = Some(TNode::Primitive(Primitive::$prim));
            visitor.$visit($value)
        }
    };
}

impl<'de> de::Deserializer<'de> for Tracer<'_, '_> {
    type Error = Error;

    fn deserialize_any<V: Visitor<'de>>(self, _visitor: V) -> Result<V::Value> {
        Err(untraceable(
            "the type calls deserialize_any, which requires a self-describing format \
             (untagged or internally tagged enums, serde(flatten), and Value types \
             cannot be encoded columnar)",
        ))
    }

    // Synthetic values are chosen to survive common validation: 1 rather than
    // 0 keeps NonZero* integers alive.
    trace_primitive!(deserialize_bool, Bool, visit_bool, false);
    trace_primitive!(deserialize_i8, I8, visit_i8, 1);
    trace_primitive!(deserialize_i16, I16, visit_i16, 1);
    trace_primitive!(deserialize_i32, I32, visit_i32, 1);
    trace_primitive!(deserialize_i64, I64, visit_i64, 1);
    trace_primitive!(deserialize_i128, I128, visit_i128, 1);
    trace_primitive!(deserialize_u8, U8, visit_u8, 1);
    trace_primitive!(deserialize_u16, U16, visit_u16, 1);
    trace_primitive!(deserialize_u32, U32, visit_u32, 1);
    trace_primitive!(deserialize_u64, U64, visit_u64, 1);
    trace_primitive!(deserialize_u128, U128, visit_u128, 1);
    trace_primitive!(deserialize_f32, F32, visit_f32, 1.0);
    trace_primitive!(deserialize_f64, F64, visit_f64, 1.0);
    trace_primitive!(deserialize_char, Char, visit_char, 'A');

    fn deserialize_str<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value> {
        *self.out = Some(TNode::String);
        visitor.visit_borrowed_str("")
    }

    fn deserialize_string<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value> {
        self.deserialize_str(visitor)
    }

    fn deserialize_bytes<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value> {
        *self.out = Some(TNode::Bytes);
        visitor.visit_borrowed_bytes(b"")
    }

    fn deserialize_byte_buf<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value> {
        self.deserialize_bytes(visitor)
    }

    fn deserialize_option<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value> {
        let depth = self.deeper()?;
        let prior = match self.prior {
            Some(TNode::Option(inner)) => Some(&**inner),
            _ => None,
        };
        let mut inner = None;
        let value = visitor.visit_some(Tracer {
            out: &mut inner,
            prior,
            depth,
        })?;
        let inner =
            inner.ok_or_else(|| untraceable("an Option impl returned without consuming input"))?;
        *self.out = Some(TNode::Option(Box::new(inner)));
        Ok(value)
    }

    fn deserialize_unit<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value> {
        *self.out = Some(TNode::Unit);
        visitor.visit_unit()
    }

    fn deserialize_unit_struct<V: Visitor<'de>>(
        self,
        name: &'static str,
        visitor: V,
    ) -> Result<V::Value> {
        *self.out = Some(TNode::UnitStruct(name));
        visitor.visit_unit()
    }

    fn deserialize_newtype_struct<V: Visitor<'de>>(
        self,
        name: &'static str,
        visitor: V,
    ) -> Result<V::Value> {
        let depth = self.deeper()?;
        // carbonite's shared wrappers announce themselves through a sentinel
        // newtype name; record a Shared node instead of a newtype struct.
        if name == crate::shared::SHARED_TOKEN {
            let prior = match self.prior {
                Some(TNode::Shared(inner)) => Some(&**inner),
                _ => None,
            };
            let mut inner = None;
            let value = visitor.visit_newtype_struct(Tracer {
                out: &mut inner,
                prior,
                depth,
            })?;
            let inner = inner
                .ok_or_else(|| untraceable("a shared impl returned without consuming input"))?;
            *self.out = Some(TNode::Shared(Box::new(inner)));
            return Ok(value);
        }
        let prior = match self.prior {
            Some(TNode::NewtypeStruct(_, inner)) => Some(&**inner),
            _ => None,
        };
        let mut inner = None;
        let value = visitor.visit_newtype_struct(Tracer {
            out: &mut inner,
            prior,
            depth,
        })?;
        let inner =
            inner.ok_or_else(|| untraceable("a newtype impl returned without consuming input"))?;
        *self.out = Some(TNode::NewtypeStruct(name, Box::new(inner)));
        Ok(value)
    }

    fn deserialize_seq<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value> {
        let depth = self.deeper()?;
        let priors = match self.prior {
            Some(TNode::Seq(elem)) => Priors::One(elem),
            _ => Priors::None,
        };
        let mut nodes = Vec::with_capacity(1);
        let value = visitor.visit_seq(TraceSeq {
            nodes: &mut nodes,
            priors,
            limit: 1,
            depth,
        })?;
        let elem = nodes
            .pop()
            .ok_or_else(|| untraceable("a sequence visitor did not read its element"))?;
        *self.out = Some(TNode::Seq(Box::new(elem)));
        Ok(value)
    }

    fn deserialize_tuple<V: Visitor<'de>>(self, len: usize, visitor: V) -> Result<V::Value> {
        let depth = self.deeper()?;
        let priors = match self.prior {
            Some(TNode::Tuple(fields)) => Priors::List(fields),
            _ => Priors::None,
        };
        let mut nodes = Vec::with_capacity(len);
        let value = visitor.visit_seq(TraceSeq {
            nodes: &mut nodes,
            priors,
            limit: len,
            depth,
        })?;
        if nodes.len() != len {
            return Err(untraceable("a tuple visitor did not read every element"));
        }
        *self.out = Some(TNode::Tuple(nodes));
        Ok(value)
    }

    fn deserialize_tuple_struct<V: Visitor<'de>>(
        self,
        name: &'static str,
        len: usize,
        visitor: V,
    ) -> Result<V::Value> {
        let depth = self.deeper()?;
        let priors = match self.prior {
            Some(TNode::TupleStruct(_, fields)) => Priors::List(fields),
            _ => Priors::None,
        };
        let mut nodes = Vec::with_capacity(len);
        let value = visitor.visit_seq(TraceSeq {
            nodes: &mut nodes,
            priors,
            limit: len,
            depth,
        })?;
        if nodes.len() != len {
            return Err(untraceable(
                "a tuple struct visitor did not read every field",
            ));
        }
        *self.out = Some(TNode::TupleStruct(name, nodes));
        Ok(value)
    }

    fn deserialize_map<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value> {
        let depth = self.deeper()?;
        let (key_prior, value_prior) = match self.prior {
            Some(TNode::Map(key, value)) => (Some(&**key), Some(&**value)),
            _ => (None, None),
        };
        let mut key = None;
        let mut value_node = None;
        let value = visitor.visit_map(TraceMap {
            key: &mut key,
            value: &mut value_node,
            key_prior,
            value_prior,
            state: 0,
            depth,
        })?;
        let key = key.ok_or_else(|| untraceable("a map visitor did not read a key"))?;
        let value_node =
            value_node.ok_or_else(|| untraceable("a map visitor did not read a value"))?;
        *self.out = Some(TNode::Map(Box::new(key), Box::new(value_node)));
        Ok(value)
    }

    fn deserialize_struct<V: Visitor<'de>>(
        self,
        name: &'static str,
        fields: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value> {
        let depth = self.deeper()?;
        let priors = match self.prior {
            Some(TNode::Struct(_, prior_fields)) => Priors::Named(prior_fields),
            _ => Priors::None,
        };
        let mut nodes = Vec::with_capacity(fields.len());
        // A key the visitor accepts but then refuses to take a value for is a
        // duplicate field, which is how `#[serde(alias)]` surfaces here; see
        // `aliased_fields`.
        let mut pending = false;
        let value = visitor
            .visit_map(TraceStruct {
                fields,
                nodes: &mut nodes,
                priors,
                pending: &mut pending,
                depth,
            })
            .map_err(|e| if pending { aliased_fields(name) } else { e })?;
        if nodes.len() != fields.len() {
            return Err(untraceable("a struct visitor did not read every field"));
        }
        *self.out = Some(TNode::Struct(
            name,
            fields.iter().copied().zip(nodes).collect(),
        ));
        Ok(value)
    }

    fn deserialize_enum<V: Visitor<'de>>(
        self,
        name: &'static str,
        variants: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value> {
        let depth = self.deeper()?;
        let chosen = choose_variant(self.prior, variants.len());
        let variant_prior = prior_variant(self.prior, chosen);
        let mut shape = None;
        // A variant index the visitor rejects means the list serde handed us
        // is longer than the enum really is, which is how variant aliases
        // surface here; see `aliased_variants`.
        let mut rejected = false;
        let value = visitor
            .visit_enum(TraceEnum {
                name,
                index: chosen,
                shape: &mut shape,
                prior: variant_prior,
                rejected: &mut rejected,
                depth,
            })
            .map_err(|e| if rejected { aliased_variants(name) } else { e })?;
        if shape.is_none() {
            return Err(untraceable("an enum visitor did not inspect its variant"));
        }
        let variant_list = variants
            .iter()
            .enumerate()
            .map(|(i, &variant_name)| (variant_name, if i == chosen { shape.take() } else { None }))
            .collect();
        *self.out = Some(TNode::Enum(name, variant_list));
        Ok(value)
    }

    fn deserialize_identifier<V: Visitor<'de>>(self, _visitor: V) -> Result<V::Value> {
        Err(untraceable(
            "unexpected deserialize_identifier at a value position",
        ))
    }

    fn deserialize_ignored_any<V: Visitor<'de>>(self, _visitor: V) -> Result<V::Value> {
        Err(untraceable(
            "unexpected deserialize_ignored_any while tracing",
        ))
    }

    fn is_human_readable(&self) -> bool {
        false
    }
}

// ---------------------------------------------------------------------------
// Synthetic access objects fed to visitors.
// ---------------------------------------------------------------------------

/// Yields up to `limit` traced elements. Sequences use `limit == 1`; tuples
/// use their arity.
struct TraceSeq<'a, 'p> {
    nodes: &'a mut Vec<TNode>,
    priors: Priors<'p>,
    limit: usize,
    depth: usize,
}

impl<'de> SeqAccess<'de> for TraceSeq<'_, '_> {
    type Error = Error;

    fn next_element_seed<S: DeserializeSeed<'de>>(&mut self, seed: S) -> Result<Option<S::Value>> {
        let index = self.nodes.len();
        if index >= self.limit {
            return Ok(None);
        }
        let (value, node) = subtrace(seed, self.priors.get(index), self.depth)?;
        self.nodes.push(node);
        Ok(Some(value))
    }

    fn size_hint(&self) -> Option<usize> {
        Some(self.limit - self.nodes.len())
    }
}

/// Feeds a struct visitor its own field names as map keys, tracing each value.
struct TraceStruct<'a, 'p> {
    fields: &'static [&'static str],
    nodes: &'a mut Vec<TNode>,
    priors: Priors<'p>,
    /// Set while a key has been handed over but its value not yet requested.
    /// Borrowed so `deserialize_struct` can tell an aliased field apart from
    /// an ordinary failure — see `aliased_fields`.
    pending: &'a mut bool,
    depth: usize,
}

impl<'de> MapAccess<'de> for TraceStruct<'_, '_> {
    type Error = Error;

    fn next_key_seed<S: DeserializeSeed<'de>>(&mut self, seed: S) -> Result<Option<S::Value>> {
        if *self.pending {
            return Err(untraceable("a struct visitor requested two keys in a row"));
        }
        let index = self.nodes.len();
        if index >= self.fields.len() {
            return Ok(None);
        }
        *self.pending = true;
        seed.deserialize(key_deserializer(self.fields[index]))
            .map(Some)
    }

    fn next_value_seed<S: DeserializeSeed<'de>>(&mut self, seed: S) -> Result<S::Value> {
        if !*self.pending {
            return Err(untraceable(
                "a struct visitor requested a value without a key",
            ));
        }
        *self.pending = false;
        let index = self.nodes.len();
        let (value, node) = subtrace(seed, self.priors.get(index), self.depth)?;
        self.nodes.push(node);
        Ok(value)
    }

    fn size_hint(&self) -> Option<usize> {
        Some(self.fields.len() - self.nodes.len())
    }
}

/// Yields exactly one traced map entry.
struct TraceMap<'a, 'p> {
    key: &'a mut Option<TNode>,
    value: &'a mut Option<TNode>,
    key_prior: Option<&'p TNode>,
    value_prior: Option<&'p TNode>,
    /// 0 = before key, 1 = before value, 2 = done.
    state: u8,
    depth: usize,
}

impl<'de> MapAccess<'de> for TraceMap<'_, '_> {
    type Error = Error;

    fn next_key_seed<S: DeserializeSeed<'de>>(&mut self, seed: S) -> Result<Option<S::Value>> {
        match self.state {
            0 => {
                self.state = 1;
                let (value, node) = subtrace(seed, self.key_prior, self.depth)?;
                *self.key = Some(node);
                Ok(Some(value))
            }
            2 => Ok(None),
            _ => Err(untraceable("a map visitor requested two keys in a row")),
        }
    }

    fn next_value_seed<S: DeserializeSeed<'de>>(&mut self, seed: S) -> Result<S::Value> {
        if self.state != 1 {
            return Err(untraceable("a map visitor requested a value without a key"));
        }
        self.state = 2;
        let (value, node) = subtrace(seed, self.value_prior, self.depth)?;
        *self.value = Some(node);
        Ok(value)
    }

    fn size_hint(&self) -> Option<usize> {
        Some(if self.state == 0 { 1 } else { 0 })
    }
}

/// Steers an enum visitor to the chosen variant and records its shape.
struct TraceEnum<'a, 'p> {
    /// The enum's own name, for error messages raised inside a variant.
    name: &'static str,
    index: usize,
    shape: &'a mut Option<TVariant>,
    prior: Option<&'p TVariant>,
    /// Set when the visitor refuses the variant index we steered it to.
    /// Borrowed so `deserialize_enum` can report aliases specifically.
    rejected: &'a mut bool,
    depth: usize,
}

impl<'de> EnumAccess<'de> for TraceEnum<'_, '_> {
    type Error = Error;
    type Variant = Self;

    fn variant_seed<S: DeserializeSeed<'de>>(self, seed: S) -> Result<(S::Value, Self::Variant)> {
        match seed.deserialize(VariantIndexDeserializer {
            index: self.index as u64,
        }) {
            Ok(value) => Ok((value, self)),
            Err(e) => {
                *self.rejected = true;
                Err(e)
            }
        }
    }
}

impl<'de> VariantAccess<'de> for TraceEnum<'_, '_> {
    type Error = Error;

    fn unit_variant(self) -> Result<()> {
        *self.shape = Some(TVariant::Unit);
        Ok(())
    }

    fn newtype_variant_seed<S: DeserializeSeed<'de>>(self, seed: S) -> Result<S::Value> {
        let prior = match self.prior {
            Some(TVariant::Newtype(inner)) => Some(&**inner),
            _ => None,
        };
        let (value, node) = subtrace(seed, prior, self.depth)?;
        *self.shape = Some(TVariant::Newtype(Box::new(node)));
        Ok(value)
    }

    fn tuple_variant<V: Visitor<'de>>(self, len: usize, visitor: V) -> Result<V::Value> {
        let priors = match self.prior {
            Some(TVariant::Tuple(fields)) => Priors::List(fields),
            _ => Priors::None,
        };
        let mut nodes = Vec::with_capacity(len);
        let value = visitor.visit_seq(TraceSeq {
            nodes: &mut nodes,
            priors,
            limit: len,
            depth: self.depth,
        })?;
        if nodes.len() != len {
            return Err(untraceable(
                "a tuple variant visitor did not read every element",
            ));
        }
        *self.shape = Some(TVariant::Tuple(nodes));
        Ok(value)
    }

    fn struct_variant<V: Visitor<'de>>(
        self,
        fields: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value> {
        let priors = match self.prior {
            Some(TVariant::Struct(prior_fields)) => Priors::Named(prior_fields),
            _ => Priors::None,
        };
        let mut nodes = Vec::with_capacity(fields.len());
        let mut pending = false;
        let name = self.name;
        let value = visitor
            .visit_map(TraceStruct {
                fields,
                nodes: &mut nodes,
                priors,
                pending: &mut pending,
                depth: self.depth,
            })
            .map_err(|e| if pending { aliased_fields(name) } else { e })?;
        if nodes.len() != fields.len() {
            return Err(untraceable(
                "a struct variant visitor did not read every field",
            ));
        }
        *self.shape = Some(TVariant::Struct(
            fields.iter().copied().zip(nodes).collect(),
        ));
        Ok(value)
    }
}

/// Hands an enum's identifier seed the steered variant index.
struct VariantIndexDeserializer {
    index: u64,
}

impl<'de> de::Deserializer<'de> for VariantIndexDeserializer {
    type Error = Error;

    fn deserialize_any<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value> {
        visitor.visit_u64(self.index)
    }

    fn deserialize_identifier<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value> {
        visitor.visit_u64(self.index)
    }

    serde::forward_to_deserialize_any! {
        bool i8 i16 i32 i64 i128 u8 u16 u32 u64 u128 f32 f64 char str string
        bytes byte_buf option unit unit_struct newtype_struct seq tuple
        tuple_struct map struct enum ignored_any
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::num::NonZeroU8;

    use serde::Deserialize;

    use super::*;

    #[test]
    fn traces_a_plain_struct() {
        #[derive(Deserialize)]
        #[allow(dead_code)]
        struct Point {
            x: u32,
            y: f32,
        }

        let node = trace::<Vec<Point>>().unwrap();
        assert_eq!(
            node,
            SchemaNode::Seq(Box::new(SchemaNode::Struct {
                name: "Point".to_owned(),
                fields: vec![
                    ("x".to_owned(), SchemaNode::Primitive(Primitive::U32)),
                    ("y".to_owned(), SchemaNode::Primitive(Primitive::F32)),
                ],
            }))
        );
    }

    #[test]
    fn traces_nested_containers() {
        #[derive(Deserialize)]
        #[allow(dead_code)]
        struct Meters(f64);

        #[derive(Deserialize)]
        #[allow(dead_code)]
        struct Wrap {
            name: String,
            data: Vec<u8>,
            lookup: BTreeMap<String, Meters>,
            pair: (bool, Option<char>),
            small: NonZeroU8,
        }

        let node = trace::<Wrap>().unwrap();
        assert_eq!(
            node,
            SchemaNode::Struct {
                name: "Wrap".to_owned(),
                fields: vec![
                    ("name".to_owned(), SchemaNode::String),
                    (
                        "data".to_owned(),
                        SchemaNode::Seq(Box::new(SchemaNode::Primitive(Primitive::U8)))
                    ),
                    (
                        "lookup".to_owned(),
                        SchemaNode::Map {
                            key: Box::new(SchemaNode::String),
                            value: Box::new(SchemaNode::NewtypeStruct {
                                name: "Meters".to_owned(),
                                inner: Box::new(SchemaNode::Primitive(Primitive::F64)),
                            }),
                        }
                    ),
                    (
                        "pair".to_owned(),
                        SchemaNode::Tuple(vec![
                            SchemaNode::Primitive(Primitive::Bool),
                            SchemaNode::Option(Box::new(SchemaNode::Primitive(Primitive::Char))),
                        ])
                    ),
                    ("small".to_owned(), SchemaNode::Primitive(Primitive::U8)),
                ],
            }
        );
    }

    #[test]
    fn explores_every_enum_variant() {
        #[derive(Deserialize)]
        #[allow(dead_code)]
        enum Inner {
            X,
            Y(u8),
        }

        #[derive(Deserialize)]
        #[allow(dead_code)]
        enum Outer {
            A,
            B(Inner),
            C { flag: bool, tail: Vec<Inner> },
        }

        let node = trace::<Outer>().unwrap();
        let inner = SchemaNode::Enum {
            name: "Inner".to_owned(),
            variants: vec![
                ("X".to_owned(), VariantNode::Unit),
                (
                    "Y".to_owned(),
                    VariantNode::Newtype(Box::new(SchemaNode::Primitive(Primitive::U8))),
                ),
            ],
        };
        assert_eq!(
            node,
            SchemaNode::Enum {
                name: "Outer".to_owned(),
                variants: vec![
                    ("A".to_owned(), VariantNode::Unit),
                    (
                        "B".to_owned(),
                        VariantNode::Newtype(Box::new(inner.clone()))
                    ),
                    (
                        "C".to_owned(),
                        VariantNode::Struct(vec![
                            ("flag".to_owned(), SchemaNode::Primitive(Primitive::Bool)),
                            ("tail".to_owned(), SchemaNode::Seq(Box::new(inner))),
                        ])
                    ),
                ],
            }
        );
    }

    #[test]
    fn rejects_untagged_enums() {
        #[derive(Deserialize)]
        #[serde(untagged)]
        #[allow(dead_code)]
        enum Loose {
            Num(u32),
            Text(String),
        }

        assert!(matches!(trace::<Loose>(), Err(Error::Untraceable { .. })));
    }

    #[test]
    fn rejects_recursive_types() {
        #[derive(Deserialize)]
        #[allow(dead_code)]
        struct Node {
            next: Option<Box<Node>>,
        }

        assert!(matches!(trace::<Node>(), Err(Error::DepthLimitExceeded)));
    }

    /// serde reports a field's aliases and its real name in one flat list, so
    /// the name the type *writes* cannot be picked out. The failure has to say
    /// so and name the way out, rather than surfacing serde's own
    /// `duplicate field` from deep inside the trace.
    #[test]
    fn rejects_aliased_fields_with_an_actionable_error() {
        #[derive(Deserialize)]
        #[allow(dead_code)]
        struct Renamed {
            #[serde(alias = "name")]
            title: String,
        }

        let Err(Error::Untraceable { reason }) = trace::<Renamed>() else {
            panic!("expected an Untraceable error");
        };
        assert!(reason.contains("Renamed"), "{reason}");
        assert!(reason.contains("alias"), "{reason}");
        assert!(reason.contains("derive"), "{reason}");
    }

    #[test]
    fn rejects_aliased_variants_with_an_actionable_error() {
        #[derive(Deserialize)]
        #[allow(dead_code)]
        enum Weapon {
            Sword,
            #[serde(alias = "Crossbow")]
            Bow {
                range: u32,
            },
        }

        let Err(Error::Untraceable { reason }) = trace::<Weapon>() else {
            panic!("expected an Untraceable error");
        };
        assert!(reason.contains("Weapon"), "{reason}");
        assert!(reason.contains("alias"), "{reason}");
    }

    /// A field inside a struct variant, which traces through a different path.
    #[test]
    fn rejects_aliased_fields_inside_a_variant() {
        #[derive(Deserialize)]
        #[allow(dead_code)]
        enum Action {
            Apply {
                #[serde(alias = "0")]
                id: u32,
            },
        }

        let Err(Error::Untraceable { reason }) = trace::<Action>() else {
            panic!("expected an Untraceable error");
        };
        assert!(reason.contains("Action"), "{reason}");
        assert!(reason.contains("alias"), "{reason}");
    }
}

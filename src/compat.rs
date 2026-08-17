//! Checking that a type can still read data written by an older version of
//! itself — a test you can run in CI against schemas you shipped.
//!
//! Snapshot the schema at each release and keep the bytes in the repository:
//!
//! ```no_run
//! # use carbonite::Schema;
//! # #[derive(serde::Serialize, serde::Deserialize)] struct Save { id: u32 }
//! std::fs::write("schemas/save-v3.bin", Schema::<Save>::new()?.to_bytes())?;
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```
//!
//! Then assert that today's type still reads every one of them:
//!
//! ```no_run
//! # use carbonite::{Schema, compat};
//! # #[derive(serde::Serialize, serde::Deserialize)] struct Save { id: u32 }
//! for snapshot in ["schemas/save-v1.bin", "schemas/save-v2.bin"] {
//!     let released = Schema::<Save>::from_bytes(&std::fs::read(snapshot)?)?;
//!     compat::check(&released).unwrap_or_else(|e| panic!("{snapshot}: {e}"));
//! }
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```
//!
//! # How the check works, and what that means
//!
//! It does not compare the two schemas. A schema records the names and shapes
//! a writer *wrote*, and nothing about the attributes a reader carries, so a
//! comparison could not tell whether an added field has a `#[serde(default)]`,
//! whether a renamed one has a `#[serde(alias)]`, or whether a field group
//! that became named declared the positions it replaced — the three most
//! common evolutions there are. It would also be a second copy of the
//! reconciliation rules, free to drift from the reader.
//!
//! Instead the check *runs* the reader: it builds a blob of synthetic values
//! conforming to the released schema and deserializes it as `T`. Whatever the
//! real reader accepts, this accepts.
//!
//! What follows from that:
//!
//! - **It answers "does this decode", not "does this still mean the same
//!   thing".** A field that changes units reads perfectly and is still a
//!   silent break. No schema tool catches that.
//! - **Value-dependent changes are out of reach.** The probe writes values
//!   that fit every integer width, so narrowing a field (`u64` to `u32`)
//!   passes here and can still reject real data. Treat narrowing as a change
//!   this check does not cover.
//! - **Enum variants are covered individually, not in combination.** Every
//!   variant of every enum is exercised at least once; two enums are not
//!   tried in every pairing, which would be exponential.
//! - **A type that validates its input cannot be probed.** Synthetic values
//!   mean nothing to a `#[serde(try_from)]` guarding a real invariant, and it
//!   is right to reject them. That is reported as
//!   [`Incompatible::Inconclusive`] rather than as a break — see there for how
//!   it is told apart from a genuine one.
//! - **[`Shared`](crate::Shared) values are written as first occurrences.**
//!   Repeats are not exercised.

use std::fmt;

use serde::de::DeserializeOwned;

use crate::error::Error;
use crate::layout::{LNode, Layout};
use crate::schema::{Primitive, Schema, SchemaNode, VariantNode};
use crate::static_schema::StaticSchema;
use crate::varint;

/// Why a released schema is not readable by the current type.
#[derive(Debug)]
#[non_exhaustive]
pub enum Incompatible {
    /// The type could not read a value written with the released schema.
    /// This is a real break: data in that format is no longer decodable.
    Unreadable(Error),
    /// The probe proves nothing either way, so the result must not be read as
    /// a break — or as an all-clear.
    ///
    /// Two situations reach here. The type may reject synthetic values, which
    /// is what a `#[serde(try_from)]` guarding a real invariant does; that is
    /// established by probing the type against its *own* schema, since a type
    /// that cannot read even that is refusing the values rather than the
    /// shape. Or the type's own schema may be unavailable, which is the case
    /// for anything [`Schema::new`] cannot trace — including every type
    /// carrying a `#[serde(alias)]`, and so every type that has been through a
    /// rename or a shape migration.
    ///
    /// [`check_static`] resolves the second case, taking the reference schema
    /// from `#[derive(Schema)]` instead of from tracing.
    Inconclusive(Error),
}

impl fmt::Display for Incompatible {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Incompatible::Unreadable(e) => {
                write!(f, "the current type cannot read this schema's data: {e}")
            }
            Incompatible::Inconclusive(e) => write!(
                f,
                "compatibility with this schema could not be determined, because the current \
                 type either rejects synthetic values or has no schema of its own to compare \
                 against (try compat::check_static if it derives Schema): {e}"
            ),
        }
    }
}

impl std::error::Error for Incompatible {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Incompatible::Unreadable(e) | Incompatible::Inconclusive(e) => Some(e),
        }
    }
}

/// Checks that `T`, as it is defined now, can read data written with
/// `released`.
///
/// See the [module docs](self) for what the check does and does not cover.
///
/// # Errors
///
/// [`Incompatible::Unreadable`] when the data is no longer decodable, and
/// [`Incompatible::Inconclusive`] when `T` rejects the probe's synthetic
/// values and nothing can be concluded.
pub fn check<T: DeserializeOwned>(released: &Schema<T>) -> Result<(), Incompatible> {
    verify(released, || crate::trace::trace::<T>().ok())
}

/// Like [`check`], but takes the type's own schema from `#[derive(Schema)]`
/// instead of tracing it.
///
/// Tracing cannot see through `#[serde(alias)]`, and a type that has been
/// renamed or has moved a field group from positional to named form carries
/// aliases — exactly the types most worth checking. For those, this reads the
/// reference schema from the derive, so a failure can still be told apart from
/// [`Incompatible::Inconclusive`].
///
/// # Errors
///
/// As [`check`].
pub fn check_static<T: DeserializeOwned + StaticSchema>(
    released: &Schema<T>,
) -> Result<(), Incompatible> {
    verify(released, || Some(T::schema_node()))
}

/// Probes `released`, and on failure probes `T`'s own schema to work out
/// whether the type refuses the values or the shape.
fn verify<T: DeserializeOwned>(
    released: &Schema<T>,
    reference: impl Fn() -> Option<SchemaNode>,
) -> Result<(), Incompatible> {
    let Err(error) = probe::<T>(released.node()) else {
        return Ok(());
    };
    // A type that cannot read a probe of its *own* schema is rejecting the
    // synthetic values rather than this particular shape, and one whose own
    // schema is unavailable cannot be placed either way. Both are reported as
    // inconclusive: only a type that demonstrably reads its own schema and
    // still fails this one has really broken.
    match reference() {
        Some(own) if probe::<T>(&own).is_ok() => Err(Incompatible::Unreadable(error)),
        _ => Err(Incompatible::Inconclusive(error)),
    }
}

/// Builds a blob conforming to `writer` and reads every row of it as `T`.
fn probe<T: DeserializeOwned>(writer: &SchemaNode) -> Result<(), Error> {
    let blob = sample(writer);
    let de = crate::Deserializer::new(Schema::<T>::from_node(writer.clone()));
    for row in de.rows(&blob)? {
        row?;
    }
    Ok(())
}

/// A blob of synthetic rows conforming to `writer`, with enough rows that
/// every enum variant is written at least once.
fn sample(writer: &SchemaNode) -> Vec<u8> {
    let layout = Layout::new(writer);
    let rows = variant_span(writer);
    let mut columns = vec![Vec::new(); layout.columns];
    for pick in 0..rows {
        write_row(writer, &layout.root, &mut columns, pick);
    }

    let mut out = Vec::new();
    varint::write(&mut out, rows as u64);
    varint::write(&mut out, layout.columns as u64);
    for column in &columns {
        varint::write(&mut out, column.len() as u64);
    }
    for column in &columns {
        out.extend_from_slice(column);
    }
    out
}

/// Writes one row into the column buffers. `pick` chooses which variant each
/// enum takes, so cycling it over [`variant_span`] rows covers them all.
fn write_row(node: &SchemaNode, lnode: &LNode, columns: &mut [Vec<u8>], pick: usize) {
    match (node, lnode) {
        (SchemaNode::Primitive(p), LNode::Primitive(col)) => {
            write_primitive(*p, &mut columns[*col]);
        }
        (SchemaNode::String, LNode::Str { len, data }) => {
            varint::write(&mut columns[*len], 1);
            columns[*data].push(b'a');
        }
        (SchemaNode::Bytes, LNode::Str { len, data }) => {
            varint::write(&mut columns[*len], 1);
            columns[*data].push(0);
        }
        (SchemaNode::Unit | SchemaNode::UnitStruct { .. }, LNode::Unit) => {}
        (SchemaNode::NewtypeStruct { inner, .. }, LNode::Newtype(linner)) => {
            write_row(inner, linner, columns, pick);
        }
        (SchemaNode::Option(inner), LNode::Option { tag, inner: linner }) => {
            // Present, so the inner value's own columns are exercised too.
            columns[*tag].push(1);
            write_row(inner, linner, columns, pick);
        }
        (
            SchemaNode::Seq(elem),
            LNode::Seq {
                len, elem: lelem, ..
            },
        ) => {
            varint::write(&mut columns[*len], 1);
            write_row(elem, lelem, columns, pick);
        }
        (
            SchemaNode::Map { key, value },
            LNode::Map {
                len,
                key: lkey,
                value: lvalue,
                ..
            },
        ) => {
            varint::write(&mut columns[*len], 1);
            write_row(key, lkey, columns, pick);
            write_row(value, lvalue, columns, pick);
        }
        (
            SchemaNode::Tuple(fields) | SchemaNode::TupleStruct { fields, .. },
            LNode::Product(lfields),
        ) => {
            for (field, lfield) in fields.iter().zip(lfields) {
                write_row(field, lfield, columns, pick);
            }
        }
        (SchemaNode::Struct { fields, .. }, LNode::Product(lfields)) => {
            for ((_, field), lfield) in fields.iter().zip(lfields) {
                write_row(field, lfield, columns, pick);
            }
        }
        (
            SchemaNode::Enum { variants, .. },
            LNode::Enum {
                tag,
                variants: lvariants,
            },
        ) => {
            let index = pick % variants.len();
            varint::write(&mut columns[*tag], index as u64);
            write_variant(&variants[index].1, &lvariants[index], columns, pick);
        }
        (SchemaNode::Shared(inner), LNode::Shared { key, inner: linner }) => {
            // Key 0 is a first occurrence, so the payload follows it.
            varint::write(&mut columns[*key], 0);
            write_row(inner, linner, columns, pick);
        }
        _ => unreachable!("layout was built from this schema"),
    }
}

fn write_variant(shape: &VariantNode, lnode: &LNode, columns: &mut [Vec<u8>], pick: usize) {
    match (shape, lnode) {
        (VariantNode::Unit, _) => {}
        (VariantNode::Newtype(inner), LNode::Newtype(linner)) => {
            write_row(inner, linner, columns, pick);
        }
        (VariantNode::Tuple(fields), LNode::Product(lfields)) => {
            for (field, lfield) in fields.iter().zip(lfields) {
                write_row(field, lfield, columns, pick);
            }
        }
        (VariantNode::Struct(fields), LNode::Product(lfields)) => {
            for ((_, field), lfield) in fields.iter().zip(lfields) {
                write_row(field, lfield, columns, pick);
            }
        }
        _ => unreachable!("layout was built from this schema"),
    }
}

/// Integers get a value that fits every width, so the probe does not fail a
/// narrowing it has no way to rule out in general — see the module docs.
fn write_primitive(p: Primitive, out: &mut Vec<u8>) {
    match p {
        Primitive::Bool => out.push(1),
        Primitive::Char => out.extend_from_slice(&u32::from('a').to_le_bytes()),
        Primitive::F32 => out.extend_from_slice(&1.0f32.to_le_bytes()),
        Primitive::F64 => out.extend_from_slice(&1.0f64.to_le_bytes()),
        integer => {
            out.push(1);
            out.extend(std::iter::repeat_n(0u8, integer.width() - 1));
        }
    }
}

/// How many rows it takes for every enum in the schema to write each of its
/// variants at least once.
fn variant_span(node: &SchemaNode) -> usize {
    match node {
        SchemaNode::Option(inner)
        | SchemaNode::NewtypeStruct { inner, .. }
        | SchemaNode::Seq(inner)
        | SchemaNode::Shared(inner) => variant_span(inner),
        SchemaNode::Map { key, value } => variant_span(key).max(variant_span(value)),
        SchemaNode::Tuple(fields) | SchemaNode::TupleStruct { fields, .. } => {
            span_of(fields.iter())
        }
        SchemaNode::Struct { fields, .. } => span_of(fields.iter().map(|(_, f)| f)),
        SchemaNode::Enum { variants, .. } => {
            let nested = variants
                .iter()
                .map(|(_, shape)| match shape {
                    VariantNode::Unit => 1,
                    VariantNode::Newtype(inner) => variant_span(inner),
                    VariantNode::Tuple(fields) => span_of(fields.iter()),
                    VariantNode::Struct(fields) => span_of(fields.iter().map(|(_, f)| f)),
                })
                .max()
                .unwrap_or(1);
            variants.len().max(nested)
        }
        _ => 1,
    }
}

fn span_of<'a>(fields: impl Iterator<Item = &'a SchemaNode>) -> usize {
    fields.map(variant_span).max().unwrap_or(1)
}

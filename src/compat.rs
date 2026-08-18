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
//! Two passes, because neither settles the question alone.
//!
//! **Running the reader.** A blob of synthetic values conforming to the
//! released schema is deserialized as `T`. Whatever the real reader accepts,
//! this accepts — including every reconciliation rule, present and future,
//! without restating any of them.
//!
//! This pass is what decides the changes that a comparison cannot, because
//! their answer is not in either schema. Whether an added field has a
//! `#[serde(default)]`, whether a renamed one has a `#[serde(alias)]`, whether
//! a field group that became named declared the positions it replaced: none of
//! that appears in a type's structure. Two structs differing only by
//! `#[serde(default)]` trace to identical field lists.
//!
//! **Comparing the schemas.** One value cannot show that a field's *range*
//! shrank — a `u64` that is now a `u32` reads every number below the new
//! ceiling and rejects the rest — so the two schemas are also walked in
//! parallel and their leaves compared. Findings are reported as
//! [`Incompatible::ValueDependent`].
//!
//! This pass reports only what it can be sure of. Where the trees stop lining
//! up — a field or variant the other side does not name, a shape that changed
//! — it says nothing, because the reader's schema records neither its aliases
//! nor its position tags, and guessing would mean false alarms. It also runs
//! without reading anything, so it still returns a verdict for a type the
//! probe cannot touch.
//!
//! What neither covers:
//!
//! - **They answer "does this decode", not "does this still mean the same
//!   thing".** A field that changes units reads perfectly and is still a
//!   silent break. No schema tool catches that.
//! - **Enum variants are covered individually, not in combination.** Every
//!   variant of every enum is exercised at least once; two enums are not
//!   tried in every pairing, which would be exponential.
//! - **A type that validates its input cannot be probed.** Synthetic values
//!   mean nothing to a `#[serde(try_from)]` guarding a real invariant, and it
//!   is right to reject them. The structural pass still speaks; anything left
//!   over is [`Incompatible::Inconclusive`] rather than a break.
//! - **Narrowing a float is not reported.** serde's `f32` accepts a
//!   `visit_f64`, so it loses precision rather than failing, and that is a
//!   change of meaning rather than of readability.
//! - **[`Shared`](crate::Shared) values are written as first occurrences.**
//!   Repeats are not exercised.

use std::fmt;

use serde::de::DeserializeOwned;

use crate::error::Error;
use crate::layout::{Layout, Node, VariantKind};
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
    /// The type reads *some* values written with the released schema and
    /// rejects others — a field whose range shrank, such as a `u64` that is
    /// now a `u32`. Found by comparing the two schemas rather than by reading
    /// anything, since no single value can demonstrate it.
    ValueDependent(Vec<String>),
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
            Incompatible::ValueDependent(findings) => {
                write!(
                    f,
                    "the current type reads only some of this schema's data: {}",
                    findings.join("; ")
                )
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
            Incompatible::ValueDependent(_) => None,
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

/// Runs both passes: a structural comparison for what values cannot settle,
/// and the probe for what structure cannot.
fn verify<T: DeserializeOwned>(
    released: &Schema<T>,
    reference: impl Fn() -> Option<SchemaNode>,
) -> Result<(), Incompatible> {
    let own = reference();
    // Structural first: it needs no values, so it still returns a verdict for
    // a type the probe cannot touch.
    let narrowed = own
        .as_ref()
        .map(|own| narrowings(released.node(), own))
        .unwrap_or_default();

    let Err(error) = probe::<T>(released.node()) else {
        return if narrowed.is_empty() {
            Ok(())
        } else {
            Err(Incompatible::ValueDependent(narrowed))
        };
    };
    // A type that cannot read a probe of its *own* schema is rejecting the
    // synthetic values rather than this particular shape, and one whose own
    // schema is unavailable cannot be placed either way. Neither is a break —
    // only a type that demonstrably reads its own schema and still fails this
    // one has really broken.
    match own {
        Some(own) if probe::<T>(&own).is_ok() => Err(Incompatible::Unreadable(error)),
        _ if !narrowed.is_empty() => Err(Incompatible::ValueDependent(narrowed)),
        _ => Err(Incompatible::Inconclusive(error)),
    }
}

// ---------------------------------------------------------------------------
// The structural pass.
//
// Values cannot settle whether a field's range still holds: a probe writes one
// number, and a narrowing rejects only the numbers above the new ceiling. So
// the two schemas are walked in parallel and their leaves compared.
//
// It reports only what it can be *sure* of. Where the trees stop lining up —
// a field or variant the other side does not name, a shape that changed — it
// says nothing and leaves the question to the probe, because the reader's
// schema records neither its `#[serde(alias)]`es nor its position tags, and
// guessing there would mean false alarms.
// ---------------------------------------------------------------------------

/// Every leaf where the released schema holds values the current type cannot.
fn narrowings(writer: &SchemaNode, reader: &SchemaNode) -> Vec<String> {
    let mut out = Vec::new();
    walk(writer, reader, &mut String::new(), &mut out);
    out
}

fn walk(writer: &SchemaNode, reader: &SchemaNode, path: &mut String, out: &mut Vec<String>) {
    match (writer, reader) {
        (SchemaNode::Primitive(w), SchemaNode::Primitive(r)) => {
            if !fits(*w, *r) {
                let at = if path.is_empty() {
                    "the value".to_owned()
                } else {
                    format!("`{path}`")
                };
                out.push(format!(
                    "{at} was written as {} and is now {}, which cannot hold every value the \
                     older one could",
                    w.name(),
                    r.name()
                ));
            }
        }
        // A newtype wrapper is invisible on the wire, so see through it on
        // either side; the same goes for a field that has gained an `Option`.
        (
            SchemaNode::NewtypeStruct { inner: w, .. },
            SchemaNode::NewtypeStruct { inner: r, .. },
        ) => walk(w, r, path, out),
        (SchemaNode::NewtypeStruct { inner: w, .. }, r) => walk(w, r, path, out),
        (w, SchemaNode::NewtypeStruct { inner: r, .. }) => walk(w, r, path, out),
        (SchemaNode::Option(w), SchemaNode::Option(r))
        | (SchemaNode::Seq(w), SchemaNode::Seq(r))
        | (SchemaNode::Shared(w), SchemaNode::Shared(r)) => walk(w, r, path, out),
        (w, SchemaNode::Option(r)) => walk(w, r, path, out),
        (SchemaNode::Map { key: wk, value: wv }, SchemaNode::Map { key: rk, value: rv }) => {
            scoped(path, "key", |p| walk(wk, rk, p, out));
            scoped(path, "value", |p| walk(wv, rv, p, out));
        }
        (
            SchemaNode::Tuple(wf) | SchemaNode::TupleStruct { fields: wf, .. },
            SchemaNode::Tuple(rf) | SchemaNode::TupleStruct { fields: rf, .. },
        ) => {
            for (index, (w, r)) in wf.iter().zip(rf).enumerate() {
                scoped(path, &index.to_string(), |p| walk(w, r, p, out));
            }
        }
        (SchemaNode::Struct { fields: wf, .. }, SchemaNode::Struct { fields: rf, .. }) => {
            for (name, w) in wf {
                // A field the reader does not name may have been renamed or
                // dropped; either way there is nothing to compare.
                if let Some((_, r)) = rf.iter().find(|(rname, _)| rname == name) {
                    scoped(path, name, |p| walk(w, r, p, out));
                }
            }
        }
        (SchemaNode::Enum { variants: wv, .. }, SchemaNode::Enum { variants: rv, .. }) => {
            for (name, w) in wv {
                if let Some((_, r)) = rv.iter().find(|(rname, _)| rname == name) {
                    scoped(path, name, |p| walk_variant(w, r, p, out));
                }
            }
        }
        // Anything else has changed shape, which the probe decides.
        _ => {}
    }
}

fn walk_variant(
    writer: &VariantNode,
    reader: &VariantNode,
    path: &mut String,
    out: &mut Vec<String>,
) {
    match (writer, reader) {
        (VariantNode::Newtype(w), VariantNode::Newtype(r)) => walk(w, r, path, out),
        (VariantNode::Tuple(wf), VariantNode::Tuple(rf)) => {
            for (index, (w, r)) in wf.iter().zip(rf).enumerate() {
                scoped(path, &index.to_string(), |p| walk(w, r, p, out));
            }
        }
        (VariantNode::Struct(wf), VariantNode::Struct(rf)) => {
            for (name, w) in wf {
                if let Some((_, r)) = rf.iter().find(|(rname, _)| rname == name) {
                    scoped(path, name, |p| walk(w, r, p, out));
                }
            }
        }
        _ => {}
    }
}

fn scoped(path: &mut String, segment: &str, f: impl FnOnce(&mut String)) {
    let restore = path.len();
    if !path.is_empty() {
        path.push('.');
    }
    path.push_str(segment);
    f(path);
    path.truncate(restore);
}

/// Whether every value the writer's primitive can hold survives being read as
/// the reader's.
///
/// Floats are left alone: serde's `f32` accepts a `visit_f64`, so narrowing one
/// loses precision rather than failing, and this check speaks to what decodes.
fn fits(writer: Primitive, reader: Primitive) -> bool {
    match (integer(writer), integer(reader)) {
        (Some((w_signed, w_bits)), Some((r_signed, r_bits))) => match (w_signed, r_signed) {
            (false, false) | (true, true) => w_bits <= r_bits,
            // An unsigned value needs one more bit to stay positive when read
            // as a signed one; a signed value's negatives never survive.
            (false, true) => w_bits < r_bits,
            (true, false) => false,
        },
        // Anything that is not a pair of integers either matches exactly or is
        // a change of kind, which the probe reports.
        _ => true,
    }
}

/// `(signed, bits)` for the integer primitives.
fn integer(p: Primitive) -> Option<(bool, u32)> {
    Some(match p {
        Primitive::U8 => (false, 8),
        Primitive::U16 => (false, 16),
        Primitive::U32 => (false, 32),
        Primitive::U64 => (false, 64),
        Primitive::U128 => (false, 128),
        Primitive::I8 => (true, 8),
        Primitive::I16 => (true, 16),
        Primitive::I32 => (true, 32),
        Primitive::I64 => (true, 64),
        Primitive::I128 => (true, 128),
        _ => return None,
    })
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
    let rows = variant_span(writer).min(MAX_PROBE_ROWS);
    let mut columns = vec![Vec::new(); layout.columns];
    for pick in 0..rows {
        write_row(&layout.root, &mut columns, pick);
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
/// enum takes; cycling it over [`variant_span`] rows covers them all.
fn write_row(node: &Node, columns: &mut [Vec<u8>], pick: usize) {
    match node {
        Node::Primitive(p, col) => {
            write_primitive(*p, &mut columns[*col]);
        }
        Node::Str {
            bytes: false,
            len,
            data,
        } => {
            varint::write(&mut columns[*len], 1);
            columns[*data].push(b'a');
        }
        Node::Str {
            bytes: true,
            len,
            data,
        } => {
            varint::write(&mut columns[*len], 1);
            columns[*data].push(0);
        }
        Node::Unit { .. } => {}
        Node::Newtype { inner, .. } => {
            write_row(inner, columns, pick);
        }
        Node::Option { tag, inner } => {
            // Present, so the inner value's own columns are exercised too.
            columns[*tag].push(1);
            write_row(inner, columns, pick);
        }
        Node::Seq { len, elem, .. } => {
            varint::write(&mut columns[*len], 1);
            write_row(elem, columns, pick);
        }
        Node::Map {
            len, key, value, ..
        } => {
            varint::write(&mut columns[*len], 1);
            write_row(key, columns, pick);
            write_row(value, columns, pick);
        }
        Node::Tuple { fields, .. } => {
            for field in fields {
                write_row(field, columns, pick);
            }
        }
        Node::Struct { fields, .. } => {
            for (_, field) in fields {
                write_row(field, columns, pick);
            }
        }
        Node::Enum { tag, variants, .. } => {
            // Mixed-radix: this enum consumes the low digit of `pick` and
            // hands the quotient to its payload, so an enum *nested inside a
            // variant* still sees every value of its own digit. Reusing the
            // whole pick below would show the inner enum only picks congruent
            // to this variant's index, skipping inner variants whenever the
            // two counts share a factor.
            let index = pick % variants.len();
            varint::write(&mut columns[*tag], index as u64);
            write_variant(&variants[index].kind, columns, pick / variants.len());
        }
        Node::Shared { key, inner } => {
            // Key 0 is a first occurrence, so the payload follows it.
            varint::write(&mut columns[*key], 0);
            write_row(inner, columns, pick);
        }
    }
}

fn write_variant(kind: &VariantKind, columns: &mut [Vec<u8>], pick: usize) {
    match kind {
        VariantKind::Unit => {}
        VariantKind::Newtype(inner) => {
            write_row(inner, columns, pick);
        }
        VariantKind::Tuple(fields) => {
            for field in fields {
                write_row(field, columns, pick);
            }
        }
        VariantKind::Struct(fields) => {
            for (_, field) in fields {
                write_row(field, columns, pick);
            }
        }
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
///
/// Siblings share each row's pick, so they need the *maximum* of their spans;
/// an enum nested inside a variant sees only the quotient after its parent
/// consumes the low digit (see `write_row`), so nesting *multiplies*. The
/// product grows with enum nesting depth — a chain of enums-inside-variants
/// needs the product of their variant counts — which is small for any real
/// type; [`MAX_PROBE_ROWS`] bounds the pathological case.
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
            variants.len().saturating_mul(nested)
        }
        _ => 1,
    }
}

/// Cap on the probe blob's row count. Only reachable by a schema whose
/// enum-nesting chain multiplies past a million combinations — at which point
/// no row-per-combination probe is tractable, and the check's coverage
/// (already "each variant at least once", not "every combination") degrades
/// to the rows that fit.
const MAX_PROBE_ROWS: usize = 1 << 20;

fn span_of<'a>(fields: impl Iterator<Item = &'a SchemaNode>) -> usize {
    fields.map(variant_span).max().unwrap_or(1)
}

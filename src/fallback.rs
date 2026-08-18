//! The per-field escape hatch: `#[carbonite(serde)]`.
//!
//! `#[derive(Schema)]` needs every field type to implement
//! [`StaticSchema`](crate::StaticSchema) — which the orphan rule makes
//! impossible for a foreign type from a crate that only ships serde impls. A
//! field marked `#[carbonite(serde)]` opts out: the derive takes that field's
//! schema from a **runtime trace** of the field type, and reads and writes it
//! through carbonite's schema-driven serde path, into that field's own slice
//! of the row's columns.
//!
//! Nothing about the encoding changes. The field keeps the columns and the
//! bytes it would have had if the whole containing type had been traced with
//! [`Schema::new`](crate::Schema::new), so:
//!
//! - the derived schema still equals the traced one (the invariant
//!   `tests/derive.rs` enforces);
//! - the schema still describes the field in full, so evolution and
//!   schema-only readers keep working;
//! - the serde path needs no knowledge of the attribute at all — only the
//!   columnar fast path, which cannot generate straight-line code for a type
//!   it knows nothing about, routes through here.
//!
//! Traces are memoized per type in a process-wide registry. Entries are
//! leaked, which bounds the leak by the number of distinct fallback field
//! types in the program (not by calls), and lets a hot loop read the cached
//! schema without touching a reference count.
//!
//! The registry lookup and the serde dispatch this performs are exactly the
//! cost the *whole* containing type used to pay for one foreign field, now
//! confined to that field.

use std::any::{TypeId, type_name};
use std::collections::HashMap;
use std::sync::{OnceLock, PoisonError, RwLock};

use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::columnar::{__split, ColumnCursor};
use crate::error::Result;
use crate::layout::Layout;
use crate::schema::SchemaNode;

/// What a `#[carbonite(serde)]` field type must provide.
///
/// Blanket-implemented, and only ever named by `#[derive(Schema)]` to bound a
/// *generic* parameter that reaches a fallback field. For concrete field types
/// the derive calls the helpers below directly, so a missing impl is reported
/// against the specific serde trait instead.
#[doc(hidden)]
#[diagnostic::on_unimplemented(
    message = "`{Self}` cannot be used for a `#[carbonite(serde)]` field",
    label = "needs `Serialize + DeserializeOwned + 'static`",
    note = "a fallback field is written through serde and its schema is traced from its \
            Deserialize impl at runtime, so it must implement both serde traits and own its data"
)]
pub trait SerdeField: Serialize + DeserializeOwned + 'static {}

impl<T: Serialize + DeserializeOwned + 'static> SerdeField for T {}

/// A memoized trace: the field type's schema, its column layout, and the
/// number of columns it occupies.
struct Traced {
    node: SchemaNode,
    layout: Layout,
    columns: usize,
}

fn registry() -> &'static RwLock<HashMap<TypeId, &'static Traced>> {
    static REGISTRY: OnceLock<RwLock<HashMap<TypeId, &'static Traced>>> = OnceLock::new();
    REGISTRY.get_or_init(RwLock::default)
}

/// Traces `T` once per process.
///
/// # Panics
///
/// Panics if `T` cannot be traced. Traceability is a static property of the
/// type, not of any value, so this fires on the first use of the containing
/// type — the same failure [`Schema::new`](crate::Schema::new) would report on
/// the containing type, surfaced where the field was opted in.
fn traced<T: DeserializeOwned + 'static>() -> &'static Traced {
    let key = TypeId::of::<T>();
    if let Some(hit) = registry()
        .read()
        .unwrap_or_else(PoisonError::into_inner)
        .get(&key)
    {
        return hit;
    }

    // Trace outside the lock: it runs the field type's Deserialize impl, which
    // is arbitrary user code and may itself reach carbonite.
    let node = crate::trace::trace::<T>().unwrap_or_else(|err| {
        panic!(
            "carbonite: the `#[carbonite(serde)]` field type `{}` cannot be traced: {err}\n\
             A fallback field's schema is discovered from its Deserialize impl, so the type \
             must be traceable — untagged or internally tagged enums, `#[serde(flatten)]`, \
             and other `deserialize_any` users are not.",
            type_name::<T>(),
        )
    });
    let layout = Layout::new(&node);
    let entry: &'static Traced = Box::leak(Box::new(Traced {
        columns: layout.columns,
        node,
        layout,
    }));

    // A concurrent tracer may have won the race; keep whichever entry landed
    // first so every caller sees one identical layout.
    registry()
        .write()
        .unwrap_or_else(PoisonError::into_inner)
        .entry(key)
        .or_insert(entry)
}

/// The schema node for a `#[carbonite(serde)]` field. Generated code only.
///
/// # Panics
///
/// If `T` cannot be traced; see [`traced`].
#[doc(hidden)]
#[must_use]
pub fn node<T: DeserializeOwned + 'static>() -> SchemaNode {
    traced::<T>().node.clone()
}

/// How many columns a `#[carbonite(serde)]` field occupies. Generated code
/// only.
///
/// # Panics
///
/// If `T` cannot be traced; see [`traced`].
#[doc(hidden)]
#[must_use]
pub fn columns<T: DeserializeOwned + 'static>() -> usize {
    traced::<T>().columns
}

/// Writes one `#[carbonite(serde)]` field, taking its columns off the front of
/// `rest`. Generated code only.
///
/// # Errors
///
/// Whatever the serde path reports for this value.
///
/// # Panics
///
/// If `T` cannot be traced; see [`traced`].
#[doc(hidden)]
pub fn serialize<T: Serialize + DeserializeOwned + 'static>(
    value: &T,
    rest: &mut &mut [Vec<u8>],
) -> Result<()> {
    let traced = traced::<T>();
    crate::ser::serialize_value(value, &traced.layout.root, __split(rest, traced.columns))
}

/// Reads one `#[carbonite(serde)]` field, taking its columns off the front of
/// `rest`. Generated code only.
///
/// # Errors
///
/// Whatever the serde path reports for this value.
///
/// # Panics
///
/// If `T` cannot be traced; see [`traced`].
#[doc(hidden)]
pub fn deserialize<'de, T: DeserializeOwned + 'static>(
    rest: &mut &mut [ColumnCursor<'de>],
) -> Result<T> {
    let traced = traced::<T>();
    // `fast` is sound here precisely because the layout came from tracing `T`
    // itself: the columnar path only runs when the file schema equals the
    // type's own, so this subtree is positional.
    crate::de::deserialize_value(&traced.layout.root, __split(rest, traced.columns), true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;

    #[derive(Serialize, Deserialize)]
    struct Foreign {
        a: u32,
        b: String,
    }

    #[test]
    fn traces_once_and_memoizes() {
        let first = traced::<Foreign>();
        let second = traced::<Foreign>();
        assert!(
            std::ptr::eq(first, second),
            "the registry must hand back one entry per type"
        );
        assert_eq!(columns::<Foreign>(), 3);
        assert_eq!(node::<Foreign>(), crate::trace::trace::<Foreign>().unwrap());
    }

    #[test]
    #[should_panic(expected = "cannot be traced")]
    fn panics_on_an_untraceable_type() {
        #[derive(Serialize, Deserialize)]
        #[serde(untagged)]
        enum Untraceable {
            A(u32),
            B(String),
        }
        let _ = node::<Untraceable>();
    }
}

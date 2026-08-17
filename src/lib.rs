//! carbonite — schema-separated columnar serialization built on serde.
//!
//! carbonite splits a serialized value into two parts:
//!
//! - a **[`struct@Schema<T>`]**, a serializable description of how `T` is laid out,
//!   discovered at runtime by tracing `T`'s [`serde::Deserialize`] impl —
//!   plain `#[derive(Serialize, Deserialize)]` types work with no extra
//!   derive; and
//! - a **data blob** in a compact columnar encoding: values of the same field
//!   are stored contiguously, so a `Vec<(u32, f32)>` becomes a length column,
//!   all the `u32`s back to back, then all the `f32`s.
//!
//! Because the two are separate, a network protocol can send the schema once
//! and reuse it for every message, while a save file can simply prepend it
//! (the self-describing framing below). Backwards compatibility is handled at
//! read time by reconciling the *writer's* schema with the current type using
//! serde's ordinary evolution rules: fields match by name, added fields need
//! `#[serde(default)]`, removed fields are skipped, `#[serde(alias)]` works,
//! and integer fields may be widened.
//!
//! # Examples
//!
//! ```
//! use serde::{Serialize, Deserialize};
//!
//! #[derive(Serialize, Deserialize, PartialEq, Debug)]
//! struct Star {
//!     name: String,
//!     mass: f64,
//!     planets: Vec<(u32, f32)>,
//! }
//!
//! let star = Star {
//!     name: "Sol".into(),
//!     mass: 1.0,
//!     planets: vec![(3, 1.0), (4, 0.53)],
//! };
//!
//! // Self-describing: the schema travels with the data.
//! let bytes = carbonite::to_vec(&star)?;
//! let back: Star = carbonite::from_slice(&bytes)?;
//! assert_eq!(back, star);
//!
//! // Schema-separated: send the schema once, then lean blobs.
//! use carbonite::{Schema, Serializer, Deserializer};
//!
//! let schema = Schema::<Star>::new()?;
//! let schema_bytes = schema.to_bytes();           // ship this once
//!
//! let ser = Serializer::new(&schema);
//! let blob = ser.to_vec(&star)?;                  // per-message payload
//!
//! let received = Schema::<Star>::from_bytes(&schema_bytes)?;
//! let de = Deserializer::new(received);
//! let back: Star = de.from_slice(&blob)?;
//! assert_eq!(back, star);
//! # Ok::<(), carbonite::Error>(())
//! ```
//!
//! # Compile-time schemas
//!
//! With the `derive` feature (on by default), `#[derive(Schema)]` implements
//! [`StaticSchema`], building the schema at compile time — no tracing pass,
//! and it works even for types that borrow from the input:
//!
//! ```
//! use serde::{Serialize, Deserialize};
//! use carbonite::StaticSchema;
//!
//! #[derive(Serialize, Deserialize, carbonite::Schema, PartialEq, Debug)]
//! struct Pixel {
//!     x: u16,
//!     y: u16,
//!     luma: f32,
//! }
//!
//! let schema = Pixel::schema();
//! // Identical to what tracing discovers — the two are interchangeable.
//! assert_eq!(schema, carbonite::Schema::<Pixel>::new()?);
//! # Ok::<(), carbonite::Error>(())
//! ```
//!
//! The derive also generates a **monomorphized columnar fast path**
//! ([`SerializeColumns`] / [`DeserializeColumns`]): straight-line code that
//! reads and writes the exact same bytes as the serde path, with no schema
//! interpretation per value. Since serialization always targets the type's
//! *current* schema, the columnar writer is the preferred one for derived
//! types; on the read side the columnar path applies when the blob's schema
//! equals the type's own, and the serde path handles everything else
//! (older files, foreign writers — anything needing evolution).
//!
//! Both directions come in single-value and batch form:
//! [`to_vec_columns`](Serializer::to_vec_columns) /
//! [`from_slice_columns`](Deserializer::from_slice_columns) for one value,
//! [`push_columns`](Batch::push_columns) /
//! [`rows_columns`](Deserializer::rows_columns) for many.
//!
//! ```
//! # use serde::{Serialize, Deserialize};
//! # #[derive(Serialize, Deserialize, carbonite::Schema, PartialEq, Debug)]
//! # struct Pixel { x: u16, y: u16, luma: f32 }
//! use carbonite::{Deserializer, Serializer, StaticSchema};
//!
//! let schema = Pixel::schema();
//! let pixel = Pixel { x: 3, y: 4, luma: 0.5 };
//!
//! let blob = Serializer::new(&schema).to_vec_columns(&pixel)?;
//! let back: Pixel = Deserializer::new_static(schema).from_slice_columns(&blob)?;
//! assert_eq!(back, pixel);
//! # Ok::<(), carbonite::Error>(())
//! ```
//!
//! [`to_vec_static`] / [`from_slice_static`] are the self-describing
//! one-shots that take this path, with no tracing pass:
//!
//! ```
//! # use serde::{Serialize, Deserialize};
//! # #[derive(Serialize, Deserialize, carbonite::Schema, PartialEq, Debug)]
//! # struct Pixel { x: u16, y: u16, luma: f32 }
//! let pixel = Pixel { x: 3, y: 4, luma: 0.5 };
//! let bytes = carbonite::to_vec_static(&pixel)?;
//! assert_eq!(carbonite::from_slice_static::<Pixel>(&bytes)?, pixel);
//! # Ok::<(), carbonite::Error>(())
//! ```
//!
//! # Types represented as another type
//!
//! serde's `#[serde(from = "...")]` / `#[serde(into = "...")]` let a type keep
//! invariants its wire form does not carry. The pair can name two *different*
//! shapes, though, and carbonite has one schema for both directions — so
//! carbonite asks for the wire type once, with `#[carbonite(as = "...")]`:
//!
//! ```
//! use serde::{Serialize, Deserialize};
//!
//! #[derive(Serialize, Deserialize, carbonite::Schema, PartialEq, Debug, Clone)]
//! #[serde(try_from = "String", into = "String")]
//! #[carbonite(as = "String")]
//! enum Mode {
//!     Fast,
//!     Small,
//! }
//!
//! impl TryFrom<String> for Mode {
//!     type Error = String;
//!     fn try_from(raw: String) -> Result<Self, String> {
//!         match raw.as_str() {
//!             "fast" => Ok(Mode::Fast),
//!             "small" => Ok(Mode::Small),
//!             other => Err(format!("unknown mode `{other}`")),
//!         }
//!     }
//! }
//!
//! impl From<Mode> for String {
//!     fn from(mode: Mode) -> String {
//!         match mode {
//!             Mode::Fast => "fast".to_owned(),
//!             Mode::Small => "small".to_owned(),
//!         }
//!     }
//! }
//!
//! // The schema, the columns, and the bytes are the repr's: a peer holding
//! // only `String` reads the blob with no idea a conversion happened.
//! let bytes = carbonite::to_vec_static(&Mode::Small)?;
//! assert_eq!(carbonite::from_slice::<String>(&bytes)?, "small");
//! assert_eq!(carbonite::from_slice_static::<Mode>(&bytes)?, Mode::Small);
//! # Ok::<(), carbonite::Error>(())
//! ```
//!
//! The container's own fields never reach the wire, so nothing is traced or
//! interpreted at runtime: reading converts through `TryFrom` (which covers
//! `From` too) and writing through `Into` on a clone, exactly as serde's own
//! codegen does, around the repr's monomorphized columnar path. A failed
//! conversion surfaces the same error on both paths.
//!
//! `as` also *enables* types tracing cannot reach at all. A validating
//! conversion rejects the synthetic values [`Schema::new`] feeds it, so
//! `Schema::<Mode>::new()` fails — and so would tracing any type containing a
//! `Mode`. The derive is unaffected, which makes [`StaticSchema::schema`] and
//! the `_static` entry points the way to use such a type.
//!
//! The attribute states what the type's serde impls *do*; carbonite cannot
//! verify that beyond checking any `serde(from)`/`serde(into)` attributes name
//! the same type, so hand-written impls carry the same
//! [caveat](derive@Schema#the-types-serde-impls-must-match) as always.
//!
//! # Fields whose types come from another crate
//!
//! `#[derive(Schema)]` needs a compile-time schema for every field type, which
//! the orphan rule makes impossible for a foreign type from a crate that only
//! ships serde impls. Mark such a field `#[carbonite(serde)]`: its schema comes
//! from a runtime trace of the field type, and its data goes through the serde
//! path, into that field's own columns.
//!
//! ```
//! use serde::{Serialize, Deserialize};
//!
//! // Imagine this type comes from another crate: serde impls, and no way for
//! // you to add a carbonite impl for it.
//! #[derive(Serialize, Deserialize, PartialEq, Debug)]
//! struct EndpointAddr {
//!     id: [u8; 4],
//!     relay: Option<String>,
//! }
//!
//! #[derive(Serialize, Deserialize, carbonite::Schema, PartialEq, Debug)]
//! struct Peer {
//!     id: u64,
//!     #[carbonite(serde)]
//!     addr: EndpointAddr,
//!     label: String,
//! }
//!
//! let peer = Peer {
//!     id: 7,
//!     addr: EndpointAddr { id: [192, 0, 2, 1], relay: Some("eu-west".to_owned()) },
//!     label: "peer-7".to_owned(),
//! };
//! let bytes = carbonite::to_vec_static(&peer)?;
//! assert_eq!(carbonite::from_slice_static::<Peer>(&bytes)?, peer);
//!
//! // Nothing about the encoding changes: the field keeps the columns and bytes
//! // it would have had if the whole type had been traced, so the derived
//! // schema still equals the traced one.
//! use carbonite::StaticSchema;
//! assert_eq!(Peer::schema(), carbonite::Schema::<Peer>::new()?);
//! # Ok::<(), carbonite::Error>(())
//! ```
//!
//! Because the schema still describes the field in full, evolution and
//! schema-only readers keep working across it, and a reader needs no knowledge
//! that the attribute was used. Only that field pays serde dispatch; the rest
//! of the type keeps the monomorphized path.
//!
//! A fallback field's type must be `DeserializeOwned + 'static` (tracing reads
//! its `Deserialize` impl, so it cannot borrow from the input even though the
//! rest of the type still can) and must be traceable. An untraceable one —
//! untagged enums, `#[serde(flatten)]` — panics the first time the schema is
//! built, since traceability is a property of the type rather than of any
//! value.
//!
//! ## `glam`
//!
//! Math types are the ones a columnar format most wants on the fast path — a
//! `Vec<Vec3>` becomes three contiguous `f32` runs — and the tracing fallback
//! above cannot give them that. So the **`glam` feature** implements
//! [`StaticSchema`], [`SerializeColumns`] and [`DeserializeColumns`] for
//! `glam`'s types directly, where the orphan rule allows it:
//!
#![cfg_attr(
    feature = "glam",
    doc = r#"```
use glam::{Quat, Vec3};
use serde::{Serialize, Deserialize};

#[derive(Serialize, Deserialize, carbonite::Schema, PartialEq, Debug)]
struct Transform {
    translation: Vec3,          // no `#[carbonite(serde)]` needed
    rotation: Quat,
    scale: Vec3,
}

let transform = Transform {
    translation: Vec3::new(1.0, 2.0, 3.0),
    rotation: Quat::IDENTITY,
    scale: Vec3::splat(2.0),
};
let bytes = carbonite::to_vec_static(&transform)?;
assert_eq!(carbonite::from_slice_static::<Transform>(&bytes)?, transform);
# Ok::<(), carbonite::Error>(())
```"#
)]
//!
//! Covered: vectors (`Vec2`/`Vec3`/`Vec3A`/`Vec4` and their `D`/`I`/`U`/`B`
//! siblings, including the sized-integer families), quaternions, matrices,
//! affine transforms, and `EulerRot`. Each reaches the wire exactly as glam's
//! own serde impls describe it — a tuple struct of components, column-major for
//! matrices — so the schema equals a traced one and the bytes are what a peer
//! holding a plain `(f32, f32, f32)` would read.
//!
//! The feature enables glam's `serde` and `all-types` features (`all-types` is
//! glam's own default), since which types exist has to be settled at compile
//! time. `BVec3A` and `BVec4A` are the two exclusions: glam's hand-written
//! impls for them contradict each other, so no schema can serve both
//! directions — use `BVec3`/`BVec4` until that is fixed upstream.
//!
//! # Shared values
//!
//! serde's tree-shaped data model serializes an `Rc`/`Arc` pointee once per
//! handle. [`Shared<T>`] (Rc) and [`SharedArc<T>`] (Arc) restore identity:
//! within a row, each unique object is written once (a key column plus
//! dictionary payload columns), and deserialization reconstructs the sharing
//! so `Shared::ptr_eq` holds after a round trip. Both carbonite paths
//! (serde-driven and columnar) implement the same encoding; in *other* serde
//! formats the wrappers are invisible and duplicate inline, matching stock
//! serde behavior. Sharing is per row; cyclic values error cleanly on read.
//!
//! ```
//! use serde::{Serialize, Deserialize};
//! use carbonite::Shared;
//!
//! #[derive(Serialize, Deserialize, carbonite::Schema, PartialEq, Debug, Clone)]
//! struct Entity {
//!     id: u32,
//!     mesh: Shared<String>,
//! }
//!
//! let mesh = Shared::new("teapot.obj".to_owned());
//! let scene = vec![
//!     Entity { id: 1, mesh: mesh.clone() },
//!     Entity { id: 2, mesh: mesh.clone() },
//! ];
//!
//! let bytes = carbonite::to_vec(&scene)?;   // "teapot.obj" written once
//! let back: Vec<Entity> = carbonite::from_slice(&bytes)?;
//! assert!(Shared::ptr_eq(&back[0].mesh, &back[1].mesh));
//! # Ok::<(), carbonite::Error>(())
//! ```
//!
//! # Untrusted input
//!
//! Deserialization treats every blob as hostile. The header's row count and
//! every length inside the data are validated against the bytes actually
//! present before they size an allocation or drive a loop, so decoding work
//! stays proportional to the input; schemas are depth-limited; varints must
//! be canonically encoded. Malformed input produces an [`Error`], never a
//! panic, an abort, or an unbounded allocation.
//!
//! The one count the data cannot bound is a repetition of a value that
//! occupies no columns — `Vec<()>` and friends, which encode nothing per
//! element. Those are capped at
//! [`MAX_ZERO_COLUMN_REPEAT`](columnar::MAX_ZERO_COLUMN_REPEAT).
//!
//! # Compatibility
//!
//! The wire format is versioned in two places: [`SCHEMA_VERSION`] leads every
//! [`Schema::to_bytes`], and [`FORMAT_VERSION`] leads every self-describing
//! frame. carbonite reads every version up to and including the ones it
//! declares, and rejects newer ones with [`Error::UnsupportedVersion`] rather
//! than misreading them. Blobs written by this release will stay readable.
//!
//! # Limitations
//!
//! The data layer is not self-describing, so serde features that require a
//! self-describing format are unsupported (they fail with a clear error, the
//! same class of restriction as bincode/postcard): `#[serde(untagged)]` and
//! internally/adjacently tagged enums that rely on `deserialize_any`,
//! `#[serde(flatten)]`, and `serde_json::Value`-like types. Recursive types
//! and `#[serde(skip_serializing_if)]` are also rejected — columnar rows must
//! be complete. Adding a field requires `#[serde(default)]` to read old data,
//! exactly as with JSON.
//!
//! Blob bytes are canonical for a given value with one exception:
//! `HashMap`/`HashSet` serialize in iteration order, which varies between
//! runs. Use `BTreeMap`/`BTreeSet` where reproducible bytes matter.

#![warn(missing_docs)]
#![forbid(unsafe_code)]

// Compile the README's examples as doctests so they can never rot.
#[cfg(doctest)]
#[doc = include_str!("../README.md")]
struct ReadmeDoctests;

pub mod columnar;
pub mod compat;
mod de;
mod error;
#[doc(hidden)]
pub mod fallback;
#[cfg(feature = "glam")]
mod glam;
mod layout;
mod schema;
mod self_describing;
mod ser;
mod shared;
mod static_schema;
mod trace;
mod varint;

pub use columnar::{ColumnCursor, DeserializeColumns, SerializeColumns};
pub use de::{Deserializer, Rows, RowsColumns};
pub use error::{Error, Result};
pub use schema::{SCHEMA_VERSION, Schema};
pub use self_describing::{
    FORMAT_VERSION, MAGIC, SelfDescribingDeserializer, SelfDescribingSerializer,
    is_self_describing, peek_schema,
};
pub use ser::{Batch, Serializer};
pub use shared::{Shared, SharedArc};
pub use static_schema::StaticSchema;

#[doc(hidden)]
pub use schema::{Primitive, SchemaNode, VariantNode};

/// Derives the compile-time schema for a type.
///
/// This implements three traits: [`StaticSchema`], giving a schema identical
/// to what runtime tracing would discover, plus [`SerializeColumns`] and
/// [`DeserializeColumns`], the monomorphized columnar fast paths over that
/// schema.
///
/// It honors the serde attributes that affect wire shape (`rename`,
/// `rename_all`, `rename_all_fields`, `skip`, `transparent`) and rejects, at
/// compile time, the ones carbonite cannot represent.
///
/// Three attributes of its own:
///
/// - `#[carbonite(crate = "...")]` on the container points the generated code
///   at a renamed carbonite dependency.
/// - `#[carbonite(as = "Repr")]` on the container declares that the type is
///   [represented as `Repr`](crate#types-represented-as-another-type) on the
///   wire, in both directions — how carbonite supports `#[serde(from)]` /
///   `#[serde(into)]` / `#[serde(try_from)]`.
/// - `#[carbonite(serde)]` on a field takes that field's schema from a runtime
///   trace and routes its data through the serde path, which is what lets a
///   derived type hold [foreign types](crate#fields-whose-types-come-from-another-crate)
///   that only ship serde impls.
///
/// The name is the same namespace trick serde uses: `Schema` is both this
/// derive macro and the [`struct@Schema`] type.
///
/// # The type's serde impls must match
///
/// The derived schema describes how serde's *derived* `Serialize` /
/// `Deserialize` lay the type out. A hand-written serde impl that differs —
/// reordered fields, a different field count, a custom representation —
/// produces a schema that silently disagrees with the serde path. Derive
/// `Serialize`/`Deserialize` alongside `Schema`, or use runtime tracing
/// ([`Schema::new`]), which reads the real `Deserialize` impl.
#[cfg(feature = "derive")]
pub use carbonite_derive::Schema;

use serde::Serialize;
use serde::de::{Deserialize, DeserializeOwned};

/// Serializes `value` into a self-describing blob (schema included).
///
/// Convenience for one-shot use; it traces the schema on every call. When `T`
/// carries `#[derive(Schema)]`, prefer [`to_vec_static`], which skips tracing
/// and takes the columnar fast path. For repeated serialization, build a
/// [`struct@Schema`] once and use [`SelfDescribingSerializer`] or
/// [`Serializer`].
///
/// `T` must implement [`DeserializeOwned`] as well as [`Serialize`] because
/// the schema is discovered through the type's `Deserialize` impl.
///
/// # Errors
///
/// Fails if `T` cannot be traced (see [`Schema::new`]) or serialized.
pub fn to_vec<T>(value: &T) -> Result<Vec<u8>>
where
    T: Serialize + DeserializeOwned,
{
    let schema = Schema::<T>::new()?;
    SelfDescribingSerializer::new(&schema).to_vec(value)
}

/// Serializes `value` into a self-describing blob using `T`'s compile-time
/// schema from `#[derive(Schema)]` — no tracing pass, and the monomorphized
/// columnar writer.
///
/// Produces byte-identical output to [`to_vec`].
///
/// # Errors
///
/// Fails if the value cannot be represented (e.g. a `serde(skip)` enum
/// variant).
pub fn to_vec_static<T>(value: &T) -> Result<Vec<u8>>
where
    T: SerializeColumns + ?Sized,
{
    let schema = T::schema();
    SelfDescribingSerializer::new(&schema).to_vec_columns(value)
}

/// Deserializes a value from a self-describing blob produced by [`to_vec`]
/// (or [`SelfDescribingSerializer`]).
///
/// # Errors
///
/// Fails on malformed input or if the embedded schema cannot be reconciled
/// with `T`.
pub fn from_slice<T>(input: &[u8]) -> Result<T>
where
    T: DeserializeOwned,
{
    SelfDescribingDeserializer::<T>::new().from_slice(input)
}

/// Like [`from_slice`], but uses `T`'s compile-time schema from
/// `#[derive(Schema)]` instead of tracing, so it also works for types that
/// borrow from the input.
///
/// Still reconciles older schemas, exactly as [`from_slice`] does.
///
/// # Errors
///
/// Fails on malformed input or if the embedded schema cannot be reconciled
/// with `T`.
pub fn from_slice_static<'de, T>(input: &'de [u8]) -> Result<T>
where
    T: StaticSchema + Deserialize<'de>,
{
    SelfDescribingDeserializer::<T>::new_static().from_slice(input)
}

//! carbonite — schema-separated columnar serialization built on serde.
//!
//! carbonite splits a serialized value into two parts:
//!
//! - a **[`Schema<T>`]**, a serializable description of how `T` is laid out,
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
//! *current* schema, `to_vec_columns` is the preferred writer for derived
//! types; on the read side the columnar path applies when the blob's schema
//! equals the type's own, and the serde path handles everything else
//! (older files, foreign writers — anything needing evolution):
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

pub mod columnar;
mod de;
mod error;
mod layout;
mod schema;
mod self_describing;
mod ser;
mod shared;
mod static_schema;
mod trace;
mod varint;

pub use columnar::{DeserializeColumns, SerializeColumns};
pub use de::{Deserializer, Rows};
pub use error::{Error, Result};
pub use schema::{Primitive, Schema, SchemaNode, VariantNode};
pub use self_describing::{SelfDescribingDeserializer, SelfDescribingSerializer};
pub use ser::{Batch, Serializer};
pub use shared::{Shared, SharedArc};
pub use static_schema::StaticSchema;

/// Derives [`StaticSchema`]: a compile-time schema identical to what runtime
/// tracing would discover, honoring the serde attributes that affect wire
/// shape (`rename`, `rename_all`, `rename_all_fields`, `skip`,
/// `transparent`). Lives in the same namespace trick serde uses: `Schema` is
/// both this derive macro and the [`Schema`] type.
#[cfg(feature = "derive")]
pub use carbonite_derive::Schema;

use serde::Serialize;
use serde::de::DeserializeOwned;

/// Serializes `value` into a self-describing blob (schema included).
///
/// Convenience for one-shot use; it traces the schema on every call. For
/// repeated serialization, build a [`Schema`] once and use
/// [`SelfDescribingSerializer`] or [`Serializer`].
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
    SelfDescribingDeserializer::<T>::from_slice(input)
}

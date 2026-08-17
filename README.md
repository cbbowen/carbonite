# carbonite

**Schema-separated columnar serialization built on serde.**

carbonite splits a serialized value into two parts:

- a **`Schema<T>`** — a serializable description of how `T` is laid out, obtained either by
  tracing `T`'s existing `Deserialize` impl at runtime or at compile time via
  `#[derive(Schema)]`; and
- a **data blob** in a compact columnar encoding: values of the same field are stored
  contiguously, so a `Vec<(u32, f32)>` becomes a length column, all the `u32`s back to back,
  then all the `f32`s.

Because the two are separate, a network protocol sends the schema **once** and then streams
lean blobs, while a save file simply prepends it. You get the compatibility story of a
self-describing format with the size and speed of a binary one:

- **Evolvable**: old files are reconciled against the current type by field name at read
  time — `#[serde(default)]`, `#[serde(alias)]`, reordering, removed fields, and integer
  widening all just work, with JSON's exact semantics.
- **Compressible**: columnar layout is what compressors love — ~33% smaller than postcard
  after deflate on mixed workloads.
- **Fast**: the derive generates monomorphized column readers/writers (no per-value schema
  interpretation) that trade blows with postcard — faster on string-heavy serialization,
  parity on deserialization.
- **Identity-aware**: `Shared<T>`/`SharedArc<T>` write each unique `Rc`/`Arc` object once
  and rebuild real pointer sharing on read.
- **Safe on untrusted input**: every count a blob claims is validated against the bytes it
  actually carries, so decoding work stays proportional to the input.

## Quick start

Plain serde derives are enough; `#[derive(Schema)]` adds the compile-time fast path.

```rust
use serde::{Serialize, Deserialize};

#[derive(Serialize, Deserialize, carbonite::Schema, PartialEq, Debug)]
struct Star {
    name: String,
    mass: f64,
    planets: Vec<(u32, f32)>,
}

let star = Star { name: "Sol".into(), mass: 1.0, planets: vec![(3, 1.0), (4, 0.53)] };

// Self-describing: the schema travels with the data.
let bytes = carbonite::to_vec(&star).unwrap();
let back: Star = carbonite::from_slice(&bytes).unwrap();
assert_eq!(back, star);
```

## Send the schema once, then stream

```rust
use serde::{Serialize, Deserialize};
use carbonite::{Schema, Serializer, Deserializer, StaticSchema};

#[derive(Serialize, Deserialize, carbonite::Schema, PartialEq, Debug, Clone)]
struct Reading {
    sensor: u16,
    celsius: f32,
}

let readings: Vec<Reading> =
    (0..1000).map(|i| Reading { sensor: i % 4, celsius: 20.0 + (i as f32).sin() }).collect();

// Sender: schema bytes go over the wire once...
let schema = Reading::schema();
let schema_bytes = schema.to_bytes();

// ...then batches share one set of columns.
let ser = Serializer::new(&schema);
let mut batch = ser.batch();
for reading in &readings {
    batch.push_columns(reading).unwrap();   // monomorphized fast path
}
let blob = batch.finish();

// Receiver: rebuild the schema, decode any number of blobs.
let received = Schema::<Reading>::from_bytes(&schema_bytes).unwrap();
let de = Deserializer::new_static(received);
let back: Vec<Reading> = de.rows_columns(&blob).unwrap().collect::<Result<_, _>>().unwrap();
assert_eq!(back, readings);
```

## Old files keep working

The writer's schema rides along (or arrives out of band), and the reader reconciles it with
the *current* type — same rules as JSON:

```rust
use serde::{Serialize, Deserialize};

#[derive(Serialize, Deserialize, carbonite::Schema)]
struct SaveV1 {
    name: String,
    hp: u32,
}

#[derive(Serialize, Deserialize, carbonite::Schema, PartialEq, Debug)]
struct Save {
    #[serde(alias = "name")]
    title: String,                 // renamed
    hp: u64,                       // widened
    #[serde(default)]
    mana: u32,                     // added
}

let old_file = carbonite::to_vec(&SaveV1 { name: "Ada".into(), hp: 90 }).unwrap();
let save: Save = carbonite::from_slice(&old_file).unwrap();
assert_eq!(save, Save { title: "Ada".into(), hp: 90, mana: 0 });
```

## Shared values, written once

```rust
use serde::{Serialize, Deserialize};
use carbonite::Shared;

#[derive(Serialize, Deserialize, carbonite::Schema, PartialEq, Debug, Clone)]
struct Entity {
    id: u32,
    mesh: Shared<String>,          // Rc-backed; SharedArc<T> for Arc
}

let mesh = Shared::new("a-very-large-asset".repeat(100));
let scene: Vec<Entity> = (0..100).map(|id| Entity { id, mesh: mesh.clone() }).collect();

// The asset is serialized once, not 100 times...
let bytes = carbonite::to_vec(&scene).unwrap();
assert!(bytes.len() < 3000);

// ...and the sharing is *reconstructed*, not just the values.
let back: Vec<Entity> = carbonite::from_slice(&bytes).unwrap();
assert!(Shared::ptr_eq(&back[0].mesh, &back[99].mesh));
```

In any other serde format the wrappers are invisible and duplicate inline, exactly like a
plain `Rc`.

## How it works

Every type maps onto the serde data model, and every leaf gets a column: fixed-width
primitives are raw little-endian runs; strings share a length column and a byte column;
sequences add a length column; options a presence column; enums a tag column with dense
per-variant payloads; shared values a key column with a dense dictionary. A blob is just a
tiny header (row count + column byte lengths) followed by the columns back to back.

Two interchangeable engines produce and consume identical bytes:

| | serde-driven | columnar (`#[derive(Schema)]`) |
|---|---|---|
| write | any `Serialize` type | `to_vec_columns` / `push_columns`, monomorphized |
| read | **any schema** — this is the evolution path | exact-schema fast path, `from_slice_columns` / `rows_columns` |

Serialization always targets the current schema, so the columnar writer is the default
choice; the serde reader takes over whenever the file's schema differs from the type's.
`carbonite::to_vec_static` / `from_slice_static` are the self-describing one-shots that
take the fast path without a tracing pass.

## Limitations

The data layer is not self-describing, so serde features that require one are unsupported
and fail with clear errors (the same class of restriction as bincode/postcard):
`#[serde(untagged)]`, internally/adjacently tagged enums, `#[serde(flatten)]`, and
`deserialize_any`-based types. Recursive types and `#[serde(skip_serializing_if)]` are
rejected. Adding a field requires `#[serde(default)]` to read old data. Shared-value
dedup is per row and per field position; cyclic values error on read.

## Compatibility and untrusted input

The wire format is versioned in two places: `SCHEMA_VERSION` leads every `Schema::to_bytes`,
and `FORMAT_VERSION` leads every self-describing frame. carbonite reads every version up to
and including the ones it declares, and rejects newer ones with `Error::UnsupportedVersion`
rather than misreading them. **Blobs written from this release forward will stay readable.**

Deserialization treats every blob as hostile. The header's row count and every length inside
the data are validated against the bytes actually present before they size an allocation or
drive a loop; schemas are depth-limited; varints must be canonically encoded. Malformed input
produces an `Error`, never a panic, an abort, or an unbounded allocation. The one count the
data cannot bound is a repetition of a value that occupies no columns — `Vec<()>` and friends,
which encode nothing per element — so those are capped at `MAX_ZERO_COLUMN_REPEAT`.

Blob bytes are canonical for a given value, with one exception: `HashMap`/`HashSet` serialize
in iteration order, which varies between runs. Use `BTreeMap`/`BTreeSet` where reproducible
bytes matter.

## License

MIT OR Apache-2.0

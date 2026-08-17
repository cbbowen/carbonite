//! Shape evolution: a field group changing between positional and named form.
//!
//! Every non-unit struct, tuple, and variant shape is a product of fields in
//! declaration order, so `Apply(u32)` and `Apply { id: u32 }` describe the same
//! data. carbonite reconciles them, but only when the correspondence is
//! *stated*, because reordering named fields is already a supported no-op and
//! the two steps have to compose:
//!
//! ```text
//! V0(f32, f32)  ->  V1 { x, y }  ->  V2 { y, x }
//! ```
//!
//! If the first step matched by declaration order, a V0 blob would decode into
//! V2 with its values silently swapped. So the named side has to tag its
//! fields with the positions they replace — `#[serde(alias = "0")]` — and the
//! reverse direction, a named payload read into a tuple, is refused outright:
//! a tuple has nowhere to put the tag.
//!
//! One tag opts the whole reader in. Every position the writer stored is then
//! offered to it by index, so an untagged field matches nothing and is
//! reported as missing rather than filled from whatever lined up, and a
//! position the reader no longer names is simply dropped.
//!
//! Note that a type carrying `#[serde(alias)]` cannot be traced (serde reports
//! aliases and real names in one list), so a migrated type needs
//! `#[derive(carbonite::Schema)]` to be *written*. Reading is unaffected.

use serde::{Deserialize, Serialize};

use carbonite::{Deserializer, Schema, Serializer, StaticSchema};

/// Writes `value` with its traced schema, then reads it back as `U`.
fn migrate<T, U>(value: &T) -> Result<U, carbonite::Error>
where
    T: Serialize + serde::de::DeserializeOwned,
    U: serde::de::DeserializeOwned,
{
    let schema = Schema::<T>::new().unwrap();
    reread(
        &schema.to_bytes(),
        &Serializer::new(&schema).to_vec(value).unwrap(),
    )
}

/// The same, for a writer that carries aliases and so cannot be traced.
fn migrate_static<T, U>(value: &T) -> Result<U, carbonite::Error>
where
    T: Serialize + StaticSchema,
    U: serde::de::DeserializeOwned,
{
    let schema = T::schema();
    reread(
        &schema.to_bytes(),
        &Serializer::new(&schema).to_vec(value).unwrap(),
    )
}

fn reread<U: serde::de::DeserializeOwned>(
    schema_bytes: &[u8],
    blob: &[u8],
) -> Result<U, carbonite::Error> {
    let reader = Schema::<U>::from_bytes(schema_bytes).unwrap();
    Deserializer::new(reader).from_slice(blob)
}

fn rejects_shape_change<T, U>(value: &T) -> String
where
    T: Serialize + serde::de::DeserializeOwned,
    U: serde::de::DeserializeOwned + std::fmt::Debug,
{
    match migrate::<T, U>(value) {
        Ok(v) => panic!("expected the shape change to be refused, got {v:?}"),
        Err(e) => e.to_string(),
    }
}

// ---------------------------------------------------------------------------
// The composition property the rule exists to protect.
// ---------------------------------------------------------------------------

#[derive(Serialize, Deserialize, PartialEq, Debug)]
struct V0(f32, f32);

#[derive(Serialize, Deserialize, carbonite::Schema, PartialEq, Debug)]
struct V1 {
    #[serde(alias = "0")]
    x: f32,
    #[serde(alias = "1")]
    y: f32,
}

/// V2 reorders V1's fields, which is a supported no-op.
#[derive(Serialize, Deserialize, carbonite::Schema, PartialEq, Debug)]
struct V2 {
    #[serde(alias = "1")]
    y: f32,
    #[serde(alias = "0")]
    x: f32,
}

#[test]
fn a_shape_change_and_a_later_reorder_compose() {
    // Every later version agrees on which of V0's two values is which...
    assert_eq!(
        migrate::<_, V1>(&V0(1.0, 2.0)).unwrap(),
        V1 { x: 1.0, y: 2.0 }
    );
    assert_eq!(
        migrate::<_, V2>(&V0(1.0, 2.0)).unwrap(),
        V2 { y: 2.0, x: 1.0 }
    );
    // ...and the intermediate step agrees too.
    assert_eq!(
        migrate_static::<_, V2>(&V1 { x: 1.0, y: 2.0 }).unwrap(),
        V2 { y: 2.0, x: 1.0 }
    );
}

/// Without the tags the same migration is refused, rather than quietly
/// depending on declaration order.
#[test]
fn an_untagged_shape_change_is_refused() {
    #[derive(Serialize, Deserialize, PartialEq, Debug)]
    struct Untagged {
        x: f32,
        y: f32,
    }

    let err = rejects_shape_change::<_, Untagged>(&V0(1.0, 2.0));
    assert!(err.contains("alias = \"0\""), "{err}");
    assert!(err.contains("alias = \"1\""), "{err}");
}

/// One tag opts the whole reader in, so a field left untagged matches nothing
/// and is reported — never filled from whichever position happened to line up.
#[test]
fn an_untagged_field_in_a_tagged_reader_is_reported() {
    #[derive(Serialize, Deserialize, PartialEq, Debug)]
    struct HalfTagged {
        #[serde(alias = "1")]
        y: f32,
        x: f32,
    }

    let err = migrate::<_, HalfTagged>(&V0(1.0, 2.0))
        .unwrap_err()
        .to_string();
    assert!(err.contains('x'), "{err}");
}

/// The reverse direction cannot be tagged at all, so it is always refused.
#[test]
fn a_named_payload_is_never_read_into_a_tuple() {
    let err = rejects_shape_change::<_, V0>(&Named { x: 1.0, y: 2.0 });
    assert!(err.contains("nowhere to declare"), "{err}");
}

#[derive(Serialize, Deserialize, PartialEq, Debug)]
struct Named {
    x: f32,
    y: f32,
}

// ---------------------------------------------------------------------------
// Enum variants.
// ---------------------------------------------------------------------------

#[derive(Serialize, Deserialize, PartialEq, Debug)]
enum NewtypeVariant {
    Apply(u32),
}

#[derive(Serialize, Deserialize, PartialEq, Debug)]
enum Tuple2Variant {
    Apply(u32, u32),
}

#[test]
fn a_newtype_variant_becomes_a_struct_variant() {
    #[derive(Serialize, Deserialize, PartialEq, Debug)]
    enum Tagged {
        Apply {
            #[serde(alias = "0")]
            id: u32,
        },
    }
    #[derive(Serialize, Deserialize, PartialEq, Debug)]
    enum Untagged {
        Apply { id: u32 },
    }

    assert_eq!(
        migrate::<_, Tagged>(&NewtypeVariant::Apply(7)).unwrap(),
        Tagged::Apply { id: 7 }
    );
    let err = rejects_shape_change::<_, Untagged>(&NewtypeVariant::Apply(7));
    assert!(err.contains("alias = \"0\""), "{err}");
}

#[test]
fn a_tuple_variant_becomes_a_struct_variant() {
    #[derive(Serialize, Deserialize, PartialEq, Debug)]
    enum Tagged {
        Apply {
            #[serde(alias = "1")]
            y: u32,
            #[serde(alias = "0")]
            x: u32,
        },
    }

    assert_eq!(
        migrate::<_, Tagged>(&Tuple2Variant::Apply(1, 2)).unwrap(),
        Tagged::Apply { y: 2, x: 1 }
    );
}

#[test]
fn a_struct_variant_is_never_read_into_a_tuple_variant() {
    #[derive(Serialize, Deserialize, PartialEq, Debug)]
    enum NamedVariant {
        Apply { x: u32, y: u32 },
    }

    let err = rejects_shape_change::<_, Tuple2Variant>(&NamedVariant::Apply { x: 1, y: 2 });
    assert!(err.contains("nowhere to declare"), "{err}");
    let err = rejects_shape_change::<_, NewtypeVariant>(&NamedVariant::Apply { x: 1, y: 2 });
    assert!(err.contains("nowhere to declare"), "{err}");
}

/// Once both sides have names, matching is by name and order is irrelevant —
/// the step the shape rule has to compose with.
#[test]
fn named_data_matches_by_name_not_position() {
    #[derive(Serialize, Deserialize, PartialEq, Debug)]
    enum Reordered {
        Apply { y: u32, x: u32 },
    }
    #[derive(Serialize, Deserialize, PartialEq, Debug)]
    enum Original {
        Apply { x: u32, y: u32 },
    }

    assert_eq!(
        migrate::<_, Reordered>(&Original::Apply { x: 1, y: 2 }).unwrap(),
        Reordered::Apply { y: 2, x: 1 },
    );
}

// ---------------------------------------------------------------------------
// Structs.
// ---------------------------------------------------------------------------

#[test]
fn a_newtype_struct_becomes_a_struct() {
    #[derive(Serialize, Deserialize, PartialEq, Debug)]
    struct Before(u32);
    #[derive(Serialize, Deserialize, PartialEq, Debug)]
    struct After {
        #[serde(alias = "0")]
        id: u32,
    }

    assert_eq!(migrate::<_, After>(&Before(7)).unwrap(), After { id: 7 });
}

#[test]
fn a_tuple_struct_becomes_a_struct() {
    #[derive(Serialize, Deserialize, PartialEq, Debug)]
    struct Before(u32, u32);
    #[derive(Serialize, Deserialize, PartialEq, Debug)]
    struct After {
        #[serde(alias = "1")]
        y: u32,
        #[serde(alias = "0")]
        x: u32,
    }

    assert_eq!(
        migrate::<_, After>(&Before(1, 2)).unwrap(),
        After { y: 2, x: 1 }
    );
}

/// A field the writer never had still needs a default, tagged or not.
#[test]
fn a_tagged_reader_still_reports_a_missing_field() {
    #[derive(Serialize, Deserialize, PartialEq, Debug)]
    struct Before(u32, u32);
    #[derive(Serialize, Deserialize, PartialEq, Debug)]
    struct After {
        #[serde(alias = "0")]
        x: u32,
        #[serde(alias = "1")]
        y: u32,
        z: u32,
    }

    let err = migrate::<_, After>(&Before(1, 2)).unwrap_err();
    assert!(err.to_string().contains('z'), "unexpected error: {err}");
}

// ---------------------------------------------------------------------------
// Arity. Positional shapes stay positional here, so no tagging is involved.
// ---------------------------------------------------------------------------

/// A positional group gains a field under the same rule a struct does:
/// `#[serde(default)]` on the new one, reported rather than guessed without.
#[test]
fn a_tuple_struct_can_gain_a_field() {
    #[derive(Serialize, Deserialize, PartialEq, Debug)]
    struct Short(u32, f32);
    #[derive(Serialize, Deserialize, PartialEq, Debug)]
    struct Grown(u32, f32, #[serde(default)] u8);
    #[derive(Serialize, Deserialize, PartialEq, Debug)]
    struct GrownWithoutDefault(u32, f32, u8);

    assert_eq!(
        migrate::<_, Grown>(&Short(1, 2.0)).unwrap(),
        Grown(1, 2.0, 0)
    );
    assert!(migrate::<_, GrownWithoutDefault>(&Short(1, 2.0)).is_err());
    assert_eq!(
        migrate::<_, Short>(&Grown(1, 2.0, 3)).unwrap(),
        Short(1, 2.0)
    );
}

/// A *bare* tuple cannot: serde's tuple impl has no per-element attribute to
/// hang a default on, and its visitor requires exactly as many elements as the
/// type has. Shrinking is fine; growing is not, and carbonite cannot make it
/// so — reach for a tuple struct if a positional group needs to grow.
#[test]
fn a_bare_tuple_cannot_gain_a_field() {
    assert_eq!(
        migrate::<(u32, f32, u8), (u32, f32)>(&(1, 2.0, 3)).unwrap(),
        (1, 2.0)
    );
    let err = migrate::<(u32, f32), (u32, f32, u8)>(&(1, 2.0)).unwrap_err();
    assert!(
        err.to_string().contains("invalid length"),
        "unexpected error: {err}"
    );
}

/// A variant's payload grows the same way.
#[test]
fn a_variant_can_gain_a_field() {
    #[derive(Serialize, Deserialize, PartialEq, Debug)]
    enum Grown {
        Apply(u32, #[serde(default)] u32),
    }

    assert_eq!(
        migrate::<_, Grown>(&NewtypeVariant::Apply(7)).unwrap(),
        Grown::Apply(7, 0),
    );
}

// ---------------------------------------------------------------------------
// Dropping fields.
// ---------------------------------------------------------------------------

/// A reader with fewer fields than the writer stops pulling early, but the
/// fields it never asked for still own columns in the row. They have to be
/// advanced past, or every row after the first decodes from a column that is
/// out of step with the rest.
#[test]
fn a_narrower_reader_still_advances_the_writers_columns() {
    #[derive(Serialize, Deserialize, PartialEq, Debug)]
    struct Wide(u32, u32, u32);
    #[derive(Serialize, Deserialize, PartialEq, Debug)]
    struct Narrow(u32, u32);

    let schema = Schema::<Wide>::new().unwrap();
    let ser = Serializer::new(&schema);
    let mut batch = ser.batch();
    for row in [Wide(1, 2, 3), Wide(4, 5, 6), Wide(7, 8, 9)] {
        batch.push(&row).unwrap();
    }
    let blob = batch.finish();

    let reader = Schema::<Narrow>::from_bytes(&schema.to_bytes()).unwrap();
    let rows: Vec<Narrow> = Deserializer::new(reader)
        .rows(&blob)
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert_eq!(rows, vec![Narrow(1, 2), Narrow(4, 5), Narrow(7, 8)]);
}

/// The tagged path drops positions by no longer naming them, and needs no
/// drain: the reader is driven over every one of the writer's fields and
/// ignores the ones it does not claim.
#[test]
fn a_tagged_reader_can_drop_a_position() {
    #[derive(Serialize, Deserialize, PartialEq, Debug)]
    struct Wide(u32, u32, u32);
    #[derive(Serialize, Deserialize, PartialEq, Debug)]
    struct Narrow {
        #[serde(alias = "2")]
        z: u32,
        #[serde(alias = "0")]
        x: u32,
    }

    assert_eq!(
        migrate::<_, Narrow>(&Wide(1, 2, 3)).unwrap(),
        Narrow { z: 3, x: 1 }
    );
}

// ---------------------------------------------------------------------------
// The self-describing path gets the same treatment.
// ---------------------------------------------------------------------------

#[test]
fn self_describing_blobs_follow_the_same_rule() {
    #[derive(Serialize, Deserialize, PartialEq, Debug)]
    enum Tagged {
        Apply {
            #[serde(alias = "0")]
            id: u32,
        },
    }
    #[derive(Serialize, Deserialize, PartialEq, Debug)]
    enum Untagged {
        Apply { id: u32 },
    }

    let bytes = carbonite::to_vec(&NewtypeVariant::Apply(7)).unwrap();
    assert_eq!(
        carbonite::from_slice::<Tagged>(&bytes).unwrap(),
        Tagged::Apply { id: 7 }
    );
    assert!(carbonite::from_slice::<Untagged>(&bytes).is_err());
}

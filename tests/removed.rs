//! `#[carbonite(removed(...))]`: slots a type has retired.
//!
//! Removing a field and adding one are each safe alone, but composing them is
//! not — a new field landing on a removed field's name or position reads the
//! dead column, and where the two types agree the schemas are byte-identical,
//! so nothing downstream can catch it. These tests cover the reader's side of
//! that (what the dead column actually does) and the retirement's effect on
//! the schema; the rejections themselves are in `tests/ui`.
#![cfg(feature = "derive")]

use carbonite::{Deserializer, Schema, Serializer, StaticSchema};
use serde::{Deserialize, Serialize};

fn migrate<T, U>(value: &T) -> Result<U, carbonite::Error>
where
    T: Serialize + serde::de::DeserializeOwned,
    U: serde::de::DeserializeOwned,
{
    let schema = Schema::<T>::new()?;
    let blob = Serializer::new(&schema).to_vec(value)?;
    let reader = Schema::<U>::from_bytes(&schema.to_bytes())?;
    Deserializer::new(reader).from_slice(&blob)
}

#[derive(Serialize, Deserialize)]
#[serde(rename = "Save")]
struct SaveV0 {
    id: u32,
    hp: u32,
}

/// The hazard itself, pinned: without a retirement, a new field that takes a
/// removed one's name reads the old data. `hp` was a level, `hp` is now a
/// fraction, and the number crosses over as serde's numeric coercion — which
/// makes this quiet rather than loud.
#[test]
fn a_reused_name_reads_the_dead_column() {
    #[derive(Serialize, Deserialize, PartialEq, Debug)]
    #[serde(rename = "Save")]
    struct Reused {
        id: u32,
        #[serde(default)]
        hp: f32,
    }

    assert_eq!(
        migrate::<_, Reused>(&SaveV0 { id: 7, hp: 42 }).unwrap(),
        Reused { id: 7, hp: 42.0 }
    );
}

/// Retiring the name is what keeps the new field off it. The new field is
/// named something else, so the dead column is skipped like any other unknown
/// field and the new one takes its default.
#[test]
fn a_retired_name_leaves_the_new_field_alone() {
    #[derive(Serialize, Deserialize, carbonite::Schema, PartialEq, Debug)]
    #[serde(rename = "Save")]
    #[carbonite(removed("hp"))]
    struct Save {
        id: u32,
        #[serde(default)]
        health: f32,
    }

    assert_eq!(
        migrate::<_, Save>(&SaveV0 { id: 7, hp: 42 }).unwrap(),
        Save { id: 7, health: 0.0 }
    );
}

/// A retirement is a compile-time assertion and nothing else: it must not
/// reach the schema, or every reader would have to know about it.
#[test]
fn a_retirement_does_not_reach_the_schema() {
    #[derive(Serialize, Deserialize, carbonite::Schema)]
    #[serde(rename = "Plain")]
    struct Plain {
        id: u32,
    }

    #[derive(Serialize, Deserialize, carbonite::Schema)]
    #[serde(rename = "Plain")]
    #[carbonite(removed("hp", "mana", 3))]
    struct Retired {
        id: u32,
    }

    assert_eq!(Plain::schema().to_bytes(), Retired::schema().to_bytes());
    assert_eq!(Plain::columns(), Retired::columns());
}

/// A `()` placeholder may hold a retired name down, for a type that would
/// rather keep the slot visible than track it in an attribute alone.
#[test]
fn a_placeholder_may_hold_a_retired_name() {
    #[derive(Serialize, Deserialize, carbonite::Schema, PartialEq, Debug)]
    #[serde(rename = "Save")]
    #[carbonite(removed("hp"))]
    struct Save {
        id: u32,
        hp: (),
        #[serde(default)]
        health: f32,
    }

    let read = migrate::<_, Save>(&SaveV0 { id: 7, hp: 42 }).unwrap();
    assert_eq!(
        read,
        Save {
            id: 7,
            hp: (),
            health: 0.0
        }
    );
    // Derived and traced schemas still agree with a placeholder present.
    assert_eq!(
        Save::schema().to_bytes(),
        Schema::<Save>::new().unwrap().to_bytes()
    );
}

#[derive(Serialize, Deserialize)]
#[serde(rename = "Point")]
struct PointV0(f32, u32);

/// Positions have no name to retire, so the slot has to be held: `()`
/// occupies no data columns, and the reader skips whatever the writer put
/// there.
#[test]
fn a_placeholder_holds_a_retired_position() {
    #[derive(Serialize, Deserialize, carbonite::Schema, PartialEq, Debug)]
    #[serde(rename = "Point")]
    #[carbonite(removed(1))]
    struct Point(f32, (), #[serde(default)] f32);

    assert_eq!(
        migrate::<_, Point>(&PointV0(1.5, 42)).unwrap(),
        Point(1.5, (), 0.0)
    );
    assert_eq!(
        Point::schema().to_bytes(),
        Schema::<Point>::new().unwrap().to_bytes()
    );
}

/// A retired position that nothing has grown back into needs no placeholder —
/// the arity simply stops short of it.
#[test]
fn a_retired_position_past_the_arity_is_fine() {
    #[derive(Serialize, Deserialize, carbonite::Schema, PartialEq, Debug)]
    #[serde(rename = "Point")]
    #[carbonite(removed(1))]
    struct Point(f32);

    assert_eq!(migrate::<_, Point>(&PointV0(1.5, 42)).unwrap(), Point(1.5));
}

/// Retirements on an enum name variants, and on a variant name its fields.
#[test]
fn an_enum_retires_variants_and_its_variants_retire_fields() {
    #[derive(Serialize, Deserialize)]
    #[serde(rename = "Weapon")]
    enum WeaponV0 {
        Sword,
        Bow { range: u32 },
    }

    #[derive(Serialize, Deserialize, carbonite::Schema, PartialEq, Debug)]
    #[serde(rename = "Weapon")]
    #[carbonite(removed("Wand"))]
    enum Weapon {
        Sword,
        #[carbonite(removed("range"))]
        Bow {
            #[serde(default)]
            reach: u32,
        },
    }

    assert_eq!(
        migrate::<_, Weapon>(&WeaponV0::Bow { range: 12 }).unwrap(),
        Weapon::Bow { reach: 0 }
    );
    assert_eq!(
        Weapon::schema().to_bytes(),
        Schema::<Weapon>::new().unwrap().to_bytes()
    );
}

/// The retired name is the one that reaches the wire, so `rename_all` applies
/// to the comparison exactly as it does to the schema.
#[test]
fn retirements_are_matched_against_the_wire_name() {
    #[derive(Serialize, Deserialize, carbonite::Schema)]
    #[serde(rename_all = "camelCase")]
    #[carbonite(removed("hitPoints"))]
    struct Save {
        id: u32,
    }

    // `hit_points` would be the ident; `hitPoints` is what the schema records,
    // and what the retirement names.
    assert_eq!(
        Save::schema().to_bytes(),
        Schema::<Save>::new().unwrap().to_bytes()
    );
}

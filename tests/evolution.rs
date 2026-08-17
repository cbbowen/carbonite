//! Schema evolution: data written by one version of a type, read by another.
//!
//! The writer's schema travels with (or alongside) the data; at read time it
//! is reconciled with the current type by field name, giving the same
//! evolution rules as JSON.

use serde::{Deserialize, Serialize};

use carbonite::{Deserializer, Schema, Serializer};

#[derive(Serialize, Deserialize, PartialEq, Debug)]
enum WeaponV1 {
    Sword,
    Bow { range: u32 },
}

#[derive(Serialize, Deserialize, PartialEq, Debug)]
struct SaveV1 {
    id: u32,
    name: String,
    hp: u32,
    inventory: Vec<String>,
    weapon: WeaponV1,
}

fn v1_blob() -> (Vec<u8>, Vec<u8>) {
    let value = SaveV1 {
        id: 7,
        name: "Ada".to_owned(),
        hp: 90,
        inventory: vec!["rope".to_owned(), "torch".to_owned()],
        weapon: WeaponV1::Bow { range: 12 },
    };
    let schema = Schema::<SaveV1>::new().unwrap();
    let blob = Serializer::new(&schema).to_vec(&value).unwrap();
    (schema.to_bytes(), blob)
}

/// Reads a V1 blob as any newer type, mimicking the network/file flow: the
/// writer's schema arrives as bytes and is retyped for the reader.
fn read_v1_as<T: serde::de::DeserializeOwned>() -> T {
    let (schema_bytes, blob) = v1_blob();
    let schema = Schema::<T>::from_bytes(&schema_bytes).unwrap();
    Deserializer::new(schema).from_slice(&blob).unwrap()
}

#[test]
fn added_field_uses_its_default() {
    #[derive(Serialize, Deserialize, PartialEq, Debug)]
    struct SaveV2 {
        id: u32,
        name: String,
        hp: u32,
        #[serde(default = "default_mana")]
        mana: u32,
        inventory: Vec<String>,
        weapon: WeaponV1,
        #[serde(default)]
        motto: Option<String>,
    }
    fn default_mana() -> u32 {
        50
    }

    let v2: SaveV2 = read_v1_as();
    assert_eq!(v2.mana, 50);
    assert_eq!(v2.motto, None);
    assert_eq!(v2.name, "Ada");
    assert_eq!(v2.inventory, vec!["rope".to_owned(), "torch".to_owned()]);
}

#[test]
fn removed_field_is_skipped() {
    // `name` and `inventory` (a variable-width field) are gone.
    #[derive(Serialize, Deserialize, PartialEq, Debug)]
    struct SaveV2 {
        id: u32,
        hp: u32,
        weapon: WeaponV1,
    }

    let v2: SaveV2 = read_v1_as();
    assert_eq!(
        v2,
        SaveV2 {
            id: 7,
            hp: 90,
            weapon: WeaponV1::Bow { range: 12 }
        }
    );
}

#[test]
fn reordered_fields_match_by_name() {
    #[derive(Serialize, Deserialize, PartialEq, Debug)]
    struct SaveV2 {
        hp: u32,
        weapon: WeaponV1,
        name: String,
        id: u32,
        inventory: Vec<String>,
    }

    let v2: SaveV2 = read_v1_as();
    assert_eq!(v2.id, 7);
    assert_eq!(v2.hp, 90);
    assert_eq!(v2.name, "Ada");
}

#[test]
fn renamed_field_needs_an_alias() {
    #[derive(Serialize, Deserialize, PartialEq, Debug)]
    struct SaveV2 {
        id: u32,
        #[serde(alias = "name")]
        title: String,
        hp: u32,
        inventory: Vec<String>,
        weapon: WeaponV1,
    }

    let v2: SaveV2 = read_v1_as();
    assert_eq!(v2.title, "Ada");
}

#[test]
fn widened_integer_field_reads_old_data() {
    #[derive(Serialize, Deserialize, PartialEq, Debug)]
    struct SaveV2 {
        id: u64, // was u32
        name: String,
        hp: u32,
        inventory: Vec<String>,
        weapon: WeaponV1,
    }

    let v2: SaveV2 = read_v1_as();
    assert_eq!(v2.id, 7);
}

#[test]
fn option_wrapped_field_reads_old_data_as_some() {
    #[derive(Serialize, Deserialize, PartialEq, Debug)]
    struct SaveV2 {
        id: u32,
        name: Option<String>, // was String
        hp: u32,
        inventory: Vec<String>,
        weapon: WeaponV1,
    }

    let v2: SaveV2 = read_v1_as();
    assert_eq!(v2.name.as_deref(), Some("Ada"));
}

#[test]
fn enum_with_added_variant_reads_old_data() {
    #[derive(Serialize, Deserialize, PartialEq, Debug)]
    enum WeaponV2 {
        Sword,
        Bow {
            range: u32,
            #[serde(default)]
            poisoned: bool,
        },
        Wand(u8), // new in V2
    }

    #[derive(Serialize, Deserialize, PartialEq, Debug)]
    struct SaveV2 {
        id: u32,
        name: String,
        hp: u32,
        inventory: Vec<String>,
        weapon: WeaponV2,
    }

    let v2: SaveV2 = read_v1_as();
    assert_eq!(
        v2.weapon,
        WeaponV2::Bow {
            range: 12,
            poisoned: false
        }
    );
}

#[test]
fn missing_field_without_default_reports_it() {
    #[derive(Serialize, Deserialize, PartialEq, Debug)]
    struct SaveV2 {
        id: u32,
        name: String,
        hp: u32,
        stamina: u32, // new, but no #[serde(default)]
        inventory: Vec<String>,
        weapon: WeaponV1,
    }

    let (schema_bytes, blob) = v1_blob();
    let schema = Schema::<SaveV2>::from_bytes(&schema_bytes).unwrap();
    let err = Deserializer::new(schema).from_slice(&blob).unwrap_err();
    assert!(
        err.to_string().contains("stamina"),
        "unexpected error: {err}"
    );
}

#[test]
fn self_describing_blobs_evolve_too() {
    #[derive(Serialize, Deserialize, PartialEq, Debug)]
    struct SaveV2 {
        id: u32,
        name: String,
        hp: u32,
        #[serde(default)]
        mana: u32,
        weapon: WeaponV1,
    }

    let value = SaveV1 {
        id: 1,
        name: "Grace".to_owned(),
        hp: 70,
        inventory: vec![],
        weapon: WeaponV1::Sword,
    };
    let bytes = carbonite::to_vec(&value).unwrap();
    let v2: SaveV2 = carbonite::from_slice(&bytes).unwrap();
    assert_eq!(
        v2,
        SaveV2 {
            id: 1,
            name: "Grace".to_owned(),
            hp: 70,
            mana: 0,
            weapon: WeaponV1::Sword
        }
    );
}

// ---------------------------------------------------------------------------
// The properties the README's evolution table claims, one test per row.
//
// Shape changes — a field group moving between positional and named form —
// have their own rule and their own file; see `tests/shape_evolution.rs`.
// ---------------------------------------------------------------------------

/// Writes `value` with its own schema and reads it back as `U`, the way a file
/// or message written by an older build arrives.
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

#[derive(Serialize, Deserialize, PartialEq, Debug)]
struct Field {
    a: u32,
    b: String,
}

fn field() -> Field {
    Field {
        a: 7,
        b: "hi".to_owned(),
    }
}

// --- fields ----------------------------------------------------------------

#[test]
fn a_renamed_field_without_an_alias_is_reported() {
    #[derive(Serialize, Deserialize, PartialEq, Debug)]
    struct Renamed {
        a: u32,
        label: String,
    }

    let err = migrate::<_, Renamed>(&field()).unwrap_err();
    assert!(err.to_string().contains("label"), "{err}");
}

/// Widening is always safe. Narrowing is **value-dependent**: it succeeds for
/// values that fit and fails for those that do not, so a narrowing change can
/// pass every test and still reject production data.
#[test]
fn narrowing_an_integer_depends_on_the_value() {
    #[derive(Serialize, Deserialize, PartialEq, Debug)]
    struct Narrow {
        a: u8,
        b: String,
    }

    assert_eq!(
        migrate::<_, Narrow>(&field()).unwrap(),
        Narrow {
            a: 7,
            b: "hi".to_owned()
        }
    );
    let err = migrate::<_, Narrow>(&Field {
        a: 999,
        b: "hi".to_owned(),
    })
    .unwrap_err();
    assert!(err.to_string().contains("999"), "{err}");
}

/// Wrapping in `Option` is a one-way door: old data reads as `Some`, but an
/// `Option` file cannot be read back into a bare field, even where it holds a
/// value.
#[test]
fn unwrapping_an_option_is_not_supported() {
    #[derive(Serialize, Deserialize, PartialEq, Debug)]
    struct Optioned {
        a: Option<u32>,
        b: String,
    }

    assert!(
        migrate::<_, Field>(&Optioned {
            a: Some(7),
            b: "hi".to_owned()
        })
        .is_err()
    );
}

#[test]
fn changing_a_fields_type_is_reported() {
    #[derive(Serialize, Deserialize, PartialEq, Debug)]
    struct Retyped {
        a: String,
        b: String,
    }

    assert!(migrate::<_, Retyped>(&field()).is_err());
}

/// Forward compatibility: a build that predates a field still reads data
/// written with it, because the extra column is skipped.
#[test]
fn an_older_reader_skips_a_field_it_does_not_know() {
    #[derive(Serialize, Deserialize, PartialEq, Debug)]
    struct Extended {
        a: u32,
        b: String,
        c: u8,
    }

    assert_eq!(
        migrate::<_, Field>(&Extended {
            a: 7,
            b: "hi".to_owned(),
            c: 1
        })
        .unwrap(),
        field()
    );
}

/// Wrapping a field's type in a newtype struct is a no-op on the wire — a
/// `NewtypeStruct` occupies exactly its inner value's columns — but is not
/// currently reconciled in either direction.
#[test]
fn wrapping_a_field_in_a_newtype_is_not_supported() {
    #[derive(Serialize, Deserialize, PartialEq, Debug)]
    struct Meters(u32);
    #[derive(Serialize, Deserialize, PartialEq, Debug)]
    struct Plain {
        d: u32,
    }
    #[derive(Serialize, Deserialize, PartialEq, Debug)]
    struct Wrapped {
        d: Meters,
    }

    assert!(migrate::<_, Wrapped>(&Plain { d: 5 }).is_err());
    assert!(migrate::<_, Plain>(&Wrapped { d: Meters(5) }).is_err());
}

#[test]
fn a_unit_struct_and_an_empty_struct_are_not_interchangeable() {
    #[derive(Serialize, Deserialize, PartialEq, Debug)]
    struct Unit;
    #[derive(Serialize, Deserialize, PartialEq, Debug)]
    struct Empty {}

    assert!(migrate::<_, Empty>(&Unit).is_err());
    assert!(migrate::<_, Unit>(&Empty {}).is_err());
}

// --- enum variants ---------------------------------------------------------

#[derive(Serialize, Deserialize, PartialEq, Debug)]
enum Tool {
    Sword,
    Bow { range: u32 },
}

/// The tag on the wire indexes the *writer's* variant list and is resolved to
/// a name before matching, so where a variant sits is not part of the
/// contract: one can be inserted anywhere, including ahead of existing ones.
#[test]
fn a_variant_can_be_added_anywhere_in_the_list() {
    #[derive(Serialize, Deserialize, PartialEq, Debug)]
    enum AddedFirst {
        Wand(u8),
        Sword,
        Bow { range: u32 },
    }

    assert_eq!(
        migrate::<_, AddedFirst>(&Tool::Sword).unwrap(),
        AddedFirst::Sword
    );
    assert_eq!(
        migrate::<_, AddedFirst>(&Tool::Bow { range: 12 }).unwrap(),
        AddedFirst::Bow { range: 12 }
    );
}

#[test]
fn variants_can_be_reordered() {
    #[derive(Serialize, Deserialize, PartialEq, Debug)]
    enum Reordered {
        Bow { range: u32 },
        Sword,
    }

    assert_eq!(
        migrate::<_, Reordered>(&Tool::Sword).unwrap(),
        Reordered::Sword
    );
    assert_eq!(
        migrate::<_, Reordered>(&Tool::Bow { range: 12 }).unwrap(),
        Reordered::Bow { range: 12 }
    );
}

#[test]
fn a_renamed_variant_needs_an_alias() {
    #[derive(Serialize, Deserialize, PartialEq, Debug)]
    enum NoAlias {
        Blade,
        Bow { range: u32 },
    }
    #[derive(Serialize, Deserialize, PartialEq, Debug)]
    enum WithAlias {
        #[serde(alias = "Sword")]
        Blade,
        Bow {
            range: u32,
        },
    }

    assert!(migrate::<_, NoAlias>(&Tool::Sword).is_err());
    assert_eq!(
        migrate::<_, WithAlias>(&Tool::Sword).unwrap(),
        WithAlias::Blade
    );
}

/// Removing a variant only breaks the data that actually used it.
#[test]
fn a_removed_variant_only_affects_its_own_values() {
    #[derive(Serialize, Deserialize, PartialEq, Debug)]
    enum Fewer {
        Bow { range: u32 },
    }

    assert_eq!(
        migrate::<_, Fewer>(&Tool::Bow { range: 12 }).unwrap(),
        Fewer::Bow { range: 12 }
    );
    let err = migrate::<_, Fewer>(&Tool::Sword).unwrap_err();
    assert!(err.to_string().contains("Sword"), "{err}");
}

/// The counterpart of skipping an unknown *field*: an unknown *variant* has no
/// meaning to fall back on, so it is reported.
#[test]
fn an_older_reader_reports_a_variant_it_does_not_know() {
    #[derive(Serialize, Deserialize, PartialEq, Debug)]
    enum More {
        Sword,
        Bow { range: u32 },
        Wand(u8),
    }

    assert_eq!(migrate::<_, Tool>(&More::Sword).unwrap(), Tool::Sword);
    let err = migrate::<_, Tool>(&More::Wand(3)).unwrap_err();
    assert!(err.to_string().contains("Wand"), "{err}");
}

/// A variant may gain or lose *fields*, but not change between having a
/// payload and not having one: a unit variant stores no columns to read a
/// payload from, and `#[serde(default)]` has no say in it.
#[test]
fn a_variant_cannot_gain_or_lose_its_payload() {
    #[derive(Serialize, Deserialize, PartialEq, Debug)]
    enum Unit {
        A,
        B,
    }
    #[derive(Serialize, Deserialize, PartialEq, Debug)]
    enum Newtype {
        A(u8),
        B,
    }
    #[derive(Serialize, Deserialize, PartialEq, Debug)]
    enum Defaulted {
        A(#[serde(default)] u8),
        B,
    }

    assert!(migrate::<_, Newtype>(&Unit::A).is_err());
    assert!(migrate::<_, Defaulted>(&Unit::A).is_err());
    assert!(migrate::<_, Unit>(&Newtype::A(3)).is_err());
}

// --- containers ------------------------------------------------------------

#[test]
fn a_sequence_element_evolves_like_any_other_value() {
    #[derive(Serialize, Deserialize, PartialEq, Debug)]
    struct Old {
        items: Vec<Field>,
    }
    #[derive(Serialize, Deserialize, PartialEq, Debug)]
    struct ItemNew {
        a: u32,
        b: String,
        #[serde(default)]
        c: u8,
    }
    #[derive(Serialize, Deserialize, PartialEq, Debug)]
    struct New {
        items: Vec<ItemNew>,
    }

    assert_eq!(
        migrate::<_, New>(&Old {
            items: vec![field()]
        })
        .unwrap(),
        New {
            items: vec![ItemNew {
                a: 7,
                b: "hi".to_owned(),
                c: 0
            }]
        }
    );
}

#[test]
fn a_vec_and_a_fixed_array_are_interchangeable() {
    #[derive(Serialize, Deserialize, PartialEq, Debug)]
    struct Dynamic {
        items: Vec<u32>,
    }
    #[derive(Serialize, Deserialize, PartialEq, Debug)]
    struct Fixed {
        items: [u32; 2],
    }

    assert_eq!(
        migrate::<_, Fixed>(&Dynamic { items: vec![1, 2] }).unwrap(),
        Fixed { items: [1, 2] }
    );
    assert_eq!(
        migrate::<_, Dynamic>(&Fixed { items: [1, 2] }).unwrap(),
        Dynamic { items: vec![1, 2] }
    );
}

// --- which read path handles evolution -------------------------------------

/// The columnar fast path decodes against the type's own schema and nothing
/// else, so evolution is the serde path's job. This is the error that says
/// which one you are on.
#[test]
fn evolution_belongs_to_the_serde_path() {
    #[derive(Serialize, Deserialize, carbonite::Schema, PartialEq, Debug)]
    struct Old {
        a: u32,
    }
    #[derive(Serialize, Deserialize, carbonite::Schema, PartialEq, Debug)]
    struct New {
        a: u32,
        #[serde(default)]
        b: u8,
    }

    let schema = Schema::<Old>::new().unwrap();
    let blob = Serializer::new(&schema).to_vec(&Old { a: 1 }).unwrap();
    let reader = Schema::<New>::from_bytes(&schema.to_bytes()).unwrap();
    let de: Deserializer<New> = Deserializer::new(reader);

    assert!(!de.uses_fast_path());
    assert!(de.from_slice_columns(&blob).is_err());
    assert_eq!(de.from_slice(&blob).unwrap(), New { a: 1, b: 0 });
}

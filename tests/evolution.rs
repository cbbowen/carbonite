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

//! Shared-structure preservation: `Shared` / `SharedArc` deduplicate per row
//! and reconstruct pointer identity on read.
#![cfg(feature = "derive")]

use serde::{Deserialize, Serialize};

use carbonite::{Deserializer, Error, Schema, Serializer, Shared, SharedArc, StaticSchema};

#[derive(Serialize, Deserialize, carbonite::Schema, PartialEq, Debug, Clone)]
struct Asset {
    name: String,
    data: Vec<u8>,
}

#[derive(Serialize, Deserialize, carbonite::Schema, PartialEq, Debug, Clone)]
struct Entity {
    id: u32,
    mesh: Shared<Asset>,
}

#[derive(Serialize, Deserialize, carbonite::Schema, PartialEq, Debug, Clone)]
struct Scene {
    entities: Vec<Entity>,
}

fn big_asset(name: &str) -> Asset {
    Asset {
        name: name.to_owned(),
        data: vec![0xAB; 1000],
    }
}

fn scene(entities: u32) -> Scene {
    let meshes = [
        Shared::new(big_asset("teapot")),
        Shared::new(big_asset("bunny")),
    ];
    Scene {
        entities: (0..entities)
            .map(|id| Entity {
                id,
                mesh: meshes[(id % 2) as usize].clone(),
            })
            .collect(),
    }
}

#[test]
fn schema_matches_trace() {
    assert_eq!(Scene::schema(), Schema::<Scene>::new().unwrap());
    assert_eq!(
        <SharedArc<Vec<u64>>>::schema(),
        Schema::<SharedArc<Vec<u64>>>::new().unwrap()
    );
}

#[test]
fn both_paths_dedupe_identically_and_rebuild_sharing() {
    let value = scene(100);
    let schema = Scene::schema();
    let ser = Serializer::new(&schema);

    let serde_bytes = ser.to_vec(&value).unwrap();
    let columnar_bytes = ser.to_vec_columns(&value).unwrap();
    assert_eq!(serde_bytes, columnar_bytes);

    // 100 entities Ã— ~1KB assets, but only two unique assets: the blob must
    // reflect the dedup.
    assert!(
        serde_bytes.len() < 3000,
        "expected deduplicated blob, got {} bytes",
        serde_bytes.len()
    );

    let de = Deserializer::new_static(schema);
    for back in [
        de.from_slice(&serde_bytes).unwrap(),
        de.from_slice_columns(&serde_bytes).unwrap(),
    ] {
        assert_eq!(back, value);
        // Identity, not just equality: entities 0 and 2 share one Rc.
        assert!(Shared::ptr_eq(
            &back.entities[0].mesh,
            &back.entities[2].mesh
        ));
        assert!(Shared::ptr_eq(
            &back.entities[1].mesh,
            &back.entities[3].mesh
        ));
        assert!(!Shared::ptr_eq(
            &back.entities[0].mesh,
            &back.entities[1].mesh
        ));
    }
}

#[test]
fn golden_layout_of_a_shared_column() {
    // Vec<Shared<u8>> with both elements pointing at one object:
    // columns = [seq len][shared key][u8 dictionary].
    let one = Shared::new(7u8);
    let value = vec![one.clone(), one];
    let schema = <Vec<Shared<u8>>>::schema();
    let blob = Serializer::new(&schema).to_vec_columns(&value).unwrap();

    #[rustfmt::skip]
    let expected: Vec<u8> = vec![
        1,          // row count
        3,          // column count
        1, 2, 1,    // column byte lengths
        2,          // sequence length
        0, 0,       // keys: new(0), repeat(0)
        7,          // dictionary: the single unique u8
    ];
    assert_eq!(blob, expected);

    let back: Vec<Shared<u8>> = Deserializer::new_static(schema).from_slice(&blob).unwrap();
    assert!(Shared::ptr_eq(&back[0], &back[1]));
}

#[test]
fn foreign_formats_degrade_to_plain_duplication() {
    let mesh = Shared::new(big_asset("teapot"));
    let entities = vec![
        Entity {
            id: 1,
            mesh: mesh.clone(),
        },
        Entity {
            id: 2,
            mesh: mesh.clone(),
        },
    ];

    // The wrapper is invisible in JSON: same output as an unshared struct,
    // duplicated inline, no addresses or keys.
    #[derive(Serialize)]
    struct PlainEntity<'a> {
        id: u32,
        mesh: &'a Asset,
    }
    let plain: Vec<PlainEntity> = entities
        .iter()
        .map(|e| PlainEntity {
            id: e.id,
            mesh: &e.mesh,
        })
        .collect();
    assert_eq!(
        serde_json::to_value(&entities).unwrap(),
        serde_json::to_value(&plain).unwrap()
    );

    // And back from JSON: values equal, sharing (documentedly) not rebuilt.
    let json = serde_json::to_string(&entities).unwrap();
    let back: Vec<Entity> = serde_json::from_str(&json).unwrap();
    assert_eq!(back, entities);
    assert!(!Shared::ptr_eq(&back[0].mesh, &back[1].mesh));
}

#[test]
fn shared_wrapper_reads_old_unshared_files() {
    // V1 had a plain field; V2 wraps it in Shared.
    #[derive(Serialize, Deserialize, carbonite::Schema, PartialEq, Debug)]
    struct EntityV1 {
        id: u32,
        mesh: Asset,
    }

    let v1_schema = EntityV1::schema();
    let blob = Serializer::new(&v1_schema)
        .to_vec(&EntityV1 {
            id: 5,
            mesh: big_asset("teapot"),
        })
        .unwrap();

    let retyped = Schema::<Entity>::from_node(v1_schema.into_node());
    let v2: Entity = Deserializer::new_static(retyped).from_slice(&blob).unwrap();
    assert_eq!(v2.id, 5);
    assert_eq!(v2.mesh.name, "teapot");
}

#[test]
fn plain_reader_handles_shared_files_until_a_repeat() {
    #[derive(Serialize, Deserialize, carbonite::Schema, PartialEq, Debug)]
    struct PlainEntity {
        id: u32,
        mesh: Asset,
    }

    let shared_schema = Entity::schema();
    let ser = Serializer::new(&shared_schema);

    // No actual sharing: every occurrence is a first occurrence, so a plain
    // reader materializes transparently.
    let unique = Entity {
        id: 1,
        mesh: Shared::new(big_asset("solo")),
    };
    let blob = ser.to_vec(&unique).unwrap();
    let retyped = Schema::<PlainEntity>::from_node(shared_schema.node().clone());
    let plain: PlainEntity = Deserializer::new_static(retyped).from_slice(&blob).unwrap();
    assert_eq!(plain.mesh.name, "solo");

    // Two occurrences in *different fields* are different schema positions
    // with separate dictionaries, so no repeat occurs and a plain reader
    // still works (at the cost of duplication on the wire).
    #[derive(Serialize, Deserialize, carbonite::Schema, PartialEq, Debug)]
    struct PlainPair {
        first: Asset,
        second: Asset,
    }
    #[derive(Serialize, Deserialize, carbonite::Schema, PartialEq, Debug)]
    struct SharedPair {
        first: Shared<Asset>,
        second: Shared<Asset>,
    }
    let mesh = Shared::new(big_asset("dup"));
    let pair_schema = SharedPair::schema();
    let blob = Serializer::new(&pair_schema)
        .to_vec(&SharedPair {
            first: mesh.clone(),
            second: mesh,
        })
        .unwrap();
    let retyped = Schema::<PlainPair>::from_node(pair_schema.into_node());
    let pair: PlainPair = Deserializer::new_static(retyped).from_slice(&blob).unwrap();
    assert_eq!(pair.first, pair.second);

    // A genuine repeat — same position, same object — cannot be
    // materialized into a plain reader and must error cleanly.
    let mesh = Shared::new(big_asset("dup"));
    let vec_schema = <Vec<Shared<Asset>>>::schema();
    let blob = Serializer::new(&vec_schema)
        .to_vec(&vec![mesh.clone(), mesh])
        .unwrap();
    let retyped = Schema::<Vec<Asset>>::from_node(vec_schema.into_node());
    let err = Deserializer::new_static(retyped)
        .from_slice(&blob)
        .unwrap_err();
    assert!(
        err.to_string().contains("carbonite::Shared"),
        "unexpected error: {err}"
    );
}

#[test]
fn removed_shared_fields_skip_cleanly_even_with_repeats() {
    #[derive(Serialize, Deserialize, carbonite::Schema, PartialEq, Debug)]
    struct SlimEntity {
        id: u32,
    }

    let mesh = Shared::new(big_asset("teapot"));
    let schema = <Vec<Entity>>::schema();
    let blob = Serializer::new(&schema)
        .to_vec(&vec![
            Entity {
                id: 1,
                mesh: mesh.clone(),
            },
            Entity {
                id: 2,
                mesh: mesh.clone(),
            },
        ])
        .unwrap();

    let retyped = Schema::<Vec<SlimEntity>>::from_node(schema.into_node());
    let slim: Vec<SlimEntity> = Deserializer::new_static(retyped).from_slice(&blob).unwrap();
    assert_eq!(slim, vec![SlimEntity { id: 1 }, SlimEntity { id: 2 }]);
}

#[test]
fn rc_and_arc_wrappers_share_a_wire_format() {
    #[derive(Serialize, Deserialize, carbonite::Schema, PartialEq, Debug)]
    struct ArcEntity {
        id: u32,
        mesh: SharedArc<Asset>,
    }

    // The wrappers share a schema node (struct names differ, the shared
    // field's node does not).
    assert_eq!(
        <Shared<Asset>>::schema().node(),
        <SharedArc<Asset>>::schema().node()
    );

    let mesh = Shared::new(big_asset("teapot"));
    let schema = <Vec<Entity>>::schema();
    let blob = Serializer::new(&schema)
        .to_vec(&vec![
            Entity {
                id: 1,
                mesh: mesh.clone(),
            },
            Entity {
                id: 2,
                mesh: mesh.clone(),
            },
        ])
        .unwrap();

    let retyped = Schema::<Vec<ArcEntity>>::from_node(schema.into_node());
    let arcs: Vec<ArcEntity> = Deserializer::new_static(retyped).from_slice(&blob).unwrap();
    assert!(SharedArc::ptr_eq(&arcs[0].mesh, &arcs[1].mesh));
    assert_eq!(arcs[0].mesh.name, "teapot");
}

#[test]
fn sharing_is_per_row_in_batches() {
    let mesh = Shared::new(big_asset("teapot"));
    let row = Entity {
        id: 1,
        mesh: mesh.clone(),
    };
    let schema = Entity::schema();
    let ser = Serializer::new(&schema);

    let mut batch = ser.batch();
    batch.push(&row).unwrap();
    batch.push_columns(&row).unwrap();
    let blob = batch.finish();

    let de = Deserializer::new_static(schema);
    let rows: Vec<Entity> = de.rows(&blob).unwrap().collect::<Result<_, _>>().unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0], rows[1]);
    // Each row got its own dictionary entry: no cross-row identity.
    assert!(!Shared::ptr_eq(&rows[0].mesh, &rows[1].mesh));
}

#[test]
fn corrupted_keys_and_truncation_error_cleanly() {
    let one = Shared::new(7u8);
    let value = vec![one.clone(), one];
    let schema = <Vec<Shared<u8>>>::schema();
    let blob = Serializer::new(&schema).to_vec_columns(&value).unwrap();
    let de = Deserializer::new_static(schema);

    // Point the repeat key past the dictionary.
    let mut bad = blob.clone();
    let key_index = blob.len() - 2; // [.., keys: 0, 0, dict: 7] â€” second key
    bad[key_index] = 9;
    assert!(matches!(
        de.from_slice(&bad),
        Err(Error::InvalidTag {
            what: "shared key",
            ..
        })
    ));
    assert!(matches!(
        de.from_slice_columns(&bad),
        Err(Error::InvalidTag {
            what: "shared key",
            ..
        })
    ));

    for cut in 0..blob.len() {
        let _ = de.from_slice(&blob[..cut]);
        let _ = de.from_slice_columns(&blob[..cut]);
    }
}

//! Columnar fast-path correctness: the monomorphized writers/readers must be
//! byte-for-byte interchangeable with the serde-driven path.
#![cfg(feature = "derive")]

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::marker::PhantomData;
use std::num::NonZeroU32;
use std::time::Duration;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use carbonite::{Deserializer, Error, Schema, Serializer, StaticSchema};

#[derive(Serialize, Deserialize, carbonite::Schema, PartialEq, Debug, Clone)]
struct Marker;

#[derive(Serialize, Deserialize, carbonite::Schema, PartialEq, Debug, Clone)]
struct Meters(f64);

#[derive(Serialize, Deserialize, carbonite::Schema, PartialEq, Debug, Clone)]
struct Pair(u8, String);

#[derive(Serialize, Deserialize, carbonite::Schema, PartialEq, Debug, Clone)]
#[serde(transparent)]
struct Wrapper {
    inner: Vec<u8>,
}

#[derive(Serialize, Deserialize, carbonite::Schema, PartialEq, Debug, Clone)]
enum Shade {
    Plain,
    Tinted(u8),
    Blend(f32, f32),
    Custom {
        red: u8,
        alpha: Option<f64>,
    },
    #[serde(skip)]
    #[allow(dead_code)]
    Secret(String),
}

#[derive(Serialize, Deserialize, carbonite::Schema, Debug, Clone)]
#[allow(dead_code)]
struct Kitchen {
    id: u128,
    letter: char,
    text: String,
    signed: isize,
    non_zero: NonZeroU32,
    #[serde(skip)]
    cache: Option<String>,
    fixed: [u16; 3],
    pair: (i8, u64, Pair),
    maybe: Option<Option<u32>>,
    shades: Vec<Shade>,
    lookup: BTreeMap<String, Meters>,
    wrapper: Wrapper,
    outcome: Result<u32, String>,
    elapsed: Duration,
    marker: Marker,
    ghost: PhantomData<u8>,
    unit: (),
    flags: Vec<bool>,
    heap: std::collections::BinaryHeap<u8>,
}

fn sample() -> Kitchen {
    Kitchen {
        id: u128::MAX - 7,
        letter: '🦀',
        text: "héllo — こんにちは".to_owned(),
        signed: -42,
        non_zero: NonZeroU32::new(7).unwrap(),
        cache: None,
        fixed: [1, 2, u16::MAX],
        pair: (-8, u64::MAX, Pair(3, "pair".to_owned())),
        maybe: Some(None),
        shades: vec![
            Shade::Plain,
            Shade::Tinted(9),
            Shade::Blend(0.25, -3.5),
            Shade::Custom {
                red: 200,
                alpha: Some(0.125),
            },
            Shade::Custom {
                red: 1,
                alpha: None,
            },
        ],
        lookup: BTreeMap::from([
            ("a".to_owned(), Meters(1.5)),
            ("b".to_owned(), Meters(-0.5)),
        ]),
        wrapper: Wrapper {
            inner: vec![1, 2, 3],
        },
        outcome: Err("nope".to_owned()),
        elapsed: Duration::new(88, 123_456_789),
        marker: Marker,
        ghost: PhantomData,
        unit: (),
        flags: vec![true, false, true],
        heap: std::collections::BinaryHeap::from([3u8, 1, 2]),
    }
}

/// The core property: both writers produce identical bytes, and both readers
/// accept either writer's output.
fn assert_paths_interchangeable<T>(value: &T)
where
    T: Serialize
        + DeserializeOwned
        + StaticSchema
        + carbonite::SerializeColumns
        + for<'de> carbonite::DeserializeColumns<'de>
        + PartialEq
        + std::fmt::Debug,
{
    let schema = T::schema();
    let ser = Serializer::new(&schema);
    let serde_bytes = ser.to_vec(value).expect("serde path serialize");
    let columnar_bytes = ser.to_vec_columns(value).expect("columnar path serialize");
    assert_eq!(
        serde_bytes, columnar_bytes,
        "writers must produce identical bytes"
    );

    let de = Deserializer::new_static(schema);
    let via_serde: T = de.from_slice(&serde_bytes).expect("serde path deserialize");
    let via_columnar: T = de
        .from_slice_columns(&serde_bytes)
        .expect("columnar path deserialize");
    assert_eq!(&via_serde, value);
    assert_eq!(&via_columnar, value);
}

#[test]
fn kitchen_sink_paths_are_interchangeable() {
    // BinaryHeap has no PartialEq; compare through a sorted view instead.
    #[derive(Serialize, Deserialize, carbonite::Schema, PartialEq, Debug, Clone)]
    struct NoHeap {
        id: u128,
        letter: char,
        text: String,
        signed: isize,
        non_zero: NonZeroU32,
        #[serde(skip)]
        cache: Option<String>,
        fixed: [u16; 3],
        pair: (i8, u64, Pair),
        maybe: Option<Option<u32>>,
        shades: Vec<Shade>,
        lookup: BTreeMap<String, Meters>,
        wrapper: Wrapper,
        outcome: Result<u32, String>,
        elapsed: Duration,
        marker: Marker,
        ghost: PhantomData<u8>,
        unit: (),
        flags: Vec<bool>,
    }

    let full = sample();
    let value = NoHeap {
        id: full.id,
        letter: full.letter,
        text: full.text.clone(),
        signed: full.signed,
        non_zero: full.non_zero,
        cache: None,
        fixed: full.fixed,
        pair: full.pair.clone(),
        maybe: full.maybe,
        shades: full.shades.clone(),
        lookup: full.lookup.clone(),
        wrapper: full.wrapper.clone(),
        outcome: full.outcome.clone(),
        elapsed: full.elapsed,
        marker: Marker,
        ghost: PhantomData,
        unit: (),
        flags: full.flags.clone(),
    };
    assert_paths_interchangeable(&value);
    assert_paths_interchangeable(&vec![value.clone(), value]);
}

#[test]
fn heap_bytes_match_between_paths() {
    // BinaryHeap iteration order is deterministic for a given heap layout, so
    // byte equality between the two writers still holds.
    let value = sample();
    let schema = Kitchen::schema();
    let ser = Serializer::new(&schema);
    assert_eq!(
        ser.to_vec(&value).unwrap(),
        ser.to_vec_columns(&value).unwrap()
    );
}

#[test]
fn primitive_and_container_roundtrips() {
    assert_paths_interchangeable(&42u8);
    assert_paths_interchangeable(&-1i64);
    assert_paths_interchangeable(&'é');
    assert_paths_interchangeable(&String::from("solo"));
    assert_paths_interchangeable(&Some(7u32));
    assert_paths_interchangeable(&None::<String>);
    assert_paths_interchangeable(&Vec::<u64>::new());
    assert_paths_interchangeable(&vec![vec![1u8], vec![], vec![2, 3]]);
    assert_paths_interchangeable(&BTreeMap::from([(1u8, "one".to_owned())]));
    assert_paths_interchangeable(&Box::new(9u32));
    assert_paths_interchangeable(&(Duration::from_millis(5), PhantomData::<u16>));
}

#[test]
fn batches_mix_both_writers() {
    let schema = <Vec<Shade>>::schema();
    let ser = Serializer::new(&schema);
    let rows = [
        vec![Shade::Plain, Shade::Tinted(1)],
        vec![],
        vec![Shade::Custom {
            red: 3,
            alpha: None,
        }],
    ];

    let mut serde_batch = ser.batch();
    let mut mixed_batch = ser.batch();
    for (i, row) in rows.iter().enumerate() {
        serde_batch.push(row).unwrap();
        if i % 2 == 0 {
            mixed_batch.push_columns(row).unwrap();
        } else {
            mixed_batch.push(row).unwrap();
        }
    }
    assert_eq!(serde_batch.finish(), mixed_batch.finish());
}

#[test]
fn columnar_write_of_skipped_variant_errors_like_serde() {
    let schema = Shade::schema();
    let ser = Serializer::new(&schema);
    let value = Shade::Secret("boo".to_owned());
    assert!(
        ser.to_vec(&value).is_err(),
        "serde path must reject skipped variants"
    );
    assert!(
        ser.to_vec_columns(&value).is_err(),
        "columnar path must reject skipped variants"
    );
}

#[test]
fn columnar_paths_reject_foreign_schemas() {
    // A schema from an older version of the type: same fields minus one.
    #[derive(Serialize, Deserialize, carbonite::Schema, PartialEq, Debug)]
    struct SaveV1 {
        id: u32,
    }
    #[derive(Serialize, Deserialize, carbonite::Schema, PartialEq, Debug)]
    struct SaveV2 {
        id: u32,
        #[serde(default)]
        mana: u32,
    }

    let v1_schema = SaveV1::schema();
    let blob = Serializer::new(&v1_schema)
        .to_vec(&SaveV1 { id: 7 })
        .unwrap();

    // Serialize: a V2 value cannot be written columnar against a V1 schema.
    let retyped = Schema::<SaveV2>::from_node(v1_schema.node().clone());
    let ser = Serializer::new(&retyped);
    assert!(matches!(
        ser.to_vec_columns(&SaveV2 { id: 7, mana: 1 }),
        Err(Error::SchemaMismatch { .. })
    ));

    // Deserialize: the columnar reader refuses, the serde reader evolves.
    let de = Deserializer::new_static(retyped);
    assert!(matches!(
        de.from_slice_columns(&blob),
        Err(Error::SchemaMismatch { .. })
    ));
    let evolved: SaveV2 = de.from_slice(&blob).unwrap();
    assert_eq!(evolved, SaveV2 { id: 7, mana: 0 });
}

#[test]
fn borrowed_columnar_roundtrip() {
    #[derive(Serialize, Deserialize, carbonite::Schema, PartialEq, Debug)]
    struct LogLine<'a> {
        #[serde(borrow)]
        message: &'a str,
        note: Cow<'a, str>,
        code: u32,
    }

    let value = LogLine {
        message: "zero copy",
        note: Cow::Borrowed("borrowed"),
        code: 7,
    };
    let schema = LogLine::schema();
    let blob = Serializer::new(&schema).to_vec_columns(&value).unwrap();
    let de = Deserializer::<LogLine>::new_static(schema);
    let back: LogLine = de.from_slice_columns(&blob).unwrap();
    assert_eq!(back, value);
    assert!(
        matches!(back.note, Cow::Borrowed(_)),
        "columnar Cow reads borrow"
    );
}

#[test]
fn truncated_input_never_panics() {
    let value = sample();
    let schema = Kitchen::schema();
    let blob = Serializer::new(&schema).to_vec_columns(&value).unwrap();
    let de = Deserializer::new_static(schema);
    for cut in 0..blob.len() {
        let _ = de.from_slice_columns(&blob[..cut]);
    }
    // Bit flips in the header/tag region must also fail cleanly.
    for i in 0..blob.len().min(64) {
        let mut bad = blob.clone();
        bad[i] ^= 0xff;
        let _ = de.from_slice_columns(&bad);
    }
}

#[test]
fn invalid_bytes_error_cleanly() {
    // presence byte out of range
    let schema = <Option<u8>>::schema();
    let ser = Serializer::new(&schema);
    let mut blob = ser.to_vec_columns(&Some(3u8)).unwrap();
    let de = Deserializer::<Option<u8>>::new_static(schema);
    // presence column is the first column; find and corrupt its byte.
    let presence_index = blob.len() - 2; // [presence][value]
    blob[presence_index] = 9;
    assert!(matches!(
        de.from_slice_columns(&blob),
        Err(Error::InvalidTag {
            what: "presence",
            ..
        })
    ));

    // enum tag out of range
    let schema = Shade::schema();
    let blob = Serializer::new(&schema)
        .to_vec_columns(&Shade::Plain)
        .unwrap();
    let de = Deserializer::<Shade>::new_static(schema);
    let mut bad = blob.clone();
    let last = bad.len() - 1;
    bad[last] = 0x37; // tag column holds the single varint at the end
    assert!(matches!(
        de.from_slice_columns(&bad),
        Err(Error::InvalidTag {
            what: "enum variant",
            ..
        })
    ));
}

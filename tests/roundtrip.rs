//! End-to-end round-trips across the supported serde data model.

use std::collections::{BTreeMap, HashMap};

use serde::{Deserialize, Serialize};

use carbonite::{Deserializer, Error, Schema, SelfDescribingDeserializer, Serializer};

fn round_trip<T>(value: &T) -> T
where
    T: Serialize + serde::de::DeserializeOwned,
{
    let bytes = carbonite::to_vec(value).expect("serialize");
    carbonite::from_slice(&bytes).expect("deserialize")
}

#[derive(Serialize, Deserialize, PartialEq, Debug, Clone)]
enum Shade {
    Plain,
    Tinted(u8),
    Blend(f32, f32),
    Custom { red: u8, alpha: Option<f64> },
}

#[derive(Serialize, Deserialize, PartialEq, Debug, Clone)]
struct Marker;

#[derive(Serialize, Deserialize, PartialEq, Debug, Clone)]
struct Meters(f64);

#[derive(Serialize, Deserialize, PartialEq, Debug, Clone)]
struct Kitchen {
    id: u128,
    delta: i128,
    letter: char,
    text: String,
    raw: Vec<u8>,
    flagged: Vec<bool>,
    fixed: [u16; 3],
    pair: (i8, u64),
    maybe: Option<Option<u32>>,
    shades: Vec<Shade>,
    lookup: BTreeMap<String, Meters>,
    counts: HashMap<u32, i64>,
    marker: Marker,
    unit: (),
}

fn sample() -> Kitchen {
    Kitchen {
        id: u128::MAX - 7,
        delta: i128::MIN + 3,
        letter: '🦀',
        text: "héllo — こんにちは".to_owned(),
        raw: vec![0, 255, 42],
        flagged: vec![true, false, true],
        fixed: [1, 2, u16::MAX],
        pair: (-8, u64::MAX),
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
        counts: HashMap::from([(3, -30), (5, 50)]),
        marker: Marker,
        unit: (),
    }
}

#[test]
fn kitchen_sink_round_trips() {
    let value = sample();
    assert_eq!(round_trip(&value), value);
}

#[test]
fn nan_and_extremes_round_trip_bit_exact() {
    let values: Vec<(f32, f64)> = vec![
        (f32::NAN, f64::NAN),
        (f32::NEG_INFINITY, f64::INFINITY),
        (-0.0, -0.0),
        (f32::MIN_POSITIVE, f64::MIN_POSITIVE),
    ];
    let bytes = carbonite::to_vec(&values).unwrap();
    let back: Vec<(f32, f64)> = carbonite::from_slice(&bytes).unwrap();
    let bits = |vs: &[(f32, f64)]| -> Vec<(u32, u64)> {
        vs.iter().map(|(a, b)| (a.to_bits(), b.to_bits())).collect()
    };
    assert_eq!(bits(&back), bits(&values));
}

#[test]
fn schema_reuse_across_blobs_and_batches() {
    let schema = Schema::<Kitchen>::new().unwrap();
    let ser = Serializer::new(&schema);
    let de = Deserializer::new(schema.clone());

    // Per-message blobs against one schema.
    let a = sample();
    let mut b = sample();
    b.id = 1;
    b.shades.clear();
    for value in [&a, &b] {
        let blob = ser.to_vec(value).unwrap();
        let back: Kitchen = de.from_slice(&blob).unwrap();
        assert_eq!(&back, value);
    }

    // Many rows in one blob.
    let mut batch = ser.batch();
    batch.push(&a).unwrap();
    batch.push(&b).unwrap();
    batch.push(&a).unwrap();
    assert_eq!(batch.rows(), 3);
    let blob = batch.finish();
    let rows: Vec<Kitchen> = de.rows(&blob).unwrap().collect::<Result<_, _>>().unwrap();
    assert_eq!(rows, vec![a.clone(), b, a]);
}

#[test]
fn top_level_primitives_and_containers() {
    assert_eq!(round_trip(&42u8), 42);
    assert_eq!(round_trip(&-1i64), -1);
    assert_eq!(round_trip(&'é'), 'é');
    assert!(round_trip(&true));
    assert_eq!(round_trip(&String::from("solo")), "solo");
    assert_eq!(round_trip(&Some(7u32)), Some(7));
    assert_eq!(round_trip(&None::<String>), None);
    assert_eq!(round_trip(&Vec::<u64>::new()), Vec::<u64>::new());
    assert_eq!(
        round_trip(&vec![vec![1u8], vec![], vec![2, 3]]),
        vec![vec![1u8], vec![], vec![2, 3]]
    );
}

#[test]
fn borrowed_strings_deserialize_zero_copy() {
    #[derive(Serialize, Deserialize, PartialEq, Debug)]
    struct Owned {
        text: String,
        blob: Vec<u8>,
    }

    #[derive(Deserialize, PartialEq, Debug)]
    struct Borrowed<'a> {
        #[serde(borrow)]
        text: &'a str,
        blob: Vec<u8>,
    }

    let schema = Schema::<Owned>::new().unwrap();
    let blob = Serializer::new(&schema)
        .to_vec(&Owned {
            text: "zero copy".into(),
            blob: vec![1, 2],
        })
        .unwrap();

    // Borrowed types can't be traced, so use the untraced constructor.
    let de = Deserializer::<Borrowed>::new_untraced(Schema::from_node(schema.into_node()));
    let back: Borrowed = de.from_slice(&blob).unwrap();
    assert_eq!(
        back,
        Borrowed {
            text: "zero copy",
            blob: vec![1, 2]
        }
    );
}

#[test]
fn self_describing_wrappers_round_trip() {
    let value = sample();
    let schema = Schema::<Kitchen>::new().unwrap();
    let bytes = carbonite::SelfDescribingSerializer::new(&schema)
        .to_vec(&value)
        .unwrap();
    let back: Kitchen = SelfDescribingDeserializer::new()
        .from_slice(&bytes)
        .unwrap();
    assert_eq!(back, value);
}

#[test]
fn corrupted_input_errors_cleanly() {
    let bytes = carbonite::to_vec(&sample()).unwrap();

    // Bad magic.
    let mut bad = bytes.clone();
    bad[0] = b'X';
    assert!(matches!(
        carbonite::from_slice::<Kitchen>(&bad),
        Err(Error::Malformed(_))
    ));

    // Truncation anywhere must never panic.
    for cut in 0..bytes.len() {
        let _ = carbonite::from_slice::<Kitchen>(&bytes[..cut]);
    }
}

#[test]
fn skip_serializing_if_is_rejected() {
    #[derive(Serialize, Deserialize, PartialEq, Debug)]
    struct Sparse {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        note: Option<String>,
    }

    let schema = Schema::<Sparse>::new().unwrap();
    let ser = Serializer::new(&schema);
    // Present value serializes fine…
    ser.to_vec(&Sparse {
        note: Some("x".into()),
    })
    .unwrap();
    // …but a skipped field would leave the row incomplete.
    assert!(matches!(
        ser.to_vec(&Sparse { note: None }),
        Err(Error::IncompleteRow)
    ));
}

#[test]
fn untraceable_types_error_clearly() {
    #[derive(Serialize, Deserialize)]
    #[serde(untagged)]
    #[allow(dead_code)]
    enum Loose {
        Num(u32),
        Text(String),
    }

    assert!(matches!(
        Schema::<Loose>::new(),
        Err(Error::Untraceable { .. })
    ));
}

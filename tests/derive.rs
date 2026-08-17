//! `#[derive(Schema)]` consistency: for every supported shape, the derived
//! schema must be identical to what runtime tracing discovers. Because
//! tracing goes through serde's own derive, these tests check our replica of
//! serde's naming rules against the real thing.
#![cfg(feature = "derive")]

use std::any::type_name;
use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::marker::PhantomData;
use std::num::{NonZeroI64, NonZeroU32};
use std::time::{Duration, SystemTime};

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use carbonite::{Deserializer, Schema, Serializer, StaticSchema};

fn assert_matches_trace<T: StaticSchema + DeserializeOwned>() {
    assert_eq!(
        T::schema(),
        Schema::<T>::new().unwrap(),
        "derived schema must match traced schema for {}",
        type_name::<T>()
    );
}

#[test]
fn std_types_match_trace() {
    assert_matches_trace::<u8>();
    assert_matches_trace::<i128>();
    assert_matches_trace::<usize>();
    assert_matches_trace::<isize>();
    assert_matches_trace::<f64>();
    assert_matches_trace::<char>();
    assert_matches_trace::<bool>();
    assert_matches_trace::<()>();
    assert_matches_trace::<String>();
    assert_matches_trace::<NonZeroU32>();
    assert_matches_trace::<NonZeroI64>();
    assert_matches_trace::<Option<Option<u32>>>();
    assert_matches_trace::<Vec<String>>();
    assert_matches_trace::<VecDeque<u8>>();
    assert_matches_trace::<BTreeSet<i32>>();
    assert_matches_trace::<HashSet<u64>>();
    assert_matches_trace::<[f32; 4]>();
    assert_matches_trace::<(u8,)>();
    assert_matches_trace::<(u8, String, Option<char>)>();
    assert_matches_trace::<HashMap<String, u32>>();
    assert_matches_trace::<BTreeMap<u64, Vec<bool>>>();
    assert_matches_trace::<Result<u32, String>>();
    assert_matches_trace::<Box<u32>>();
    assert_matches_trace::<Cow<'static, str>>();
    assert_matches_trace::<PhantomData<u8>>();
    assert_matches_trace::<Duration>();
    assert_matches_trace::<SystemTime>();
}

#[test]
fn struct_shapes_match_trace() {
    #[derive(Serialize, Deserialize, carbonite::Schema)]
    struct Marker;

    #[derive(Serialize, Deserialize, carbonite::Schema)]
    struct Meters(f64);

    #[derive(Serialize, Deserialize, carbonite::Schema)]
    struct Pair(u8, String);

    #[derive(Serialize, Deserialize, carbonite::Schema)]
    struct Empty {}

    #[derive(Serialize, Deserialize, carbonite::Schema)]
    #[allow(dead_code)]
    struct Kitchen {
        id: u64,
        label: String,
        meters: Meters,
        pair: Pair,
        marker: Marker,
        tags: Vec<Option<char>>,
        empty: Empty,
    }

    assert_matches_trace::<Marker>();
    assert_matches_trace::<Meters>();
    assert_matches_trace::<Pair>();
    assert_matches_trace::<Empty>();
    assert_matches_trace::<Kitchen>();
}

#[test]
fn enum_shapes_match_trace() {
    #[derive(Serialize, Deserialize, carbonite::Schema)]
    #[allow(dead_code)]
    enum Shade {
        Plain,
        Tinted(u8),
        Blend(f32, f32),
        Custom { red: u8, alpha: Option<f64> },
    }

    #[derive(Serialize, Deserialize, carbonite::Schema)]
    #[allow(dead_code)]
    enum Nested {
        Leaf,
        Deep(Vec<Shade>),
    }

    assert_matches_trace::<Shade>();
    assert_matches_trace::<Nested>();
}

#[test]
fn rename_attributes_match_trace() {
    #[derive(Serialize, Deserialize, carbonite::Schema)]
    #[serde(rename = "WireName", rename_all = "camelCase")]
    #[allow(dead_code)]
    struct Renamed {
        user_id: u64,
        #[serde(rename = "display")]
        display_name: String,
        home_address_line: Option<String>,
    }

    // serde's snake_case rule splits on any Unicode uppercase, not just
    // ASCII, so a non-ASCII capital mid-identifier is where a replica of the
    // rule most easily drifts out of step with the real thing.
    #[derive(Serialize, Deserialize, carbonite::Schema)]
    #[serde(rename_all = "snake_case")]
    #[allow(dead_code, non_camel_case_types)]
    enum Units {
        MetresPerSecond,
        AÉrogare,
        Watt,
    }

    #[derive(Serialize, Deserialize, carbonite::Schema)]
    #[serde(rename_all = "SCREAMING-KEBAB-CASE")]
    #[allow(dead_code)]
    struct Screaming {
        max_retry_count: u32,
    }

    #[derive(Serialize, Deserialize, carbonite::Schema)]
    #[serde(rename_all = "snake_case", rename_all_fields = "kebab-case")]
    #[allow(dead_code)]
    enum HttpEvent {
        RequestStarted,
        #[serde(rename = "hdr")]
        HeaderParsed(String),
        BodyChunk {
            byte_count: u64,
            is_last: bool,
        },
        #[serde(rename_all = "UPPERCASE")]
        ConnectionClosed {
            reason_code: u16,
        },
    }

    assert_matches_trace::<Renamed>();
    assert_matches_trace::<Screaming>();
    assert_matches_trace::<Units>();
    assert_matches_trace::<HttpEvent>();
}

#[test]
fn skip_and_transparent_match_trace() {
    #[derive(Serialize, Deserialize, carbonite::Schema)]
    #[allow(dead_code)]
    struct WithSkip {
        kept: u32,
        #[serde(skip)]
        cache: Option<String>,
        also_kept: bool,
    }

    #[derive(Serialize, Deserialize, carbonite::Schema)]
    #[serde(transparent)]
    struct Wrapper {
        inner: Vec<u8>,
    }

    #[derive(Serialize, Deserialize, carbonite::Schema)]
    #[allow(dead_code)]
    enum WithSkippedVariant {
        Used(u8),
        #[serde(skip)]
        Hidden(String),
        AlsoUsed,
    }

    assert_matches_trace::<WithSkip>();
    assert_matches_trace::<Wrapper>();
    assert_matches_trace::<WithSkippedVariant>();
}

#[test]
fn generics_match_trace() {
    #[derive(Serialize, Deserialize, carbonite::Schema)]
    #[allow(dead_code)]
    struct Tagged<T> {
        value: T,
        tag: String,
    }

    #[derive(Serialize, Deserialize, carbonite::Schema)]
    #[allow(dead_code)]
    enum Tree<L, R> {
        Left(L),
        Right { value: R },
    }

    assert_matches_trace::<Tagged<u32>>();
    assert_matches_trace::<Tagged<Vec<Tagged<bool>>>>();
    assert_matches_trace::<Tree<String, f64>>();
}

#[test]
fn derived_schema_round_trips_end_to_end() {
    #[derive(Serialize, Deserialize, carbonite::Schema, PartialEq, Debug, Clone)]
    #[serde(rename_all = "camelCase")]
    struct Save {
        player_name: String,
        hit_points: u32,
        inventory: Vec<(String, u8)>,
    }

    let value = Save {
        player_name: "Ada".to_owned(),
        hit_points: 90,
        inventory: vec![("rope".to_owned(), 2), ("torch".to_owned(), 5)],
    };

    let schema = Save::schema();
    let blob = Serializer::new(&schema).to_vec(&value).unwrap();

    // new_static must detect the fast path (schemas are identical).
    let de = Deserializer::new_static(schema);
    let back: Save = de.from_slice(&blob).unwrap();
    assert_eq!(back, value);
}

#[test]
fn borrowing_types_get_the_static_fast_path() {
    // Borrowing types can't be traced (no DeserializeOwned), but the derive
    // gives them a compile-time schema — and therefore the fast path and
    // self-describing decoding.
    #[derive(Serialize, Deserialize, carbonite::Schema, PartialEq, Debug)]
    struct LogLine<'a> {
        #[serde(borrow)]
        message: &'a str,
        code: u32,
    }

    let value = LogLine {
        message: "zero copy",
        code: 7,
    };
    let schema = LogLine::schema();

    let blob = Serializer::new(&schema).to_vec(&value).unwrap();
    let de = Deserializer::<LogLine>::new_static(schema.clone());
    let back: LogLine = de.from_slice(&blob).unwrap();
    assert_eq!(back, value);

    let framed = carbonite::SelfDescribingSerializer::new(&schema)
        .to_vec(&value)
        .unwrap();
    let back: LogLine = carbonite::from_slice_static(&framed).unwrap();
    assert_eq!(back, value);
}

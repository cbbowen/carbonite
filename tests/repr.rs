//! `#[carbonite(as = "Repr")]`: types whose wire shape is another type.
//!
//! serde reaches that shape with `from`/`try_from` + `into`; the attribute
//! tells carbonite which type it is, once, for both directions. The schema, the
//! column count, and both columnar paths then come from the repr, so every test
//! here checks that a converted type is indistinguishable from writing the repr
//! directly.
#![cfg(feature = "derive")]

use std::collections::BTreeMap;

use carbonite::{Deserializer, Error, Schema, Serializer, StaticSchema};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Fixtures.
// ---------------------------------------------------------------------------

/// The classic case: a type with invariants its wire form does not carry.
#[derive(Serialize, Deserialize, carbonite::Schema, PartialEq, Debug, Clone)]
#[serde(from = "f64", into = "f64")]
#[carbonite(as = "f64")]
struct Degrees(f64);

impl From<f64> for Degrees {
    fn from(raw: f64) -> Self {
        Degrees(raw.rem_euclid(360.0))
    }
}

impl From<Degrees> for f64 {
    fn from(angle: Degrees) -> f64 {
        angle.0
    }
}

/// A fallible conversion, and a repr that is a whole struct rather than a
/// primitive.
#[derive(Serialize, Deserialize, carbonite::Schema, PartialEq, Debug, Clone)]
struct RangeRepr {
    lo: u32,
    hi: u32,
}

#[derive(Serialize, Deserialize, carbonite::Schema, PartialEq, Debug, Clone)]
#[serde(try_from = "RangeRepr", into = "RangeRepr")]
#[carbonite(as = "RangeRepr")]
struct Range {
    lo: u32,
    hi: u32,
}

impl TryFrom<RangeRepr> for Range {
    type Error = String;

    fn try_from(repr: RangeRepr) -> Result<Self, String> {
        if repr.lo > repr.hi {
            return Err(format!("empty range {}..{}", repr.lo, repr.hi));
        }
        Ok(Range {
            lo: repr.lo,
            hi: repr.hi,
        })
    }
}

impl From<Range> for RangeRepr {
    fn from(range: Range) -> RangeRepr {
        RangeRepr {
            lo: range.lo,
            hi: range.hi,
        }
    }
}

/// A repr that is itself derived, so the whole path stays monomorphized, and a
/// container that reorders its fields on the way out.
#[derive(Serialize, Deserialize, carbonite::Schema, PartialEq, Debug, Clone)]
struct SortedRepr {
    values: Vec<i16>,
}

#[derive(Serialize, Deserialize, carbonite::Schema, PartialEq, Debug, Clone)]
#[serde(from = "SortedRepr", into = "SortedRepr")]
#[carbonite(as = "SortedRepr")]
struct Sorted {
    values: Vec<i16>,
}

impl From<SortedRepr> for Sorted {
    fn from(repr: SortedRepr) -> Self {
        let mut values = repr.values;
        values.sort_unstable();
        Sorted { values }
    }
}

impl From<Sorted> for SortedRepr {
    fn from(sorted: Sorted) -> SortedRepr {
        SortedRepr {
            values: sorted.values,
        }
    }
}

/// An enum whose wire form is a plain string.
#[derive(Serialize, Deserialize, carbonite::Schema, PartialEq, Debug, Clone)]
#[serde(try_from = "String", into = "String")]
#[carbonite(as = "String")]
enum Mode {
    Fast,
    Small,
}

impl TryFrom<String> for Mode {
    type Error = String;

    fn try_from(raw: String) -> Result<Self, String> {
        match raw.as_str() {
            "fast" => Ok(Mode::Fast),
            "small" => Ok(Mode::Small),
            other => Err(format!("unknown mode `{other}`")),
        }
    }
}

impl From<Mode> for String {
    fn from(mode: Mode) -> String {
        match mode {
            Mode::Fast => "fast".to_owned(),
            Mode::Small => "small".to_owned(),
        }
    }
}

/// A generic container, where the repr mentions the parameter.
#[derive(Serialize, Deserialize, carbonite::Schema, PartialEq, Debug, Clone)]
#[serde(from = "Vec<T>", into = "Vec<T>")]
#[carbonite(as = "Vec<T>")]
struct Reversed<T: Clone> {
    items: Vec<T>,
}

impl<T: Clone> From<Vec<T>> for Reversed<T> {
    fn from(mut items: Vec<T>) -> Self {
        items.reverse();
        Reversed { items }
    }
}

impl<T: Clone> From<Reversed<T>> for Vec<T> {
    fn from(reversed: Reversed<T>) -> Vec<T> {
        let mut items = reversed.items;
        items.reverse();
        items
    }
}

/// Converted types must work as ordinary fields of an ordinary derived type.
#[derive(Serialize, Deserialize, carbonite::Schema, PartialEq, Debug, Clone)]
struct Settings {
    id: u32,
    heading: Degrees,
    span: Range,
    mode: Mode,
    lookup: BTreeMap<String, Mode>,
    history: Vec<Degrees>,
}

fn settings() -> Settings {
    Settings {
        id: 9,
        heading: Degrees(37.5),
        span: Range { lo: 2, hi: 8 },
        mode: Mode::Small,
        lookup: BTreeMap::from([("a".to_owned(), Mode::Fast)]),
        history: vec![Degrees(0.0), Degrees(359.5)],
    }
}

// ---------------------------------------------------------------------------
// Helpers, mirroring tests/columnar.rs.
// ---------------------------------------------------------------------------

fn assert_matches_trace<T: StaticSchema + DeserializeOwned>() {
    assert_eq!(
        T::schema(),
        Schema::<T>::new().unwrap(),
        "a converted type's derived schema must match its traced schema",
    );
}

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

// ---------------------------------------------------------------------------
// The schema is the repr's schema.
// ---------------------------------------------------------------------------

#[test]
fn the_schema_is_the_reprs() {
    assert_eq!(Degrees::schema(), f64::schema().cast());
    assert_eq!(Range::schema(), RangeRepr::schema().cast());
    assert_eq!(Mode::schema(), String::schema().cast());
    assert_eq!(Sorted::schema(), SortedRepr::schema().cast());
    assert_eq!(
        <Reversed<u8>>::schema(),
        <Vec<u8> as StaticSchema>::schema().cast()
    );

    // Including the column count and the fixed-width hint that lets sequences
    // bulk-reserve, so `Vec<Degrees>` reserves exactly as `Vec<f64>` does.
    assert_eq!(Degrees::columns(), 1);
    assert_eq!(<Degrees as StaticSchema>::FIXED_WIDTH, Some(8));
    assert_eq!(Range::columns(), RangeRepr::columns());
    assert_eq!(Mode::columns(), 2);
}

#[test]
fn derived_schemas_match_trace() {
    // Conversions total enough to accept the tracer's synthetic values still
    // trace, and must agree with the derive.
    assert_matches_trace::<Degrees>();
    assert_matches_trace::<Range>();
    assert_matches_trace::<Sorted>();
    assert_matches_trace::<Reversed<String>>();
    assert_matches_trace::<Vec<Sorted>>();
}

/// A *validating* conversion cannot be traced at all: tracing works by driving
/// `Deserialize` with synthetic values, and `Mode::try_from` rejects them. So
/// the attribute does not merely save the trace here — it is the only way to
/// get a schema for such a type, or for anything containing one.
#[test]
fn validating_conversions_are_derive_only() {
    assert!(matches!(
        Schema::<Mode>::new(),
        Err(Error::Untraceable { .. })
    ));
    assert!(matches!(
        Schema::<Settings>::new(),
        Err(Error::Untraceable { .. })
    ));

    // The derive is unaffected, and the schema is still the repr's.
    assert_eq!(Mode::schema(), String::schema().cast());
    assert_paths_interchangeable(&Mode::Fast);
    assert_paths_interchangeable(&settings());

    let bytes = carbonite::to_vec_static(&settings()).unwrap();
    assert_eq!(
        carbonite::from_slice_static::<Settings>(&bytes).unwrap(),
        settings()
    );
}

// ---------------------------------------------------------------------------
// Both writers, and the bytes of the repr written directly.
// ---------------------------------------------------------------------------

#[test]
fn both_paths_agree_on_converted_types() {
    assert_paths_interchangeable(&Degrees(37.5));
    assert_paths_interchangeable(&Range { lo: 0, hi: 0 });
    assert_paths_interchangeable(&Mode::Fast);
    assert_paths_interchangeable(&Sorted {
        values: vec![-3, 1, 4],
    });
    assert_paths_interchangeable(&Reversed {
        items: vec!["a".to_owned(), "b".to_owned()],
    });
    assert_paths_interchangeable(&settings());
    assert_paths_interchangeable(&vec![settings(), settings()]);
}

#[test]
fn the_bytes_are_the_reprs_bytes() {
    // A converted value and its repr are the same blob, so a peer holding only
    // the repr type reads it with no idea a conversion happened.
    let angle = Degrees(37.5);
    let converted = Serializer::new(&Degrees::schema())
        .to_vec_columns(&angle)
        .unwrap();
    let raw = Serializer::new(&f64::schema())
        .to_vec_columns(&37.5f64)
        .unwrap();
    assert_eq!(converted, raw);

    let bytes = carbonite::to_vec_static(&Mode::Small).unwrap();
    assert_eq!(
        carbonite::from_slice::<String>(&bytes).unwrap(),
        "small".to_owned()
    );

    // ...and the conversion still runs on the way back in.
    let sorted: Sorted = carbonite::from_slice_static(
        &carbonite::to_vec_static(&SortedRepr {
            values: vec![9, -1, 4],
        })
        .unwrap(),
    )
    .unwrap();
    assert_eq!(sorted.values, vec![-1, 4, 9]);
}

#[test]
fn conversions_run_on_both_read_paths() {
    // `from` normalizes: 400 degrees comes back as 40.
    let bytes = carbonite::to_vec_static(&400.0f64).unwrap();
    let (schema, data) = carbonite::peek_schema::<Degrees>(&bytes).unwrap();
    let de = Deserializer::new_static(schema);
    let via_serde: Degrees = de.from_slice(data).unwrap();
    let via_columnar: Degrees = de.from_slice_columns(data).unwrap();
    assert_eq!(via_serde, Degrees(40.0));
    assert_eq!(via_columnar, Degrees(40.0));
}

// ---------------------------------------------------------------------------
// Failing conversions.
// ---------------------------------------------------------------------------

#[test]
fn a_rejected_conversion_reports_the_same_error_on_both_paths() {
    let bytes = Serializer::new(&RangeRepr::schema())
        .to_vec_columns(&RangeRepr { lo: 8, hi: 2 })
        .unwrap();
    let de = Deserializer::new_static(Range::schema());

    let serde_err = de.from_slice(&bytes).map(|_: Range| ()).unwrap_err();
    let columnar_err = de
        .from_slice_columns(&bytes)
        .map(|_: Range| ())
        .unwrap_err();
    assert!(
        matches!(&serde_err, Error::Message(msg) if msg == "empty range 8..2"),
        "{serde_err}"
    );
    assert_eq!(serde_err.to_string(), columnar_err.to_string());
}

#[test]
fn a_rejected_conversion_inside_a_row_fails_the_row() {
    #[derive(Serialize, Deserialize, carbonite::Schema, PartialEq, Debug)]
    struct Holder {
        before: u8,
        span: Range,
        after: u8,
    }
    #[derive(Serialize, Deserialize, carbonite::Schema, PartialEq, Debug)]
    struct RawHolder {
        before: u8,
        span: RangeRepr,
        after: u8,
    }

    let bytes = carbonite::to_vec_static(&RawHolder {
        before: 1,
        span: RangeRepr { lo: 5, hi: 4 },
        after: 2,
    })
    .unwrap();
    assert!(matches!(
        carbonite::from_slice_static::<Holder>(&bytes),
        Err(Error::Message(_))
    ));
}

// ---------------------------------------------------------------------------
// How the repr may be spelled, and reaching it without serde's attributes.
// ---------------------------------------------------------------------------

/// The same type named two different ways. The derive checks `as` against
/// serde's conversion attributes through the type system rather than by
/// comparing the strings, so an equivalent path is not a false conflict.
#[derive(Serialize, Deserialize, carbonite::Schema, PartialEq, Debug, Clone)]
#[serde(from = "RangeRepr", into = "RangeRepr")]
#[carbonite(as = "crate::RangeRepr")]
struct Aliased {
    lo: u32,
    hi: u32,
}

impl From<RangeRepr> for Aliased {
    fn from(repr: RangeRepr) -> Self {
        Aliased {
            lo: repr.lo,
            hi: repr.hi,
        }
    }
}

impl From<Aliased> for RangeRepr {
    fn from(value: Aliased) -> RangeRepr {
        RangeRepr {
            lo: value.lo,
            hi: value.hi,
        }
    }
}

/// `as` describes the type's serde representation however it was arrived at, so
/// hand-written impls that delegate to a repr work with no serde attributes to
/// cross-check against.
#[derive(carbonite::Schema, PartialEq, Debug, Clone)]
#[carbonite(as = "f64")]
struct Manual(f64);

impl From<f64> for Manual {
    fn from(raw: f64) -> Self {
        Manual(raw)
    }
}

impl From<Manual> for f64 {
    fn from(value: Manual) -> f64 {
        value.0
    }
}

impl Serialize for Manual {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.0.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for Manual {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        f64::deserialize(deserializer).map(Manual)
    }
}

#[test]
fn an_equivalent_path_is_not_a_conflict() {
    assert_matches_trace::<Aliased>();
    assert_eq!(Aliased::schema(), RangeRepr::schema().cast());
    assert_paths_interchangeable(&Aliased { lo: 1, hi: 2 });
}

#[test]
fn hand_written_impls_can_declare_a_repr() {
    assert_matches_trace::<Manual>();
    assert_eq!(Manual::schema(), f64::schema().cast());
    assert_paths_interchangeable(&Manual(1.5));
    assert_eq!(
        carbonite::to_vec_static(&Manual(1.5)).unwrap(),
        carbonite::to_vec_static(&1.5f64).unwrap()
    );
}

// ---------------------------------------------------------------------------
// Evolution, and readers that know only the repr.
// ---------------------------------------------------------------------------

#[test]
fn evolution_applies_to_the_repr() {
    #[derive(Serialize, Deserialize, carbonite::Schema, PartialEq, Debug, Clone)]
    struct SpanV1 {
        lo: u32,
    }

    #[derive(Serialize, Deserialize, carbonite::Schema, PartialEq, Debug, Clone)]
    struct SpanV2 {
        lo: u32,
        #[serde(default)]
        hi: u32,
    }

    #[derive(Serialize, Deserialize, carbonite::Schema, PartialEq, Debug, Clone)]
    #[serde(from = "SpanV2", into = "SpanV2")]
    #[carbonite(as = "SpanV2")]
    struct Span {
        lo: u32,
        hi: u32,
    }

    impl From<SpanV2> for Span {
        fn from(repr: SpanV2) -> Self {
            Span {
                lo: repr.lo,
                hi: repr.hi,
            }
        }
    }

    impl From<Span> for SpanV2 {
        fn from(span: Span) -> SpanV2 {
            SpanV2 {
                lo: span.lo,
                hi: span.hi,
            }
        }
    }

    // An old file written before `hi` existed still reads: the schema in the
    // file is reconciled against the *repr*, exactly as for any other type.
    let old = carbonite::to_vec_static(&SpanV1 { lo: 3 }).unwrap();
    let span: Span = carbonite::from_slice(&old).unwrap();
    assert_eq!(span, Span { lo: 3, hi: 0 });
}

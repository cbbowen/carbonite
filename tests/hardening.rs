//! Untrusted-input hardening: every count a blob claims must be justified by
//! the bytes it actually carries.
//!
//! Each test here pins a specific way a tiny hand-built blob used to drive
//! unbounded work. They all assert the same shape of outcome: a clean
//! [`carbonite::Error`], never a panic, an abort, or an allocation out of
//! proportion to the input.

use carbonite::{Deserializer, Error, Schema, Serializer, StaticSchema};
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Blob-building helpers.
// ---------------------------------------------------------------------------

fn varint(out: &mut Vec<u8>, mut value: u64) {
    loop {
        let byte = (value & 0x7f) as u8;
        value >>= 7;
        if value == 0 {
            out.push(byte);
            return;
        }
        out.push(byte | 0x80);
    }
}

/// Assembles a blob header and columns the way `Batch::finish` does.
fn blob(rows: u64, columns: &[Vec<u8>]) -> Vec<u8> {
    let mut out = Vec::new();
    varint(&mut out, rows);
    varint(&mut out, columns.len() as u64);
    for column in columns {
        varint(&mut out, column.len() as u64);
    }
    for column in columns {
        out.extend_from_slice(column);
    }
    out
}

fn column(values: &[u64]) -> Vec<u8> {
    let mut out = Vec::new();
    for &value in values {
        varint(&mut out, value);
    }
    out
}

#[track_caller]
fn assert_limit_exceeded<T: std::fmt::Debug>(result: Result<T, Error>) {
    match result {
        Err(Error::LimitExceeded { .. }) => {}
        other => panic!("expected Error::LimitExceeded, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Types under test.
// ---------------------------------------------------------------------------

#[derive(Serialize, Deserialize, carbonite::Schema, PartialEq, Debug, Clone)]
struct Reading {
    sensor: u16,
    celsius: f32,
}

/// Occupies no columns at all, so nothing in the data bounds how many of it a
/// header can claim.
#[derive(Serialize, Deserialize, carbonite::Schema, PartialEq, Debug)]
struct Marker;

// ---------------------------------------------------------------------------
// Row counts.
// ---------------------------------------------------------------------------

/// A blob whose header claims far more rows than its columns could hold used
/// to be accepted, and `Rows::size_hint` reported the claim verbatim —
/// collecting it asked the allocator for 48 PB and aborted the process.
#[test]
fn tampered_row_count_is_rejected_when_the_blob_is_opened() {
    let schema = Reading::schema();
    let good = Serializer::new(&schema)
        .to_vec(&Reading {
            sensor: 1,
            celsius: 2.0,
        })
        .unwrap();

    // Rewrite only the leading row-count varint; every column stays valid.
    let mut tampered = Vec::new();
    varint(&mut tampered, 1_000_000_000_000_000);
    tampered.extend_from_slice(&good[1..]);
    assert!(tampered.len() < 32, "the whole attack fits in a few bytes");

    let de = Deserializer::new_static(Reading::schema());
    assert_limit_exceeded(de.rows(&tampered).map(|_| ()));
    assert_limit_exceeded::<Reading>(de.from_slice(&tampered));
    assert_limit_exceeded(de.rows_columns(&tampered).map(|_| ()));
    assert_limit_exceeded::<Reading>(de.from_slice_columns(&tampered));
}

/// The lower bound must stay at zero: a row that has not been decoded yet may
/// still fail, so no consumer should size an allocation from the claim.
#[test]
fn rows_size_hint_never_promises_rows_it_has_not_decoded() {
    let schema = Reading::schema();
    let ser = Serializer::new(&schema);
    let mut batch = ser.batch();
    for sensor in 0..4 {
        batch
            .push_columns(&Reading {
                sensor,
                celsius: 1.0,
            })
            .unwrap();
    }
    let bytes = batch.finish();

    let de = Deserializer::new_static(Reading::schema());
    let rows = de.rows(&bytes).unwrap();
    assert_eq!(rows.size_hint(), (0, Some(4)));

    let rows: carbonite::RowsColumns<'_, Reading> = de.rows_columns(&bytes).unwrap();
    assert_eq!(rows.size_hint(), (0, Some(4)));
}

/// A zero-column row type carries nothing per row, so the count is capped
/// rather than bounded by the data.
#[test]
fn zero_column_row_count_is_capped() {
    let de = Deserializer::new_static(Marker::schema());

    let ok = blob(1_000, &[]);
    assert_eq!(de.rows(&ok).unwrap().count(), 1_000);

    let absurd = blob(carbonite::columnar::MAX_ZERO_COLUMN_REPEAT + 1, &[]);
    assert!(absurd.len() < 16);
    assert_limit_exceeded(de.rows(&absurd).map(|_| ()));
}

// ---------------------------------------------------------------------------
// Sequence and map lengths.
// ---------------------------------------------------------------------------

/// An 8-byte blob used to decode into a 500-million-element `Vec`, spending
/// 16 seconds to do it, because a zero-column element consumes no input.
#[test]
fn zero_column_sequence_length_is_capped() {
    let de = Deserializer::new_static(<Vec<Marker>>::schema());

    let ok = blob(1, &[column(&[1_000])]);
    let decoded: Vec<Marker> = de.from_slice(&ok).unwrap();
    assert_eq!(decoded.len(), 1_000);

    let absurd = blob(1, &[column(&[500_000_000])]);
    assert!(absurd.len() < 16);
    assert_limit_exceeded::<Vec<Marker>>(de.from_slice(&absurd));
}

/// For an element that does occupy columns, the remaining bytes are an exact
/// bound: a `u32` element cannot appear more often than there are bytes left.
#[test]
fn sequence_length_is_bounded_by_the_remaining_bytes() {
    let de = Deserializer::new_static(<Vec<u32>>::schema());

    let ok = blob(1, &[column(&[2]), vec![0; 8]]);
    let decoded: Vec<u32> = de.from_slice(&ok).unwrap();
    assert_eq!(decoded, vec![0, 0]);

    // Claims 500M elements but supplies eight bytes of payload.
    let absurd = blob(1, &[column(&[500_000_000]), vec![0; 8]]);
    assert_limit_exceeded::<Vec<u32>>(de.from_slice(&absurd));
}

#[test]
fn map_length_is_bounded_by_the_remaining_bytes() {
    type Map = std::collections::BTreeMap<u8, u8>;
    let de = Deserializer::new_static(Map::schema());

    let ok = blob(1, &[column(&[2]), vec![1, 2], vec![3, 4]]);
    let decoded: Map = de.from_slice(&ok).unwrap();
    assert_eq!(decoded, Map::from([(1, 3), (2, 4)]));

    let absurd = blob(1, &[column(&[500_000_000]), vec![1, 2], vec![3, 4]]);
    assert_limit_exceeded::<Map>(de.from_slice(&absurd));
}

/// The same bound has to apply when a field is being *skipped* — the
/// evolution path walks the writer's columns without materializing them, and
/// that walk is just as much of a loop.
#[test]
fn lengths_are_bounded_while_skipping_a_removed_field() {
    #[derive(Serialize, Deserialize, carbonite::Schema)]
    struct V1 {
        keep: u8,
        dropped: Vec<u32>,
    }
    #[derive(Serialize, Deserialize, carbonite::Schema, PartialEq, Debug)]
    struct V2 {
        keep: u8,
    }

    let writer: Schema<V2> = V1::schema().cast();
    let de = Deserializer::new(writer);

    // Columns: keep(u8), dropped length, dropped u32 payload.
    let ok = blob(1, &[vec![7], column(&[1]), vec![0; 4]]);
    let decoded: V2 = de.from_slice(&ok).unwrap();
    assert_eq!(decoded, V2 { keep: 7 });

    let absurd = blob(1, &[vec![7], column(&[500_000_000]), vec![0; 4]]);
    assert_limit_exceeded::<V2>(de.from_slice(&absurd));
}

// ---------------------------------------------------------------------------
// Canonical encodings.
// ---------------------------------------------------------------------------

/// Overlong varints gave a value more than one valid encoding, so decoding
/// and re-encoding a blob was not guaranteed to reproduce its bytes.
#[test]
fn non_canonical_varints_are_rejected() {
    let de = Deserializer::new_static(<Vec<u8>>::schema());

    let canonical = blob(1, &[vec![0x02], vec![9, 9]]);
    let decoded: Vec<u8> = de.from_slice(&canonical).unwrap();
    assert_eq!(decoded, vec![9, 9]);

    // The same length of 2, padded to two bytes.
    let padded = blob(1, &[vec![0x82, 0x00], vec![9, 9]]);
    let padded_result: Result<Vec<u8>, Error> = de.from_slice(&padded);
    assert!(matches!(padded_result, Err(Error::InvalidVarint)));
}

/// Round-tripping a blob must reproduce it byte for byte.
#[test]
fn decoding_and_re_encoding_reproduces_the_bytes() {
    let schema = Reading::schema();
    let ser = Serializer::new(&schema);
    let mut batch = ser.batch();
    for sensor in 0..8 {
        batch
            .push_columns(&Reading {
                sensor,
                celsius: sensor as f32 * 0.5,
            })
            .unwrap();
    }
    let original = batch.finish();

    let de = Deserializer::new_static(Reading::schema());
    let rows: Vec<Reading> = de.rows(&original).unwrap().collect::<Result<_, _>>().unwrap();

    let mut again = ser.batch();
    for row in &rows {
        again.push_columns(row).unwrap();
    }
    assert_eq!(again.finish(), original);
}

// ---------------------------------------------------------------------------
// Truncation and corruption never panic.
// ---------------------------------------------------------------------------

/// Every prefix of a valid blob, and every single-byte corruption of one,
/// must produce an error rather than a panic.
#[test]
fn truncation_and_corruption_stay_clean() {
    #[derive(Serialize, Deserialize, carbonite::Schema, PartialEq, Debug)]
    struct Wide {
        id: u32,
        name: String,
        tags: Vec<Option<u8>>,
        pair: (bool, char),
    }

    let value = Wide {
        id: 7,
        name: "carbonite".to_owned(),
        tags: vec![Some(1), None, Some(3)],
        pair: (true, 'z'),
    };
    let good = carbonite::to_vec_static(&value).unwrap();
    assert_eq!(carbonite::from_slice_static::<Wide>(&good).unwrap(), value);

    for cut in 0..good.len() {
        let _ = carbonite::from_slice_static::<Wide>(&good[..cut]);
    }
    for index in 0..good.len() {
        for bit in 0..8 {
            let mut corrupted = good.clone();
            corrupted[index] ^= 1 << bit;
            let _ = carbonite::from_slice_static::<Wide>(&corrupted);
        }
    }
}

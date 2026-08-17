//! The public API surface: the shapes and guarantees downstream code is
//! allowed to depend on.

use std::fmt::Debug;

use carbonite::{
    Batch, ColumnCursor, Deserializer, Error, Rows, RowsColumns, Schema,
    SelfDescribingDeserializer, SelfDescribingSerializer, Serializer, StaticSchema,
};
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, carbonite::Schema, PartialEq, Debug, Clone)]
struct Reading {
    sensor: u16,
    celsius: f32,
}

fn readings(n: u16) -> Vec<Reading> {
    (0..n)
        .map(|sensor| Reading {
            sensor,
            celsius: f32::from(sensor) * 0.25,
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Trait shape.
// ---------------------------------------------------------------------------

/// The column count lives on `StaticSchema` alone. When both columnar traits
/// declared their own, `T::columns()` was ambiguous (E0034) and the two could
/// silently disagree about a type's layout.
#[test]
fn columns_is_declared_once() {
    assert_eq!(Reading::columns(), 2);
    assert_eq!(<Vec<Reading>>::columns(), 3);
    assert_eq!(<Option<String>>::columns(), 3);
    assert_eq!(<()>::columns(), 0);
}

#[test]
fn handle_types_implement_debug() {
    fn assert_debug<T: Debug>() {}
    assert_debug::<Serializer<'_, Reading>>();
    assert_debug::<Deserializer<Reading>>();
    assert_debug::<Batch<'_, Reading>>();
    assert_debug::<Rows<'_, '_, Reading>>();
    assert_debug::<RowsColumns<'_, Reading>>();
    assert_debug::<SelfDescribingSerializer<'_, Reading>>();
    assert_debug::<SelfDescribingDeserializer<Reading>>();
    assert_debug::<ColumnCursor<'_>>();
    assert_debug::<Schema<Reading>>();
    assert_debug::<Error>();

    // Debug output should say something useful, not just the type name.
    let schema = Reading::schema();
    let rendered = format!("{:?}", Serializer::new(&schema));
    assert!(rendered.contains("Reading"), "{rendered}");
    assert!(rendered.contains("columns"), "{rendered}");
}

#[test]
fn row_iterators_are_fused() {
    fn assert_fused<I: std::iter::FusedIterator>(_: &I) {}
    let schema = Reading::schema();
    let bytes = Serializer::new(&schema)
        .to_vec_columns(&readings(1)[0])
        .unwrap();
    let de = Deserializer::new_static(Reading::schema());
    assert_fused(&de.rows(&bytes).unwrap());
    let columns: RowsColumns<'_, Reading> = de.rows_columns(&bytes).unwrap();
    assert_fused(&columns);
}

/// The documented contract for manual columnar impls is only meaningful if a
/// downstream crate can build a cursor to test against.
#[test]
fn column_cursor_is_constructible_downstream() {
    let mut cursor = ColumnCursor::new(&[0x2a, 0, 0, 0]);
    assert_eq!(cursor.remaining(), 4);
    assert_eq!(u32::from_le_bytes(cursor.fixed().unwrap()), 42);
    assert!(cursor.at_end());
}

// ---------------------------------------------------------------------------
// The columnar fast path, in both directions.
// ---------------------------------------------------------------------------

/// A batch written with `push_columns` must be readable through the
/// monomorphized reader, not only through the serde one.
#[test]
fn rows_columns_reads_a_batch_written_with_push_columns() {
    let schema = Reading::schema();
    let ser = Serializer::new(&schema);
    let mut batch = ser.batch();
    let expected = readings(64);
    for reading in &expected {
        batch.push_columns(reading).unwrap();
    }
    let bytes = batch.finish();

    let de = Deserializer::new_static(Reading::schema());
    let via_columns: Vec<Reading> = de
        .rows_columns(&bytes)
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    let via_serde: Vec<Reading> = de.rows(&bytes).unwrap().collect::<Result<_, _>>().unwrap();

    assert_eq!(via_columns, expected);
    assert_eq!(via_serde, expected);
}

#[test]
fn rows_columns_rejects_a_foreign_schema() {
    #[derive(Serialize, Deserialize, carbonite::Schema)]
    struct Older {
        sensor: u16,
    }

    let bytes = carbonite::to_vec_static(&Older { sensor: 1 }).unwrap();
    let (schema, data) = carbonite::peek_schema::<Reading>(&bytes).unwrap();
    let de = Deserializer::new_static(schema);
    let attempt: Result<RowsColumns<'_, Reading>, _> = de.rows_columns(data);
    assert!(matches!(attempt, Err(Error::SchemaMismatch { .. })));
}

/// The self-describing writer must be able to use the fast path too, and
/// produce the same bytes as the serde writer.
#[test]
fn self_describing_columnar_writer_matches_the_serde_writer() {
    let schema = Reading::schema();
    let ser = SelfDescribingSerializer::new(&schema);
    let value = Reading {
        sensor: 3,
        celsius: 1.5,
    };
    assert_eq!(
        ser.to_vec_columns(&value).unwrap(),
        ser.to_vec(&value).unwrap()
    );
    assert_eq!(
        carbonite::to_vec_static(&value).unwrap(),
        carbonite::to_vec(&value).unwrap()
    );

    let back: Reading =
        carbonite::from_slice_static(&carbonite::to_vec_static(&value).unwrap()).unwrap();
    assert_eq!(back, value);
}

// ---------------------------------------------------------------------------
// Buffer reuse.
// ---------------------------------------------------------------------------

#[test]
fn finish_into_reuses_the_column_buffers() {
    let schema = Reading::schema();
    let ser = Serializer::new(&schema);
    let mut batch = ser.batch();
    let mut blob = Vec::new();

    for reading in &readings(4) {
        blob.clear();
        batch.push_columns(reading).unwrap();
        batch.finish_into(&mut blob);
        assert_eq!(batch.rows(), 0, "finish_into leaves the batch empty");

        let de = Deserializer::new_static(Reading::schema());
        let back: Reading = de.from_slice_columns(&blob).unwrap();
        assert_eq!(&back, reading);
    }
}

#[test]
fn reset_discards_pushed_rows() {
    let schema = Reading::schema();
    let ser = Serializer::new(&schema);
    let mut batch = ser.batch();
    batch.push_columns(&readings(1)[0]).unwrap();
    assert_eq!(batch.rows(), 1);
    batch.reset();
    assert_eq!(batch.rows(), 0);
    assert_eq!(batch.finish(), ser.batch().finish());
}

// ---------------------------------------------------------------------------
// Versioning and framing.
// ---------------------------------------------------------------------------

#[test]
fn schema_bytes_lead_with_the_schema_version() {
    let bytes = Reading::schema().to_bytes();
    assert_eq!(bytes[0] as u64, carbonite::SCHEMA_VERSION);
    assert_eq!(
        Schema::<Reading>::from_bytes(&bytes).unwrap(),
        Reading::schema()
    );
}

#[test]
fn a_newer_schema_version_is_rejected_rather_than_misread() {
    let mut bytes = Reading::schema().to_bytes();
    bytes[0] = (carbonite::SCHEMA_VERSION + 1) as u8;
    assert!(matches!(
        Schema::<Reading>::from_bytes(&bytes),
        Err(Error::UnsupportedVersion { what: "schema", .. })
    ));
}

#[test]
fn a_newer_frame_version_is_rejected_rather_than_misread() {
    let mut bytes = carbonite::to_vec_static(&readings(1)[0]).unwrap();
    bytes[carbonite::MAGIC.len()] = (carbonite::FORMAT_VERSION + 1) as u8;
    assert!(matches!(
        carbonite::from_slice_static::<Reading>(&bytes),
        Err(Error::UnsupportedVersion { what: "frame", .. })
    ));
}

#[test]
fn a_frame_can_be_sniffed_and_its_schema_read_without_decoding() {
    let value = readings(1).remove(0);
    let bytes = carbonite::to_vec_static(&value).unwrap();

    assert!(carbonite::is_self_describing(&bytes));
    assert!(!carbonite::is_self_describing(b"not carbonite"));

    let (schema, data) = carbonite::peek_schema::<Reading>(&bytes).unwrap();
    assert_eq!(schema, Reading::schema());
    assert_eq!(schema.to_string(), "struct `Reading`");

    // The data half is a plain blob the writer's schema can decode.
    let back: Reading = Deserializer::new_static(schema).from_slice(data).unwrap();
    assert_eq!(back, value);

    assert!(matches!(
        carbonite::peek_schema::<Reading>(b"nope"),
        Err(Error::Malformed(_))
    ));
}

#[test]
fn schema_survives_a_round_trip_through_another_serde_format() {
    let schema = Reading::schema();
    let json = serde_json::to_string(&schema).unwrap();
    let back: Schema<Reading> = serde_json::from_str(&json).unwrap();
    assert_eq!(back, schema);
}

// ---------------------------------------------------------------------------
// Diagnostics.
// ---------------------------------------------------------------------------

#[test]
fn the_deserializer_reports_which_path_it_took() {
    #[derive(Serialize, Deserialize, carbonite::Schema)]
    struct Older {
        sensor: u16,
    }

    assert!(Deserializer::new_static(Reading::schema()).uses_fast_path());

    let older: Schema<Reading> = Older::schema().cast();
    assert!(!Deserializer::new_static(older).uses_fast_path());
    assert!(!Deserializer::new_untraced(Reading::schema()).uses_fast_path());
}

#[test]
fn schema_displays_its_shape() {
    assert_eq!(Reading::schema().to_string(), "struct `Reading`");
    assert_eq!(<Vec<Reading>>::schema().to_string(), "sequence");
    assert_eq!(<Option<u8>>::schema().to_string(), "option<u8>");
}

// ---------------------------------------------------------------------------
// A renamed carbonite dependency.
// ---------------------------------------------------------------------------

mod renamed_dependency {
    use carbonite as cbn;
    use serde::{Deserialize, Serialize};

    #[derive(Serialize, Deserialize, cbn::Schema, PartialEq, Debug)]
    #[carbonite(crate = "cbn")]
    pub struct Pixel {
        pub x: u16,
        pub luma: f32,
    }

    #[derive(Serialize, Deserialize, cbn::Schema, PartialEq, Debug)]
    #[carbonite(crate = "cbn")]
    pub enum Shade {
        Flat,
        Ramp(u8, u8),
        Named { label: String },
    }
}

/// The generated code must not assume the crate is called `carbonite`.
#[test]
fn a_renamed_carbonite_dependency_still_derives() {
    use renamed_dependency::{Pixel, Shade};

    let pixel = Pixel { x: 4, luma: 0.5 };
    let bytes = carbonite::to_vec_static(&pixel).unwrap();
    assert_eq!(
        carbonite::from_slice_static::<Pixel>(&bytes).unwrap(),
        pixel
    );
    assert_eq!(Pixel::schema(), Schema::<Pixel>::new().unwrap());

    let shade = Shade::Named {
        label: "dusk".to_owned(),
    };
    let bytes = carbonite::to_vec_static(&shade).unwrap();
    assert_eq!(
        carbonite::from_slice_static::<Shade>(&bytes).unwrap(),
        shade
    );
    assert_eq!(Shade::schema(), Schema::<Shade>::new().unwrap());
}

// ---------------------------------------------------------------------------
// Schema ownership.
// ---------------------------------------------------------------------------

/// Every engine takes its schema borrowed (`&schema`, to share one long-lived
/// schema) or owned (`schema`, when it was parsed from the wire for this
/// engine alone). Both forms must keep compiling.
#[test]
fn engines_take_schemas_borrowed_or_owned() {
    let value = Reading {
        sensor: 1,
        celsius: 20.0,
    };
    let schema = Reading::schema();

    let borrowed = Serializer::new(&schema).to_vec_columns(&value).unwrap();
    let owned = Serializer::new(Reading::schema())
        .to_vec_columns(&value)
        .unwrap();
    assert_eq!(borrowed, owned);

    let de_borrowed = Deserializer::new_static(&schema);
    let de_owned = Deserializer::new_static(Reading::schema());
    let via_borrowed: Reading = de_borrowed.from_slice_columns(&borrowed).unwrap();
    let via_owned: Reading = de_owned.from_slice_columns(&owned).unwrap();
    assert_eq!(via_borrowed, via_owned);

    let framed = SelfDescribingSerializer::new(&schema)
        .to_vec_columns(&value)
        .unwrap();
    assert_eq!(
        framed,
        SelfDescribingSerializer::new(Reading::schema())
            .to_vec_columns(&value)
            .unwrap()
    );
}

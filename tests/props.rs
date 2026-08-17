//! Property tests: arbitrary values of a rich fixture type must round-trip.

use std::collections::BTreeMap;

use proptest::prelude::*;
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, carbonite::Schema, PartialEq, Debug, Clone)]
enum Shade {
    Plain,
    Tinted(u8),
    Custom { red: f32, alpha: Option<f64> },
}

#[derive(Serialize, Deserialize, carbonite::Schema, PartialEq, Debug, Clone)]
struct Item {
    id: u64,
    label: String,
    shade: Shade,
    points: Vec<(i32, i32)>,
    blob: Vec<u8>,
    table: BTreeMap<String, u16>,
}

/// Adjacent and nested variable-length collections, including elements that
/// occupy no columns at all. Decoding bounds every claimed length against the
/// bytes remaining across all columns; this type is where that bound is
/// loosest, so it is where a false rejection would show up first.
#[derive(Serialize, Deserialize, carbonite::Schema, PartialEq, Debug, Clone)]
struct Nested {
    grid: Vec<Vec<u8>>,
    flags: Vec<Option<()>>,
    units: Vec<()>,
    pairs: Vec<(Vec<u16>, String)>,
    lookup: BTreeMap<u8, Vec<u8>>,
}

fn nested_strategy() -> impl Strategy<Value = Nested> {
    (
        proptest::collection::vec(proptest::collection::vec(any::<u8>(), 0..6), 0..6),
        proptest::collection::vec(proptest::option::of(Just(())), 0..8),
        proptest::collection::vec(Just(()), 0..8),
        proptest::collection::vec(
            (proptest::collection::vec(any::<u16>(), 0..4), ".{0,4}"),
            0..4,
        ),
        proptest::collection::btree_map(
            any::<u8>(),
            proptest::collection::vec(any::<u8>(), 0..4),
            0..4,
        ),
    )
        .prop_map(|(grid, flags, units, pairs, lookup)| Nested {
            grid,
            flags,
            units,
            pairs,
            lookup,
        })
}

fn shade_strategy() -> impl Strategy<Value = Shade> {
    prop_oneof![
        Just(Shade::Plain),
        any::<u8>().prop_map(Shade::Tinted),
        (-1e30f32..1e30f32, proptest::option::of(-1e300f64..1e300f64))
            .prop_map(|(red, alpha)| Shade::Custom { red, alpha }),
    ]
}

fn item_strategy() -> impl Strategy<Value = Item> {
    (
        any::<u64>(),
        ".{0,12}",
        shade_strategy(),
        proptest::collection::vec((any::<i32>(), any::<i32>()), 0..8),
        proptest::collection::vec(any::<u8>(), 0..32),
        proptest::collection::btree_map(".{0,6}", any::<u16>(), 0..6),
    )
        .prop_map(|(id, label, shade, points, blob, table)| Item {
            id,
            label,
            shade,
            points,
            blob,
            table,
        })
}

/// A type with serde impls and no carbonite derive, so it has no
/// `StaticSchema` and can only reach a derived struct through
/// `#[carbonite(serde)]`.
#[derive(Serialize, Deserialize, PartialEq, Debug, Clone)]
struct Payload {
    tag: Option<u16>,
    parts: Vec<String>,
    table: BTreeMap<u8, i64>,
}

#[derive(Serialize, Deserialize, carbonite::Schema, PartialEq, Debug, Clone)]
struct Envelope {
    item: Item,
    #[carbonite(serde)]
    payload: Payload,
    trailer: u32,
}

fn envelope_strategy() -> impl Strategy<Value = Envelope> {
    (
        item_strategy(),
        (
            proptest::option::of(any::<u16>()),
            proptest::collection::vec(".{0,6}", 0..5),
            proptest::collection::btree_map(any::<u8>(), any::<i64>(), 0..5),
        )
            .prop_map(|(tag, parts, table)| Payload { tag, parts, table }),
        any::<u32>(),
    )
        .prop_map(|(item, payload, trailer)| Envelope {
            item,
            payload,
            trailer,
        })
}

proptest! {
    #[test]
    fn arbitrary_values_round_trip(items in proptest::collection::vec(item_strategy(), 0..10)) {
        let bytes = carbonite::to_vec(&items).unwrap();
        let back: Vec<Item> = carbonite::from_slice(&bytes).unwrap();
        prop_assert_eq!(back, items);
    }

    #[test]
    fn columnar_path_matches_serde_path(items in proptest::collection::vec(item_strategy(), 0..10)) {
        use carbonite::StaticSchema;
        let schema = <Vec<Item>>::schema();
        let ser = carbonite::Serializer::new(&schema);
        let serde_bytes = ser.to_vec(&items).unwrap();
        let columnar_bytes = ser.to_vec_columns(&items).unwrap();
        prop_assert_eq!(&serde_bytes, &columnar_bytes);

        let de = carbonite::Deserializer::new_static(schema);
        let back: Vec<Item> = de.from_slice_columns(&columnar_bytes).unwrap();
        prop_assert_eq!(back, items);
    }

    /// A `#[carbonite(serde)]` field must be indistinguishable from a traced
    /// one: same schema, same bytes from either writer, and readable by a type
    /// that never heard of the attribute.
    #[test]
    fn fallback_fields_are_indistinguishable(
        values in proptest::collection::vec(envelope_strategy(), 0..8)
    ) {
        use carbonite::StaticSchema;

        let schema = <Vec<Envelope>>::schema();
        prop_assert_eq!(&schema, &carbonite::Schema::<Vec<Envelope>>::new().unwrap());

        let ser = carbonite::Serializer::new(&schema);
        let columnar_bytes = ser.to_vec_columns(&values).unwrap();
        prop_assert_eq!(&columnar_bytes, &ser.to_vec(&values).unwrap());

        let de = carbonite::Deserializer::new_static(schema);
        let via_serde: Vec<Envelope> = de.from_slice(&columnar_bytes).unwrap();
        let via_columns: Vec<Envelope> = de.from_slice_columns(&columnar_bytes).unwrap();
        prop_assert_eq!(&via_serde, &values);
        prop_assert_eq!(&via_columns, &values);

        // A reader that skips the whole row must still walk the fallback
        // field's columns exactly.
        let framed = carbonite::to_vec_static(&values).unwrap();
        let ignored: Vec<serde::de::IgnoredAny> = carbonite::from_slice(&framed).unwrap();
        prop_assert_eq!(ignored.len(), values.len());
    }

    /// Nothing carbonite itself writes may be rejected by the length bounds
    /// it enforces on read, on either path.
    #[test]
    fn nested_collections_are_never_falsely_rejected(
        values in proptest::collection::vec(nested_strategy(), 0..6)
    ) {
        use carbonite::StaticSchema;

        let schema = <Vec<Nested>>::schema();
        let ser = carbonite::Serializer::new(&schema);
        let bytes = ser.to_vec_columns(&values).unwrap();
        prop_assert_eq!(&bytes, &ser.to_vec(&values).unwrap());

        let de = carbonite::Deserializer::new_static(schema);
        let via_serde: Vec<Nested> = de.from_slice(&bytes).unwrap();
        let via_columns: Vec<Nested> = de.from_slice_columns(&bytes).unwrap();
        prop_assert_eq!(&via_serde, &values);
        prop_assert_eq!(&via_columns, &values);

        // ...and the same values must survive being skipped by a reader that
        // does not know the field, which walks the columns without decoding.
        let framed = carbonite::to_vec_static(&values).unwrap();
        let ignored: Vec<serde::de::IgnoredAny> = carbonite::from_slice(&framed).unwrap();
        prop_assert_eq!(ignored.len(), values.len());
    }

    #[test]
    fn schema_bytes_round_trip(items in proptest::collection::vec(item_strategy(), 0..4)) {
        // The schema's stable encoding must survive a byte round-trip and
        // still decode the data.
        let schema = carbonite::Schema::<Vec<Item>>::new().unwrap();
        let blob = carbonite::Serializer::new(&schema).to_vec(&items).unwrap();
        let schema2 = carbonite::Schema::<Vec<Item>>::from_bytes(&schema.to_bytes()).unwrap();
        prop_assert_eq!(&schema2, &schema);
        let back: Vec<Item> = carbonite::Deserializer::new(schema2).from_slice(&blob).unwrap();
        prop_assert_eq!(back, items);
    }
}

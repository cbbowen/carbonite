//! Property tests: arbitrary values of a rich fixture type must round-trip.

use std::collections::BTreeMap;

use proptest::prelude::*;
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, PartialEq, Debug, Clone)]
enum Shade {
    Plain,
    Tinted(u8),
    Custom { red: f32, alpha: Option<f64> },
}

#[derive(Serialize, Deserialize, PartialEq, Debug, Clone)]
struct Item {
    id: u64,
    label: String,
    shade: Shade,
    points: Vec<(i32, i32)>,
    blob: Vec<u8>,
    table: BTreeMap<String, u16>,
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

proptest! {
    #[test]
    fn arbitrary_values_round_trip(items in proptest::collection::vec(item_strategy(), 0..10)) {
        let bytes = carbonite::to_vec(&items).unwrap();
        let back: Vec<Item> = carbonite::from_slice(&bytes).unwrap();
        prop_assert_eq!(back, items);
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

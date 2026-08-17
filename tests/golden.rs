//! Golden byte-layout tests: lock in the wire format and prove the encoding
//! really is columnar.

use carbonite::{Deserializer, Schema, Serializer};

#[test]
fn vec_of_pairs_is_stored_column_wise() {
    let value: Vec<(u32, f32)> = vec![(1, 1.5), (2, 2.5)];
    let schema = Schema::<Vec<(u32, f32)>>::new().unwrap();
    let blob = Serializer::new(&schema).to_vec(&value).unwrap();

    #[rustfmt::skip]
    let expected: Vec<u8> = vec![
        // header
        1,                      // row count
        3,                      // column count
        1, 8, 8,                // column byte lengths
        // column 0: sequence length
        2,
        // column 1: both u32s, contiguous, little-endian
        1, 0, 0, 0,
        2, 0, 0, 0,
        // column 2: both f32s, contiguous, little-endian
        0x00, 0x00, 0xC0, 0x3F, // 1.5
        0x00, 0x00, 0x20, 0x40, // 2.5
    ];
    assert_eq!(blob, expected);

    let back: Vec<(u32, f32)> = Deserializer::new(schema).from_slice(&blob).unwrap();
    assert_eq!(back, value);
}

#[test]
fn strings_share_one_data_column() {
    let value: Vec<String> = vec!["ab".into(), "".into(), "cde".into()];
    let schema = Schema::<Vec<String>>::new().unwrap();
    let blob = Serializer::new(&schema).to_vec(&value).unwrap();

    #[rustfmt::skip]
    let expected: Vec<u8> = vec![
        1,                      // row count
        3,                      // columns: seq len, string lens, string bytes
        1, 3, 5,                // column byte lengths
        3,                      // three strings
        2, 0, 3,                // per-string byte lengths
        b'a', b'b', b'c', b'd', b'e',
    ];
    assert_eq!(blob, expected);
}

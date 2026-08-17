//! Hostile bytes through the schema decoder, plus the re-encode identity for
//! anything that decodes.
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok(schema) = carbonite::Schema::<()>::from_bytes(data) {
        // Whatever decodes must re-encode to the same bytes.
        assert_eq!(schema.to_bytes(), data);
    }
});

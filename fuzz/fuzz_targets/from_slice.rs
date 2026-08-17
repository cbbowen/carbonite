//! Self-describing frames from hostile bytes, through the serde-driven
//! (evolution) reader: the input controls the schema *and* the data.
#![no_main]

use carbonite_fuzz::Kitchen;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _ = carbonite::from_slice::<Kitchen>(data);
});

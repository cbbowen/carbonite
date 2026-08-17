//! Hostile blobs through the monomorphized columnar reader, against the
//! type's own schema.
#![no_main]

use carbonite_fuzz::Kitchen;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _ = carbonite::from_slice_static::<Kitchen>(data);
});

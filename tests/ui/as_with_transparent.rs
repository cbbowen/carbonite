//! Two declarations of the wire type cannot coexist.

#[derive(serde::Serialize, carbonite::Schema)]
#[serde(transparent)]
#[carbonite(as = "u32")]
struct Wrapper(u32);

fn main() {}

//! serde(remote) generates conversion functions, not impls to hang a schema on.

#[derive(serde::Serialize, carbonite::Schema)]
#[serde(remote = "std::time::Duration")]
struct DurationDef {
    secs: u64,
    nanos: u32,
}

fn main() {}

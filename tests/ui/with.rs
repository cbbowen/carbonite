//! serde(with) hides the field's wire shape from the derive.

#[derive(serde::Serialize, carbonite::Schema)]
struct Timestamped {
    #[serde(with = "some_module")]
    at: u64,
}

fn main() {}

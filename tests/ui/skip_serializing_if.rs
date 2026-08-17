//! Columnar rows must be complete.

#[derive(serde::Serialize, carbonite::Schema)]
struct Sparse {
    #[serde(skip_serializing_if = "Option::is_none")]
    note: Option<String>,
}

fn main() {}

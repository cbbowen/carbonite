//! serde(flatten) requires a self-describing format.

#[derive(serde::Deserialize, carbonite::Schema)]
struct Outer {
    id: u32,
    #[serde(flatten)]
    rest: std::collections::BTreeMap<String, u32>,
}

fn main() {}

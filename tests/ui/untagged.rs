//! Untagged enums have no columnar layout.

#[derive(serde::Deserialize, carbonite::Schema)]
#[serde(untagged)]
enum Loose {
    Num(u32),
    Text(String),
}

fn main() {}

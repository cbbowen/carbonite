//! An alias claims a name just as a field name does — including the `"0"`
//! form that claims a position.

#[derive(serde::Deserialize, carbonite::Schema)]
#[carbonite(removed("hp", 1))]
struct Save {
    id: u32,
    #[serde(alias = "hp")]
    health: f32,
}

#[derive(serde::Deserialize, carbonite::Schema)]
#[carbonite(removed(1))]
struct Point {
    #[serde(alias = "0")]
    x: f32,
    #[serde(alias = "1")]
    y: f32,
}

fn main() {}

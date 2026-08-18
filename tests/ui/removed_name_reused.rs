//! A new field may not take a retired field's name.

#[derive(serde::Deserialize, carbonite::Schema)]
#[carbonite(removed("hp"))]
struct Save {
    id: u32,
    #[serde(default)]
    hp: f32,
}

fn main() {}

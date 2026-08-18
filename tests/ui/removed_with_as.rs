//! With `as`, this type's own fields never reach the wire.

#[derive(serde::Deserialize, carbonite::Schema)]
#[serde(from = "u32")]
#[carbonite(as = "u32", removed("hp"))]
struct Level(u32);

impl From<u32> for Level {
    fn from(value: u32) -> Self {
        Level(value)
    }
}

fn main() {}

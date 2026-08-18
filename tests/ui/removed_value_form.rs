//! Retirements accumulate, so they are written as a list.

#[derive(serde::Deserialize, carbonite::Schema)]
#[carbonite(removed = "hp")]
struct Save {
    id: u32,
}

fn main() {}

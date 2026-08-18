//! A retirement names a slot with no field left, so it goes on the container.

#[derive(serde::Deserialize, carbonite::Schema)]
struct Save {
    id: u32,
    #[carbonite(removed)]
    hp: (),
}

fn main() {}

//! Variants are matched by name, so a variant index is not a slot to retire.

#[derive(serde::Deserialize, carbonite::Schema)]
#[carbonite(removed(1))]
enum Weapon {
    Sword,
}

fn main() {}

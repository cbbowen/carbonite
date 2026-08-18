//! A retired variant name cannot come back: the tag resolves by name, and a
//! variant has no `()` form to hold the name down with.

#[derive(serde::Deserialize, carbonite::Schema)]
#[carbonite(removed("Bow"))]
enum Weapon {
    Sword,
    Bow,
}

fn main() {}

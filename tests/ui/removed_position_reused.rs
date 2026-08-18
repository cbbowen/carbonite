//! A retired position must stay empty or hold a `()` placeholder.

#[derive(serde::Deserialize, carbonite::Schema)]
#[carbonite(removed(1))]
struct Point(f32, f32);

fn main() {}

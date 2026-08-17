//! Internally tagged enums rely on self-describing input.

#[derive(serde::Deserialize, carbonite::Schema)]
#[serde(tag = "kind")]
enum Message {
    Ping { seq: u32 },
}

fn main() {}

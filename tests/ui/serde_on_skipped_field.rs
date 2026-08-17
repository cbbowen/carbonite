//! A skipped field has no columns for carbonite(serde) to route.

#[derive(serde::Serialize, serde::Deserialize, carbonite::Schema)]
struct Config {
    id: u32,
    #[serde(skip)]
    #[carbonite(serde)]
    cache: Vec<u8>,
}

fn main() {}

//! serde(from)/serde(into) may name different shapes; carbonite is told the
//! wire type once, with carbonite(as).

#[derive(Clone, serde::Serialize, serde::Deserialize, carbonite::Schema)]
#[serde(from = "u32", into = "u32")]
struct Meters(f64);

impl From<u32> for Meters {
    fn from(raw: u32) -> Self {
        Meters(f64::from(raw))
    }
}

impl From<Meters> for u32 {
    fn from(meters: Meters) -> u32 {
        meters.0 as u32
    }
}

fn main() {}

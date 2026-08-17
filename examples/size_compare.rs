//! Payload-size comparison: carbonite vs postcard, raw and deflate-compressed.
//!
//! Run with: `cargo run --release --example size_compare`

use std::io::Write as _;

use flate2::Compression;
use flate2::write::DeflateEncoder;
use serde::{Deserialize, Serialize};

use carbonite::StaticSchema;

// Same fixtures as benches/compare.rs.

#[derive(Serialize, Deserialize, carbonite::Schema, Clone)]
enum Kind {
    Photon,
    Electron { spin: i8 },
    Ion(u8),
}

#[derive(Serialize, Deserialize, carbonite::Schema, Clone)]
struct Particle {
    position: [f32; 3],
    velocity: [f32; 3],
    id: u32,
    energy: f32,
    kind: Kind,
}

#[derive(Serialize, Deserialize, carbonite::Schema, Clone)]
enum Level {
    Debug,
    Info,
    Warn,
    Error,
}

#[derive(Serialize, Deserialize, carbonite::Schema, Clone)]
struct LogRecord {
    timestamp: u64,
    level: Level,
    target: String,
    message: String,
    code: Option<u16>,
}

fn particles(n: u32) -> Vec<Particle> {
    (0..n)
        .map(|i| {
            let f = i as f32;
            Particle {
                position: [f * 0.5, f * 0.25, -f],
                velocity: [f.sin(), f.cos(), 0.1],
                id: i,
                energy: f.mul_add(1.5, 0.25),
                kind: match i % 3 {
                    0 => Kind::Photon,
                    1 => Kind::Electron {
                        spin: (i % 2) as i8,
                    },
                    _ => Kind::Ion((i % 250) as u8),
                },
            }
        })
        .collect()
}

fn logs(n: u32) -> Vec<LogRecord> {
    (0..n)
        .map(|i| LogRecord {
            timestamp: 1_700_000_000_000 + u64::from(i) * 37,
            level: match i % 4 {
                0 => Level::Debug,
                1 => Level::Info,
                2 => Level::Warn,
                _ => Level::Error,
            },
            target: format!("app::module{}", i % 12),
            message: format!(
                "request {i} handled in {}ms with status {}",
                i % 250,
                200 + (i % 3) * 100
            ),
            code: (i % 5 != 0).then_some((i % 900) as u16),
        })
        .collect()
}

fn deflate_len(bytes: &[u8]) -> usize {
    let mut encoder = DeflateEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(bytes).expect("in-memory write");
    encoder.finish().expect("in-memory finish").len()
}

fn report<T>(name: &str, rows: usize, data: &Vec<T>)
where
    T: Serialize + StaticSchema,
{
    let schema = <Vec<T>>::schema();
    let carb = carbonite::Serializer::new(&schema)
        .to_vec(data)
        .expect("serialize");
    let post = postcard::to_allocvec(data).expect("serialize");

    println!(
        "{name:<14} {rows:>7}  {:>12} {:>12}  {:>9.3}  {:>14} {:>14}  {:>8.3}   {:>6}",
        carb.len(),
        post.len(),
        carb.len() as f64 / post.len() as f64,
        deflate_len(&carb),
        deflate_len(&post),
        deflate_len(&carb) as f64 / deflate_len(&post) as f64,
        schema.to_bytes().len(),
    );
}

fn main() {
    println!(
        "{:<14} {:>7}  {:>12} {:>12}  {:>9}  {:>14} {:>14}  {:>8}   {:>6}",
        "dataset",
        "rows",
        "carbonite",
        "postcard",
        "ratio",
        "carbonite+defl",
        "postcard+defl",
        "ratio",
        "schema",
    );
    report("particles", 10_000, &particles(10_000));
    report("logs", 2_000, &logs(2_000));
    println!();
    println!("sizes in bytes; ratio = carbonite / postcard (lower is better for carbonite)");
    println!("`schema` is carbonite's one-time schema cost, excluded from the data sizes");
}

//! carbonite vs postcard: serialize/deserialize speed on two workloads, plus
//! the cost of building a schema by tracing vs by derive.
//!
//! Sizes (raw and compressed) are reported by `cargo run --example size_compare`.

use std::hint::black_box;
use std::time::Duration;

use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use serde::{Deserialize, Serialize};

use carbonite::StaticSchema;

// ---------------------------------------------------------------------------
// Fixtures.
// ---------------------------------------------------------------------------

#[derive(Serialize, Deserialize, carbonite::Schema, PartialEq, Debug, Clone)]
pub enum Kind {
    Photon,
    Electron { spin: i8 },
    Ion(u8),
}

/// Numeric-heavy rows: where columnar layout should shine.
#[derive(Serialize, Deserialize, carbonite::Schema, PartialEq, Debug, Clone)]
pub struct Particle {
    pub position: [f32; 3],
    pub velocity: [f32; 3],
    pub id: u32,
    pub energy: f32,
    pub kind: Kind,
}

#[derive(Serialize, Deserialize, carbonite::Schema, PartialEq, Debug, Clone)]
pub enum Level {
    Debug,
    Info,
    Warn,
    Error,
}

/// String-heavy rows.
#[derive(Serialize, Deserialize, carbonite::Schema, PartialEq, Debug, Clone)]
pub struct LogRecord {
    pub timestamp: u64,
    pub level: Level,
    pub target: String,
    pub message: String,
    pub code: Option<u16>,
}

pub fn particles(n: u32) -> Vec<Particle> {
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

pub fn logs(n: u32) -> Vec<LogRecord> {
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

// ---------------------------------------------------------------------------
// Benchmarks.
// ---------------------------------------------------------------------------

fn bench_dataset<T>(c: &mut Criterion, name: &str, data: &Vec<T>)
where
    T: Serialize
        + serde::de::DeserializeOwned
        + StaticSchema
        + carbonite::SerializeColumns
        + for<'de> carbonite::DeserializeColumns<'de>
        + PartialEq
        + std::fmt::Debug,
{
    let schema = <Vec<T>>::schema();
    let ser = carbonite::Serializer::new(&schema);
    let de = carbonite::Deserializer::new_static(schema.clone());

    let carb_blob = ser.to_vec(data).unwrap();
    let post_blob = postcard::to_allocvec(data).unwrap();
    // Sanity: the two carbonite writers agree, and everything round-trips.
    assert_eq!(carb_blob, ser.to_vec_columns(data).unwrap());
    assert_eq!(&de.from_slice(&carb_blob).unwrap(), data);
    assert_eq!(&de.from_slice_columns(&carb_blob).unwrap(), data);
    assert_eq!(&postcard::from_bytes::<Vec<T>>(&post_blob).unwrap(), data);

    let rows = data.len() as u64;

    let mut group = c.benchmark_group(format!("serialize/{name}"));
    group.throughput(Throughput::Elements(rows));
    group.bench_function("carbonite-serde", |b| {
        b.iter(|| ser.to_vec(black_box(data)).unwrap())
    });
    group.bench_function("carbonite-columnar", |b| {
        b.iter(|| ser.to_vec_columns(black_box(data)).unwrap())
    });
    group.bench_function("postcard", |b| {
        b.iter(|| postcard::to_allocvec(black_box(data)).unwrap())
    });
    group.finish();

    let mut group = c.benchmark_group(format!("deserialize/{name}"));
    group.throughput(Throughput::Elements(rows));
    group.bench_function("carbonite-serde", |b| {
        b.iter(|| de.from_slice(black_box(&carb_blob)).unwrap())
    });
    group.bench_function("carbonite-columnar", |b| {
        b.iter(|| de.from_slice_columns(black_box(&carb_blob)).unwrap())
    });
    group.bench_function("postcard", |b| {
        b.iter(|| postcard::from_bytes::<Vec<T>>(black_box(&post_blob)).unwrap())
    });
    group.finish();
}

fn bench_datasets(c: &mut Criterion) {
    bench_dataset(c, "particles-10k", &particles(10_000));
    bench_dataset(c, "logs-2k", &logs(2_000));
}

fn bench_schema_construction(c: &mut Criterion) {
    let mut group = c.benchmark_group("schema");
    group.bench_function("traced", |b| {
        b.iter(|| carbonite::Schema::<Vec<Particle>>::new().unwrap())
    });
    group.bench_function("derived", |b| b.iter(<Vec<Particle>>::schema));
    group.finish();
}

criterion_group! {
    name = benches;
    config = Criterion::default()
        .sample_size(60)
        .warm_up_time(Duration::from_millis(500))
        .measurement_time(Duration::from_secs(2));
    targets = bench_datasets, bench_schema_construction
}
criterion_main!(benches);

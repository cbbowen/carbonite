//! Shared fixtures for the fuzz targets.
//!
//! Run with `cargo +nightly fuzz run <target>` from the repository root.
//! Every target asserts the same property the hardening tests do: hostile
//! bytes produce a clean `Err`, never a panic, an abort, or work out of
//! proportion to the input.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// One of everything: every schema node kind, so a fuzzed blob can reach
/// every decoding path, including the shared-value dictionary protocol.
#[derive(Serialize, Deserialize, carbonite::Schema, PartialEq, Debug)]
pub struct Kitchen {
    pub id: u64,
    pub name: String,
    pub blob: Vec<u8>,
    pub scale: f32,
    pub letter: char,
    pub flag: bool,
    pub maybe: Option<Box<Kitchen2>>,
    pub pairs: Vec<(u32, String)>,
    pub lookup: BTreeMap<String, i16>,
    pub state: State,
    pub mesh: carbonite::Shared<String>,
    pub fixed: [i32; 3],
    pub unit: (),
}

/// A second layer so options and boxes nest without recursion.
#[derive(Serialize, Deserialize, carbonite::Schema, PartialEq, Debug)]
pub struct Kitchen2 {
    pub tag: u8,
    pub tail: Vec<State>,
}

#[derive(Serialize, Deserialize, carbonite::Schema, PartialEq, Debug)]
pub enum State {
    Idle,
    Running(u32),
    Halted { code: i64, message: String },
}

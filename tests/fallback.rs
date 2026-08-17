//! `#[carbonite(serde)]`: fields whose types only ship serde impls.
//!
//! The point of the attribute is that nothing about the encoding changes — the
//! field keeps the columns and bytes it would have had if the whole containing
//! type had been traced — so every test here is some form of "the fallback
//! field is indistinguishable from a traced one".
#![cfg(feature = "derive")]

#[cfg(feature = "shared")]
use std::rc::Rc;

#[cfg(feature = "shared")]
use carbonite::Shared;
use carbonite::{Batch, Deserializer, Schema, Serializer, StaticSchema};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

/// Stands in for a crate that ships serde impls and knows nothing about
/// carbonite: nothing in here derives `Schema`, so none of these types
/// implement `StaticSchema` and none can be used in a derived struct without
/// the attribute (the orphan rule blocks a downstream impl).
mod foreign {
    use std::collections::BTreeMap;

    use serde::{Deserialize, Serialize};

    #[derive(Serialize, Deserialize, PartialEq, Debug, Clone)]
    pub struct EndpointAddr {
        pub id: [u8; 4],
        pub relay: Option<String>,
        pub paths: Vec<Path>,
        pub meta: BTreeMap<String, u32>,
    }

    #[derive(Serialize, Deserialize, PartialEq, Debug, Clone)]
    pub enum Path {
        Direct(String),
        Relayed { via: String, hops: u8 },
        Unknown,
    }

    #[derive(Serialize, Deserialize, PartialEq, Debug, Clone)]
    pub struct Ticket(pub u64, pub String);

    pub fn addr(relay: Option<&str>) -> EndpointAddr {
        EndpointAddr {
            id: [1, 2, 3, 4],
            relay: relay.map(str::to_owned),
            paths: vec![
                Path::Direct("192.0.2.1:4433".to_owned()),
                Path::Relayed {
                    via: "eu-west".to_owned(),
                    hops: 3,
                },
                Path::Unknown,
            ],
            meta: BTreeMap::from([("rtt".to_owned(), 12), ("mtu".to_owned(), 1400)]),
        }
    }
}

#[derive(Serialize, Deserialize, carbonite::Schema, PartialEq, Debug, Clone)]
struct Peer {
    id: u64,
    #[carbonite(serde)]
    addr: foreign::EndpointAddr,
    label: String,
}

fn peer(id: u64) -> Peer {
    Peer {
        id,
        addr: foreign::addr(Some("eu-west")),
        label: format!("peer-{id}"),
    }
}

// ---------------------------------------------------------------------------
// Helpers, mirroring tests/columnar.rs.
// ---------------------------------------------------------------------------

fn assert_matches_trace<T: StaticSchema + DeserializeOwned>() {
    assert_eq!(
        T::schema(),
        Schema::<T>::new().unwrap(),
        "a fallback field must produce the same node tracing would",
    );
}

/// Both writers produce identical bytes, and both readers accept either
/// writer's output.
fn assert_paths_interchangeable<T>(value: &T)
where
    T: Serialize
        + DeserializeOwned
        + StaticSchema
        + carbonite::SerializeColumns
        + for<'de> carbonite::DeserializeColumns<'de>
        + PartialEq
        + std::fmt::Debug,
{
    let schema = T::schema();
    let ser = Serializer::new(&schema);
    let serde_bytes = ser.to_vec(value).expect("serde path serialize");
    let columnar_bytes = ser.to_vec_columns(value).expect("columnar path serialize");
    assert_eq!(
        serde_bytes, columnar_bytes,
        "writers must produce identical bytes"
    );

    let de = Deserializer::new_static(schema);
    let via_serde: T = de.from_slice(&serde_bytes).expect("serde path deserialize");
    let via_columnar: T = de
        .from_slice_columns(&serde_bytes)
        .expect("columnar path deserialize");
    assert_eq!(&via_serde, value);
    assert_eq!(&via_columnar, value);
}

// ---------------------------------------------------------------------------
// The central invariant.
// ---------------------------------------------------------------------------

#[test]
fn derived_schema_still_matches_trace() {
    assert_matches_trace::<Peer>();
    assert_matches_trace::<Vec<Peer>>();
    assert_matches_trace::<Option<Peer>>();
}

#[test]
fn the_fallback_field_is_described_in_full() {
    // Not an opaque blob: the schema spells the foreign type out, so a reader
    // holding only the schema sees the same shape a traced writer would emit.
    let rendered = format!("{:?}", Peer::schema());
    assert!(rendered.contains("EndpointAddr"), "{rendered}");
    assert!(rendered.contains("Relayed"), "{rendered}");
    assert!(rendered.contains("relay"), "{rendered}");
}

#[test]
fn column_count_matches_the_traced_layout() {
    // The derive's runtime sum must agree with what the serde path allocates
    // from the same schema — the invariant every column offset rests on.
    let schema = Peer::schema();
    let blob = Serializer::new(&schema).to_vec_columns(&peer(1)).unwrap();
    let ncols = {
        // header: row count, then column count
        let mut cursor = &blob[..];
        let rows = read_varint(&mut cursor);
        assert_eq!(rows, 1);
        read_varint(&mut cursor)
    };
    assert_eq!(ncols as usize, Peer::columns());

    // And the count is the traced one: the foreign field spreads over its own
    // columns rather than collapsing into a length+data pair.
    let plain = Schema::<PeerReader>::new().unwrap();
    let plain_blob = Serializer::new(&plain)
        .to_vec(&PeerReader {
            id: 1,
            addr: foreign::addr(None),
            label: String::new(),
        })
        .unwrap();
    let mut cursor = &plain_blob[..];
    read_varint(&mut cursor);
    assert_eq!(read_varint(&mut cursor) as usize, Peer::columns());
}

fn read_varint(input: &mut &[u8]) -> u64 {
    let mut value = 0u64;
    let mut shift = 0;
    loop {
        let byte = input[0];
        *input = &input[1..];
        value |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return value;
        }
        shift += 7;
    }
}

#[test]
fn both_paths_agree_on_a_fallback_field() {
    assert_paths_interchangeable(&peer(7));
    assert_paths_interchangeable(&vec![peer(1), peer(2), peer(3)]);
    assert_paths_interchangeable(&Some(peer(9)));
    assert_paths_interchangeable(&Peer {
        id: 0,
        addr: foreign::addr(None),
        label: String::new(),
    });
}

#[test]
fn batches_stay_row_aligned() {
    // Every row must consume the fallback field's columns exactly; `Rows`
    // reports TrailingBytes otherwise.
    let schema = Peer::schema();
    let ser = Serializer::new(&schema);
    let mut batch: Batch<'_, Peer> = ser.batch();
    let rows: Vec<Peer> = (0..4).map(peer).collect();
    for (index, row) in rows.iter().enumerate() {
        // Alternate the writers: their rows must interleave byte-compatibly.
        if index % 2 == 0 {
            batch.push_columns(row).unwrap();
        } else {
            batch.push(row).unwrap();
        }
    }
    let blob = batch.finish();

    let de = Deserializer::new_static(schema);
    let via_serde: Vec<Peer> = de.rows(&blob).unwrap().collect::<Result<_, _>>().unwrap();
    let via_columnar: Vec<Peer> = de
        .rows_columns(&blob)
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(via_serde, rows);
    assert_eq!(via_columnar, rows);
}

// ---------------------------------------------------------------------------
// Every shape the attribute can appear in.
// ---------------------------------------------------------------------------

#[derive(Serialize, Deserialize, carbonite::Schema, PartialEq, Debug, Clone)]
struct Wrapped(#[carbonite(serde)] foreign::Ticket);

#[derive(Serialize, Deserialize, carbonite::Schema, PartialEq, Debug, Clone)]
struct Positional(u8, #[carbonite(serde)] foreign::Ticket, String);

#[derive(Serialize, Deserialize, carbonite::Schema, PartialEq, Debug, Clone)]
#[serde(transparent)]
struct Transparent {
    #[carbonite(serde)]
    inner: foreign::Ticket,
}

#[derive(Serialize, Deserialize, carbonite::Schema, PartialEq, Debug, Clone)]
struct Nested {
    // A std container of a foreign type traces fine, so the attribute covers
    // it too.
    #[carbonite(serde)]
    peers: Vec<foreign::EndpointAddr>,
    #[carbonite(serde)]
    maybe: Option<foreign::Ticket>,
    plain: Wrapped,
}

#[derive(Serialize, Deserialize, carbonite::Schema, PartialEq, Debug, Clone)]
enum Event {
    Ping,
    Connected(#[carbonite(serde)] foreign::EndpointAddr),
    Ticketed {
        #[carbonite(serde)]
        ticket: foreign::Ticket,
        at: u64,
    },
    Plain(u32),
}

#[derive(Serialize, Deserialize, carbonite::Schema, PartialEq, Debug, Clone)]
struct Skipping {
    id: u8,
    #[serde(skip)]
    cache: Option<String>,
    #[carbonite(serde)]
    ticket: foreign::Ticket,
}

fn ticket() -> foreign::Ticket {
    foreign::Ticket(42, "hunter2".to_owned())
}

#[test]
fn newtype_tuple_and_transparent_shapes() {
    assert_matches_trace::<Wrapped>();
    assert_matches_trace::<Positional>();
    assert_matches_trace::<Transparent>();

    assert_paths_interchangeable(&Wrapped(ticket()));
    assert_paths_interchangeable(&Positional(3, ticket(), "tail".to_owned()));
    assert_paths_interchangeable(&Transparent { inner: ticket() });
}

#[test]
fn nested_and_repeated_fallback_fields() {
    assert_matches_trace::<Nested>();
    assert_paths_interchangeable(&Nested {
        peers: vec![foreign::addr(None), foreign::addr(Some("us-east"))],
        maybe: Some(ticket()),
        plain: Wrapped(ticket()),
    });
    assert_paths_interchangeable(&Nested {
        peers: Vec::new(),
        maybe: None,
        plain: Wrapped(ticket()),
    });
}

#[test]
fn enum_variants_carry_fallback_fields() {
    assert_matches_trace::<Event>();
    for event in [
        Event::Ping,
        Event::Connected(foreign::addr(Some("eu-west"))),
        Event::Ticketed {
            ticket: ticket(),
            at: 99,
        },
        Event::Plain(7),
    ] {
        assert_paths_interchangeable(&event);
    }
    assert_paths_interchangeable(&vec![
        Event::Ping,
        Event::Connected(foreign::addr(None)),
        Event::Plain(1),
        Event::Ticketed {
            ticket: ticket(),
            at: 0,
        },
    ]);
}

#[test]
fn skipped_fields_coexist() {
    assert_matches_trace::<Skipping>();
    assert_paths_interchangeable(&Skipping {
        id: 1,
        cache: None,
        ticket: ticket(),
    });
}

// ---------------------------------------------------------------------------
// Generic containers.
// ---------------------------------------------------------------------------

/// `T` is reached only through a fallback field, so it must *not* pick up a
/// `StaticSchema` bound — that is the whole point for a foreign payload.
#[derive(Serialize, Deserialize, carbonite::Schema, PartialEq, Debug, Clone)]
struct Envelope<T> {
    seq: u32,
    #[carbonite(serde)]
    payload: T,
}

/// `A` still needs a schema of its own; only `B` goes through serde.
#[derive(Serialize, Deserialize, carbonite::Schema, PartialEq, Debug, Clone)]
struct Mixed<A, B> {
    plain: A,
    #[carbonite(serde)]
    foreign: B,
}

#[test]
fn generic_payloads_need_no_static_schema() {
    assert_matches_trace::<Envelope<foreign::Ticket>>();
    assert_paths_interchangeable(&Envelope {
        seq: 5,
        payload: ticket(),
    });
    assert_paths_interchangeable(&Envelope {
        seq: 6,
        payload: foreign::addr(None),
    });

    assert_matches_trace::<Mixed<Vec<u16>, foreign::Ticket>>();
    assert_paths_interchangeable(&Mixed {
        plain: vec![1u16, 2, 3],
        foreign: ticket(),
    });
}

/// The fallback field itself must own its data, but the rest of the type can
/// still borrow from the input — one of the derive's reasons to exist.
#[derive(Serialize, Deserialize, carbonite::Schema, PartialEq, Debug)]
struct Borrowing<'a, T> {
    borrowed: &'a str,
    #[carbonite(serde)]
    payload: T,
}

#[test]
fn the_rest_of_the_type_can_still_borrow() {
    let value = Borrowing {
        borrowed: "zero-copy",
        payload: ticket(),
    };
    let schema = <Borrowing<'_, foreign::Ticket>>::schema();
    let ser = Serializer::new(&schema);
    let bytes = ser.to_vec_columns(&value).unwrap();
    assert_eq!(bytes, ser.to_vec(&value).unwrap());

    let de = Deserializer::new_static(schema);
    let back: Borrowing<'_, foreign::Ticket> = de.from_slice_columns(&bytes).unwrap();
    assert_eq!(back, value);
}

// ---------------------------------------------------------------------------
// Readers that know nothing about the attribute.
// ---------------------------------------------------------------------------

#[derive(Serialize, Deserialize, PartialEq, Debug)]
struct PeerReader {
    id: u64,
    addr: foreign::EndpointAddr,
    label: String,
}

#[derive(Serialize, Deserialize, PartialEq, Debug)]
struct PeerGrown {
    id: u64,
    addr: foreign::EndpointAddr,
    label: String,
    #[serde(default)]
    note: Option<String>,
}

#[derive(Serialize, Deserialize, PartialEq, Debug)]
struct PeerShrunk {
    id: u64,
    label: String,
}

#[test]
fn a_plain_type_reads_a_fallback_written_blob() {
    // No derive, no attribute, no knowledge that the writer treated `addr`
    // specially: the bytes are ordinary carbonite bytes.
    let bytes = carbonite::to_vec_static(&peer(3)).unwrap();
    let read: PeerReader = carbonite::from_slice(&bytes).unwrap();
    assert_eq!(read.id, 3);
    assert_eq!(read.addr, foreign::addr(Some("eu-west")));
    assert_eq!(read.label, "peer-3");
}

#[test]
fn evolution_still_works_across_a_fallback_field() {
    let bytes = carbonite::to_vec_static(&peer(4)).unwrap();

    // Added field: filled from Default via the name-matched path.
    let grown: PeerGrown = carbonite::from_slice(&bytes).unwrap();
    assert_eq!(grown.addr, foreign::addr(Some("eu-west")));
    assert_eq!(grown.note, None);

    // Removed field: the reader skips the fallback field's whole subtree.
    let shrunk: PeerShrunk = carbonite::from_slice(&bytes).unwrap();
    assert_eq!(shrunk.id, 4);
    assert_eq!(shrunk.label, "peer-4");
}

#[test]
fn a_foreign_format_sees_nothing_unusual() {
    // The attribute is a carbonite codegen instruction, not a serde one.
    let json = serde_json::to_string(&peer(2)).unwrap();
    assert!(json.contains("\"relay\":\"eu-west\""), "{json}");
    let back: Peer = serde_json::from_str(&json).unwrap();
    assert_eq!(back, peer(2));
}

// ---------------------------------------------------------------------------
// Untraceable field types.
// ---------------------------------------------------------------------------

#[derive(Serialize, Deserialize, PartialEq, Debug, Clone)]
#[serde(untagged)]
enum Untagged {
    Number(u32),
    Text(String),
}

#[derive(Serialize, Deserialize, carbonite::Schema, PartialEq, Debug, Clone)]
struct HasUntraceable {
    id: u8,
    #[carbonite(serde)]
    payload: Untagged,
}

#[test]
fn tracing_the_container_reports_the_untraceable_field() {
    // The traced path has always failed here, and still does, cleanly.
    assert!(matches!(
        Schema::<HasUntraceable>::new(),
        Err(carbonite::Error::Untraceable { .. })
    ));
}

#[test]
#[should_panic(expected = "cannot be traced")]
fn a_derived_schema_panics_on_an_untraceable_fallback_field() {
    // Traceability is a property of the type, not of a value, so the derive
    // cannot report it as an error: `schema_node` is infallible.
    let _ = HasUntraceable::schema();
}

// ---------------------------------------------------------------------------
// Shared values inside a fallback field.
// ---------------------------------------------------------------------------

/// Holds carbonite's `Shared` but has no derive of its own, so it can only
/// reach a derived struct through the attribute.
#[cfg(feature = "shared")]
#[derive(Serialize, Deserialize, PartialEq, Debug, Clone)]
struct Assets {
    meshes: Vec<Shared<String>>,
    count: u32,
}

#[cfg(feature = "shared")]
#[derive(Serialize, Deserialize, carbonite::Schema, PartialEq, Debug, Clone)]
struct Scene {
    id: u32,
    #[carbonite(serde)]
    assets: Assets,
}

#[cfg(feature = "shared")]
#[test]
fn shared_dedup_survives_the_boundary() {
    let mesh = Shared::from_ptr(Rc::new("teapot.obj".to_owned()));
    let scene = Scene {
        id: 1,
        assets: Assets {
            meshes: vec![mesh.clone(), mesh.clone(), mesh],
            count: 3,
        },
    };

    assert_matches_trace::<Scene>();

    // Both writers must run the same dictionary protocol at the same schema
    // position, so the payload is written once either way.
    let schema = Scene::schema();
    let ser = Serializer::new(&schema);
    let serde_bytes = ser.to_vec(&scene).unwrap();
    let columnar_bytes = ser.to_vec_columns(&scene).unwrap();
    assert_eq!(serde_bytes, columnar_bytes);

    let de = Deserializer::new_static(schema.clone());
    for bytes in [&serde_bytes, &columnar_bytes] {
        let back: Scene = de.from_slice(bytes).unwrap();
        assert_eq!(back, scene);
        assert!(Shared::ptr_eq(
            &back.assets.meshes[0],
            &back.assets.meshes[1]
        ));
        assert!(Shared::ptr_eq(
            &back.assets.meshes[1],
            &back.assets.meshes[2]
        ));

        let columnar: Scene = de.from_slice_columns(bytes).unwrap();
        assert!(Shared::ptr_eq(
            &columnar.assets.meshes[0],
            &columnar.assets.meshes[2]
        ));
    }

    // One payload, three keys: the dictionary really did dedupe.
    let one_mesh = Scene {
        id: 1,
        assets: Assets {
            meshes: vec![Shared::new("teapot.obj".to_owned())],
            count: 3,
        },
    };
    let single = ser.to_vec_columns(&one_mesh).unwrap();
    assert_eq!(
        columnar_bytes.len(),
        single.len() + 2,
        "three occurrences should add only two more key bytes"
    );
}

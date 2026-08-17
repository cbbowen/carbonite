//! Fast-path coverage for std types beyond the primitive/collection core:
//! paths, network addresses, ranges, bounds, numeric wrappers, and boxed
//! slices/strings.
//!
//! Every hand-written impl must mirror std's serde impls exactly, so each
//! type is pinned three ways: its static schema equals the traced one, the
//! columnar bytes equal the serde-path bytes, and both readers round-trip.
#![cfg(feature = "derive")]

use std::fmt::Debug;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4, SocketAddrV6};
use std::num::Wrapping;
use std::ops::{Bound, Range, RangeInclusive};
use std::path::PathBuf;

use carbonite::{DeserializeColumns, Schema, SerializeColumns, StaticSchema};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

fn assert_parity<T>(value: T)
where
    T: Serialize
        + DeserializeOwned
        + SerializeColumns
        + for<'de> DeserializeColumns<'de>
        + PartialEq
        + Debug,
{
    assert_eq!(
        T::schema(),
        Schema::<T>::new().unwrap(),
        "static schema must equal the traced one for {}",
        std::any::type_name::<T>(),
    );

    let traced = carbonite::to_vec(&value).unwrap();
    let fast = carbonite::to_vec_static(&value).unwrap();
    assert_eq!(
        traced,
        fast,
        "columnar bytes must equal serde-path bytes for {}",
        std::any::type_name::<T>(),
    );

    assert_eq!(carbonite::from_slice::<T>(&traced).unwrap(), value);
    assert_eq!(carbonite::from_slice_static::<T>(&fast).unwrap(), value);
}

#[test]
fn paths() {
    assert_parity(PathBuf::from("saves/slot1.sav"));
    assert_parity(PathBuf::new());
    // Inside containers, where the derive would place them.
    assert_parity(vec![PathBuf::from("a"), PathBuf::from("b/c")]);
}

#[test]
fn network_addresses() {
    assert_parity(Ipv4Addr::new(192, 0, 2, 1));
    assert_parity(Ipv6Addr::LOCALHOST);
    assert_parity(IpAddr::V4(Ipv4Addr::new(198, 51, 100, 7)));
    assert_parity(IpAddr::V6(Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 1)));
    assert_parity(SocketAddrV4::new(Ipv4Addr::new(203, 0, 113, 9), 8080));
    // serde drops flowinfo and scope_id, so parity holds for the zero forms.
    assert_parity(SocketAddrV6::new(Ipv6Addr::LOCALHOST, 443, 0, 0));
    assert_parity(SocketAddr::from((Ipv4Addr::new(10, 0, 0, 1), 9000)));
    assert_parity(SocketAddr::from((Ipv6Addr::LOCALHOST, 9001)));
    // A column of addresses, the shape a columnar format is for.
    assert_parity(vec![
        SocketAddr::from((Ipv4Addr::new(10, 0, 0, 1), 1)),
        SocketAddr::from((Ipv6Addr::LOCALHOST, 2)),
    ]);
}

/// std's serde impl writes only the address and port of a `SocketAddrV6`;
/// `flowinfo` and `scope_id` come back as zero. The fast path must be exactly
/// as lossy, not less.
#[test]
fn socket_addr_v6_extras_are_dropped_like_serde() {
    let full = SocketAddrV6::new(Ipv6Addr::LOCALHOST, 443, 5, 6);
    let traced = carbonite::to_vec(&full).unwrap();
    let fast = carbonite::to_vec_static(&full).unwrap();
    assert_eq!(traced, fast);

    let expected = SocketAddrV6::new(Ipv6Addr::LOCALHOST, 443, 0, 0);
    assert_eq!(
        carbonite::from_slice::<SocketAddrV6>(&traced).unwrap(),
        expected
    );
    assert_eq!(
        carbonite::from_slice_static::<SocketAddrV6>(&fast).unwrap(),
        expected
    );
}

#[test]
fn ranges_and_bounds() {
    assert_parity(3u32..9);
    assert_parity(1u8..=5);
    assert_parity(Bound::<u16>::Unbounded);
    assert_parity(Bound::Included(7u16));
    assert_parity(Bound::Excluded(8u16));
    assert_parity(vec![0u64..10, 10..20]);
}

#[test]
fn numeric_wrappers_and_boxes() {
    assert_parity(Wrapping(250u8));
    assert_parity(Wrapping(-3i64));
    assert_parity(Box::<str>::from("boxed"));
    assert_parity(vec![1u32, 2, 3].into_boxed_slice());
    assert_parity(Vec::<Box<[u8]>>::from([
        Box::from([1u8, 2].as_slice()),
        Box::from([].as_slice()),
    ]));
}

/// The point of the coverage: these types as *fields* of a derived struct,
/// with no `#[carbonite(serde)]` needed.
#[test]
fn derived_structs_hold_the_new_types_directly() {
    #[derive(Serialize, Deserialize, carbonite::Schema, PartialEq, Debug)]
    struct Manifest {
        save: PathBuf,
        peer: SocketAddr,
        ports: Range<u16>,
        window: RangeInclusive<u32>,
        cutoff: Bound<u64>,
        counter: Wrapping<u32>,
        label: Box<str>,
        samples: Box<[f32]>,
    }

    let manifest = Manifest {
        save: PathBuf::from("saves/slot1.sav"),
        peer: SocketAddr::from((Ipv4Addr::new(192, 0, 2, 1), 7777)),
        ports: 1024..2048,
        window: 4..=44,
        cutoff: Bound::Excluded(99),
        counter: Wrapping(u32::MAX),
        label: Box::from("autosave"),
        samples: vec![0.5, -0.5].into_boxed_slice(),
    };

    assert_eq!(Manifest::schema(), Schema::<Manifest>::new().unwrap());
    let traced = carbonite::to_vec(&manifest).unwrap();
    let fast = carbonite::to_vec_static(&manifest).unwrap();
    assert_eq!(traced, fast);
    assert_eq!(carbonite::from_slice::<Manifest>(&fast).unwrap(), manifest);
}

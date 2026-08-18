//! `carbonite::compat`: verifying that today's type still reads the schemas
//! you shipped, the way a CI job would.

// These tests exercise `#[derive(Schema)]` types end to end.
#![cfg(feature = "derive")]
// The candidate types here exist to be deserialized into, not read from; what
// is under test is whether they decode at all.
#![allow(dead_code)]

use serde::{Deserialize, Serialize};

use carbonite::compat::{self, Incompatible};
use carbonite::{Schema, StaticSchema};

/// The released type, whose schema a project would have snapshotted.
#[derive(Serialize, Deserialize)]
struct SaveV1 {
    id: u32,
    name: String,
    tags: Vec<String>,
    weapon: WeaponV1,
}

#[derive(Serialize, Deserialize)]
enum WeaponV1 {
    Sword,
    Bow { range: u32 },
    Wand(u8),
}

/// The bytes a project would keep in its repository, reinterpreted for
/// whichever candidate type is under test.
fn released<T>() -> Schema<T> {
    Schema::<SaveV1>::new()
        .expect("the released type traces")
        .cast()
}

fn check<T: serde::de::DeserializeOwned>() -> Result<(), Incompatible> {
    compat::check(&released::<T>())
}

// ---------------------------------------------------------------------------
// The cases a schema-to-schema comparison could not decide, because the answer
// lives in the reader's attributes rather than in either schema.
// ---------------------------------------------------------------------------

#[test]
fn an_unchanged_type_is_readable() {
    check::<SaveV1>().unwrap();
}

#[test]
fn an_added_field_depends_on_its_default() {
    #[derive(Deserialize)]
    struct WithDefault {
        id: u32,
        name: String,
        tags: Vec<String>,
        weapon: WeaponV1,
        #[serde(default)]
        stamina: u32,
    }
    #[derive(Deserialize)]
    struct WithoutDefault {
        id: u32,
        name: String,
        tags: Vec<String>,
        weapon: WeaponV1,
        stamina: u32,
    }

    check::<WithDefault>().unwrap();
    let err = check::<WithoutDefault>().unwrap_err();
    assert!(matches!(err, Incompatible::Unreadable(_)), "{err}");
    assert!(err.to_string().contains("stamina"), "{err}");
}

#[test]
fn a_renamed_field_depends_on_its_alias() {
    #[derive(Deserialize)]
    struct WithAlias {
        id: u32,
        #[serde(alias = "name")]
        title: String,
        tags: Vec<String>,
        weapon: WeaponV1,
    }
    #[derive(Deserialize)]
    struct WithoutAlias {
        id: u32,
        title: String,
        tags: Vec<String>,
        weapon: WeaponV1,
    }

    check::<WithAlias>().unwrap();
    assert!(check::<WithoutAlias>().is_err());
}

/// The shape rule: a positional group becoming named is readable only if the
/// reader declared the positions it replaced.
#[test]
fn a_shape_change_depends_on_its_position_tags() {
    #[derive(Serialize, Deserialize)]
    struct PointV1(f32, f32);

    #[derive(Deserialize)]
    struct Tagged {
        #[serde(alias = "0")]
        x: f32,
        #[serde(alias = "1")]
        y: f32,
    }
    #[derive(Deserialize)]
    struct Untagged {
        x: f32,
        y: f32,
    }

    let point = Schema::<PointV1>::new().unwrap();
    compat::check::<Tagged>(&point.clone().cast()).unwrap();

    let err = compat::check::<Untagged>(&point.cast()).unwrap_err();
    assert!(err.to_string().contains("alias = \"0\""), "{err}");
}

// ---------------------------------------------------------------------------
// Changes that are decidable either way, checked so the tool agrees with the
// reader.
// ---------------------------------------------------------------------------

#[test]
fn removing_a_field_stays_readable() {
    #[derive(Deserialize)]
    struct Fewer {
        id: u32,
        weapon: WeaponV1,
    }

    check::<Fewer>().unwrap();
}

#[test]
fn reordering_fields_stays_readable() {
    #[derive(Deserialize)]
    struct Reordered {
        weapon: WeaponV1,
        tags: Vec<String>,
        name: String,
        id: u32,
    }

    check::<Reordered>().unwrap();
}

#[test]
fn changing_a_fields_type_is_a_break() {
    #[derive(Deserialize)]
    struct Retyped {
        id: String,
        name: String,
        tags: Vec<String>,
        weapon: WeaponV1,
    }

    assert!(matches!(
        check::<Retyped>().unwrap_err(),
        Incompatible::Unreadable(_)
    ));
}

/// Every variant is written across the probe's rows, so dropping one is caught
/// even though it is only reachable from a single row.
#[test]
fn dropping_an_enum_variant_is_caught() {
    #[derive(Deserialize)]
    enum Fewer {
        Sword,
        Bow { range: u32 },
    }
    #[derive(Deserialize)]
    struct Save {
        id: u32,
        name: String,
        tags: Vec<String>,
        weapon: Fewer,
    }

    let err = check::<Save>().unwrap_err();
    assert!(err.to_string().contains("Wand"), "{err}");
}

/// And the tombstone that fixes it: keep the name, drop the payload.
#[test]
fn a_tombstoned_variant_is_readable_again() {
    #[derive(Deserialize)]
    enum Tombstoned {
        Sword,
        Bow {
            range: u32,
        },
        #[serde(alias = "Wand")]
        Retired,
    }
    #[derive(Deserialize)]
    struct Save {
        id: u32,
        name: String,
        tags: Vec<String>,
        weapon: Tombstoned,
    }

    check::<Save>().unwrap();
}

/// Adding a variant is readable: old data never names it.
#[test]
fn adding_a_variant_stays_readable() {
    #[derive(Deserialize)]
    enum More {
        Wand(u8),
        Sword,
        Bow { range: u32 },
        Hammer,
    }
    #[derive(Deserialize)]
    struct Save {
        id: u32,
        name: String,
        tags: Vec<String>,
        weapon: More,
    }

    check::<Save>().unwrap();
}

// ---------------------------------------------------------------------------
// Telling "your type refuses synthetic values" apart from a real break.
// ---------------------------------------------------------------------------

#[derive(Serialize, Deserialize, carbonite::Schema, Clone, Copy)]
#[serde(try_from = "u32", into = "u32")]
#[carbonite(as = "u32")]
struct Even(u32);

impl TryFrom<u32> for Even {
    type Error = &'static str;
    fn try_from(value: u32) -> Result<Self, Self::Error> {
        if value % 2 == 0 {
            Ok(Even(value))
        } else {
            Err("odd")
        }
    }
}

impl From<Even> for u32 {
    fn from(value: Even) -> u32 {
        value.0
    }
}

/// The probe writes 1, which `Even` rejects. That is the type refusing the
/// values, not the schema, and it must not be reported as a break — including
/// when the schema handed in is the type's own.
#[test]
fn a_validating_type_is_reported_as_inconclusive() {
    #[derive(Serialize, Deserialize, carbonite::Schema)]
    struct Holder {
        v: Even,
    }

    // A validating type cannot be traced either, so it is derive-only.
    assert!(Schema::<Holder>::new().is_err());

    let err = compat::check_static::<Holder>(&Holder::schema()).unwrap_err();
    assert!(
        matches!(err, Incompatible::Inconclusive(_)),
        "expected Inconclusive, got {err}"
    );
    // And `check`, which has no reference schema to work from here, reaches
    // the same conclusion rather than crying break.
    let err = compat::check::<Holder>(&Holder::schema()).unwrap_err();
    assert!(
        matches!(err, Incompatible::Inconclusive(_)),
        "expected Inconclusive, got {err}"
    );
}

/// The counterpart: a type that cannot be traced is never reported as broken
/// by `check`, because there is nothing to establish the difference. That is
/// what `check_static` is for, and it is worth seeing the two disagree.
#[test]
fn an_untraceable_type_is_inconclusive_under_check_but_decided_by_check_static() {
    #[derive(Serialize, Deserialize)]
    struct Original {
        name: String,
    }
    #[derive(Serialize, Deserialize, carbonite::Schema)]
    struct Broken {
        // No alias, so this genuinely cannot read `Original`...
        title: String,
        // ...and this makes the type untraceable, hiding that from `check`.
        #[serde(alias = "other")]
        extra: Option<u8>,
    }

    let original = Schema::<Original>::new().unwrap();
    assert!(matches!(
        compat::check::<Broken>(&original.clone().cast()).unwrap_err(),
        Incompatible::Inconclusive(_)
    ));
    assert!(matches!(
        compat::check_static::<Broken>(&original.cast()).unwrap_err(),
        Incompatible::Unreadable(_)
    ));
}

/// `check` classifies by tracing the current type, which `#[serde(alias)]`
/// defeats — and a migrated type always carries aliases. `check_static` reads
/// the reference schema from the derive instead, so the distinction survives.
#[test]
fn check_static_classifies_an_aliased_type() {
    #[derive(Serialize, Deserialize, carbonite::Schema)]
    struct Aliased {
        #[serde(alias = "name")]
        title: String,
    }
    #[derive(Serialize, Deserialize)]
    struct Original {
        name: String,
    }

    // The type is untraceable, so its own schema is unavailable to `check`...
    assert!(Schema::<Aliased>::new().is_err());
    // ...but the derive still has it, and the migration checks out.
    let original = Schema::<Original>::new().unwrap();
    compat::check_static::<Aliased>(&original.cast()).unwrap();
    // The derived schema is of course readable by its own type.
    compat::check_static::<Aliased>(&Aliased::schema()).unwrap();
}

// ---------------------------------------------------------------------------
// What the check deliberately does not cover, recorded so the limit is a
// decision rather than a surprise.
// ---------------------------------------------------------------------------

/// Narrowing an integer is value-dependent: it reads every number below the
/// new ceiling and rejects the rest, so no single probe value can show it.
/// The structural pass compares the leaves instead.
#[test]
fn narrowing_an_integer_is_caught_structurally() {
    #[derive(Serialize, Deserialize)]
    struct Wide {
        v: u64,
        untouched: String,
    }
    #[derive(Deserialize)]
    struct Narrow {
        v: u8,
        untouched: String,
    }

    let wide = Schema::<Wide>::new().unwrap();
    let err = compat::check::<Narrow>(&wide.cast()).unwrap_err();
    let Incompatible::ValueDependent(findings) = &err else {
        panic!("expected ValueDependent, got {err}");
    };
    assert_eq!(findings.len(), 1, "{findings:?}");
    assert!(
        findings[0].contains('v') && findings[0].contains("u64"),
        "{err}"
    );
}

#[test]
fn widening_an_integer_is_not_a_finding() {
    #[derive(Serialize, Deserialize)]
    struct Small {
        a: u8,
        b: u32,
        c: i8,
    }
    #[derive(Deserialize)]
    struct Wider {
        a: u64,
        b: i64, // unsigned into a strictly wider signed type
        c: i32,
    }

    let small = Schema::<Small>::new().unwrap();
    compat::check::<Wider>(&small.cast()).unwrap();
}

/// Signedness is part of the range: an unsigned field needs one more bit to
/// stay positive, and a signed one's negatives never survive.
#[test]
fn a_signedness_change_is_value_dependent() {
    #[derive(Serialize, Deserialize)]
    struct Signed {
        v: i32,
    }
    #[derive(Deserialize)]
    struct Unsigned {
        v: u32,
    }
    #[derive(Serialize, Deserialize)]
    struct SameWidthUnsigned {
        v: u32,
    }
    #[derive(Deserialize)]
    struct SameWidthSigned {
        v: i32,
    }

    let signed = Schema::<Signed>::new().unwrap();
    assert!(matches!(
        compat::check::<Unsigned>(&signed.cast()).unwrap_err(),
        Incompatible::ValueDependent(_)
    ));

    let unsigned = Schema::<SameWidthUnsigned>::new().unwrap();
    assert!(matches!(
        compat::check::<SameWidthSigned>(&unsigned.cast()).unwrap_err(),
        Incompatible::ValueDependent(_)
    ));
}

/// The structural pass needs no values, so it still returns a verdict for a
/// type the probe cannot touch.
#[test]
fn a_validating_type_still_gets_a_structural_verdict() {
    #[derive(Serialize, Deserialize, carbonite::Schema)]
    struct WideHolder {
        v: Even,
        range: u64,
    }
    #[derive(Serialize, Deserialize, carbonite::Schema)]
    struct NarrowHolder {
        v: Even,
        range: u16,
    }

    // The probe alone can say nothing about this type...
    assert!(matches!(
        compat::check_static::<NarrowHolder>(&NarrowHolder::schema()).unwrap_err(),
        Incompatible::Inconclusive(_)
    ));
    // ...but comparing the schemas still finds the narrowing.
    let err = compat::check_static::<NarrowHolder>(&WideHolder::schema().cast()).unwrap_err();
    assert!(
        matches!(err, Incompatible::ValueDependent(_)),
        "expected ValueDependent, got {err}"
    );
}

/// Narrowing nested inside the shapes the walk has to see through.
#[test]
fn narrowing_is_found_through_containers_and_variants() {
    #[derive(Serialize, Deserialize)]
    struct Meters(u64);
    #[derive(Serialize, Deserialize)]
    enum Wide {
        A { depth: u64 },
        B(Vec<Option<u64>>),
        C(Meters),
    }
    #[derive(Deserialize)]
    struct Centimetres(u16);
    #[derive(Deserialize)]
    enum Narrow {
        A { depth: u16 },
        B(Vec<Option<u16>>),
        C(Centimetres),
    }

    let wide = Schema::<Wide>::new().unwrap();
    let err = compat::check::<Narrow>(&wide.cast()).unwrap_err();
    let Incompatible::ValueDependent(findings) = &err else {
        panic!("expected ValueDependent, got {err}");
    };
    assert_eq!(findings.len(), 3, "{findings:?}");
    assert!(
        findings.iter().any(|f| f.contains("A.depth")),
        "{findings:?}"
    );
}

/// A field the other side does not name could have been renamed, dropped, or
/// tagged; the walk stays quiet and leaves it to the probe.
#[test]
fn the_structural_pass_stays_quiet_where_it_cannot_be_sure() {
    #[derive(Serialize, Deserialize)]
    struct Before {
        a: u64,
    }
    #[derive(Deserialize)]
    struct Renamed {
        #[serde(alias = "a")]
        b: u16, // narrowed *and* renamed: unmatched, so not reported
    }

    let before = Schema::<Before>::new().unwrap();
    // Readable, and the narrowing goes unremarked rather than guessed at.
    compat::check::<Renamed>(&before.cast()).unwrap();
}

/// Round-tripping the snapshot through bytes is the actual CI flow.
#[test]
fn a_schema_snapshot_round_trips_through_bytes() {
    let snapshot = Schema::<SaveV1>::new().unwrap().to_bytes();
    let released = Schema::<SaveV1>::from_bytes(&snapshot).unwrap();
    compat::check(&released).unwrap();
}

// ---------------------------------------------------------------------------
// Nested enums. The sampler's pick is mixed-radix — each enum consumes the
// low digit and hands the quotient to its payload — so an enum nested inside
// a variant is exercised across all of *its* variants too. Regression: with
// one shared pick, an inner enum whose count shared a factor with the outer's
// only ever saw one residue class, and removed inner variants went unnoticed.
// ---------------------------------------------------------------------------

#[test]
fn dropping_a_nested_enum_variant_is_caught() {
    #[derive(Serialize, Deserialize)]
    enum InnerV1 {
        X,
        Y,
    }
    #[derive(Serialize, Deserialize)]
    enum OuterV1 {
        A,
        B(InnerV1),
    }

    #[derive(Deserialize)]
    enum Inner {
        Y,
    }
    #[derive(Deserialize)]
    enum Outer {
        A,
        B(Inner),
    }

    let snapshot = Schema::<OuterV1>::new().unwrap().cast::<Outer>();

    // Real old data using the removed inner variant no longer reads...
    let old = carbonite::to_vec(&OuterV1::B(InnerV1::X)).unwrap();
    assert!(carbonite::from_slice::<Outer>(&old).is_err());

    // ...so the check must say so rather than passing.
    let err = compat::check(&snapshot).unwrap_err();
    assert!(matches!(err, Incompatible::Unreadable(_)), "{err}");
}

#[test]
fn a_variant_three_levels_deep_is_still_exercised() {
    // Distinct leaf types per position, so dropping a variant from the
    // *deepest, least-sampled* one (U → N → P) is only caught if the sampler
    // really reaches every leaf variant on every path.
    #[derive(Serialize, Deserialize)]
    enum Leaf1 {
        P,
        Q,
    }
    #[derive(Serialize, Deserialize)]
    enum Leaf2 {
        P,
        Q,
    }
    #[derive(Serialize, Deserialize)]
    enum MidV1 {
        M(Leaf1),
        N(Leaf2),
    }
    #[derive(Serialize, Deserialize)]
    enum TopV1 {
        T,
        U(MidV1),
    }

    #[derive(Deserialize)]
    enum Leaf2Small {
        Q,
    }
    #[derive(Deserialize)]
    enum MidSmall {
        M(Leaf1),
        N(Leaf2Small),
    }
    #[derive(Deserialize)]
    enum TopSmall {
        T,
        U(MidSmall),
    }

    let err = compat::check(&Schema::<TopV1>::new().unwrap().cast::<TopSmall>()).unwrap_err();
    assert!(matches!(err, Incompatible::Unreadable(_)), "{err}");

    // The unchanged type still passes with the multiplied row count.
    compat::check(&Schema::<TopV1>::new().unwrap()).unwrap();
}

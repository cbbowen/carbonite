//! `glam` support: the impls behind the `glam` feature describe glam's own
//! hand-written serde impls, so for every type the derived schema must equal
//! the traced one and the columnar path must produce the serde path's bytes.
#![cfg(feature = "glam")]

use std::any::type_name;
use std::fmt::Debug;

use glam::{
    Affine2, Affine3, Affine3A, BVec2, BVec3, BVec3A, BVec4, BVec4A, DAffine2, DAffine3, DMat2,
    DMat3, DMat4, DQuat, DVec2, DVec3, DVec4, EulerRot, I8Vec2, I8Vec3, I8Vec4, I16Vec2, I16Vec3,
    I16Vec4, I64Vec2, I64Vec3, I64Vec4, ISizeVec2, ISizeVec3, ISizeVec4, IVec2, IVec3, IVec4, Mat2,
    Mat3, Mat3A, Mat4, Quat, U8Vec2, U8Vec3, U8Vec4, U16Vec2, U16Vec3, U16Vec4, U64Vec2, U64Vec3,
    U64Vec4, USizeVec2, USizeVec3, USizeVec4, UVec2, UVec3, UVec4, Vec2, Vec3, Vec3A, Vec4,
};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use carbonite::{
    DeserializeColumns, Deserializer, Schema, SerializeColumns, Serializer, StaticSchema,
};

/// The full contract for one glam type: the compile-time schema matches a
/// trace of glam's `Deserialize`, both writers emit identical bytes, and both
/// readers recover the value.
fn assert_supported<T>(value: T)
where
    T: Serialize
        + DeserializeOwned
        + StaticSchema
        + SerializeColumns
        + for<'de> DeserializeColumns<'de>
        + PartialEq
        + Debug,
{
    let name = type_name::<T>();
    let schema = T::schema();
    assert_eq!(
        schema,
        Schema::<T>::new().unwrap(),
        "derived schema must match traced schema for {name}"
    );

    let ser = Serializer::new(&schema);
    let serde_bytes = ser.to_vec(&value).expect("serde path serialize");
    let columnar_bytes = ser.to_vec_columns(&value).expect("columnar path serialize");
    assert_eq!(
        serde_bytes, columnar_bytes,
        "writers must produce identical bytes for {name}"
    );

    let de = Deserializer::new_static(schema);
    assert!(de.uses_fast_path(), "schema should be exact for {name}");
    let via_serde: T = de.from_slice(&serde_bytes).expect("serde path deserialize");
    let via_columns: T = de
        .from_slice_columns(&serde_bytes)
        .expect("columnar path deserialize");
    assert_eq!(via_serde, value, "serde path round trip for {name}");
    assert_eq!(via_columns, value, "columnar path round trip for {name}");
}

#[test]
fn float_vectors_are_supported() {
    assert_supported(Vec2::new(1.5, -2.25));
    assert_supported(Vec3::new(1.5, -2.25, f32::MAX));
    assert_supported(Vec3A::new(0.0, -0.5, 7.75));
    assert_supported(Vec4::new(1.5, -2.25, 3.0, f32::MIN_POSITIVE));
    assert_supported(DVec2::new(1.5, -2.25));
    assert_supported(DVec3::new(1.5, -2.25, f64::MAX));
    assert_supported(DVec4::new(1.5, -2.25, 3.0, -0.0));
}

#[test]
fn signed_integer_vectors_are_supported() {
    assert_supported(I8Vec2::new(i8::MIN, 7));
    assert_supported(I8Vec3::new(-1, 0, i8::MAX));
    assert_supported(I8Vec4::new(-1, 0, 1, 2));
    assert_supported(I16Vec2::new(i16::MIN, 7));
    assert_supported(I16Vec3::new(-1, 0, i16::MAX));
    assert_supported(I16Vec4::new(-1, 0, 1, 2));
    assert_supported(IVec2::new(i32::MIN, 7));
    assert_supported(IVec3::new(-1, 0, i32::MAX));
    assert_supported(IVec4::new(-1, 0, 1, 2));
    assert_supported(I64Vec2::new(i64::MIN, 7));
    assert_supported(I64Vec3::new(-1, 0, i64::MAX));
    assert_supported(I64Vec4::new(-1, 0, 1, 2));
    assert_supported(ISizeVec2::new(isize::MIN, 7));
    assert_supported(ISizeVec3::new(-1, 0, isize::MAX));
    assert_supported(ISizeVec4::new(-1, 0, 1, 2));
}

#[test]
fn unsigned_integer_vectors_are_supported() {
    assert_supported(U8Vec2::new(0, u8::MAX));
    assert_supported(U8Vec3::new(1, 2, 3));
    assert_supported(U8Vec4::new(1, 2, 3, 4));
    assert_supported(U16Vec2::new(0, u16::MAX));
    assert_supported(U16Vec3::new(1, 2, 3));
    assert_supported(U16Vec4::new(1, 2, 3, 4));
    assert_supported(UVec2::new(0, u32::MAX));
    assert_supported(UVec3::new(1, 2, 3));
    assert_supported(UVec4::new(1, 2, 3, 4));
    assert_supported(U64Vec2::new(0, u64::MAX));
    assert_supported(U64Vec3::new(1, 2, 3));
    assert_supported(U64Vec4::new(1, 2, 3, 4));
    assert_supported(USizeVec2::new(0, usize::MAX));
    assert_supported(USizeVec3::new(1, 2, 3));
    assert_supported(USizeVec4::new(1, 2, 3, 4));
}

#[test]
fn bool_vectors_are_supported() {
    assert_supported(BVec2::new(true, false));
    assert_supported(BVec3::new(false, true, true));
    assert_supported(BVec4::new(true, false, false, true));
}

#[test]
fn quaternions_are_supported() {
    assert_supported(Quat::from_xyzw(0.5, -0.5, 0.5, 0.5));
    assert_supported(DQuat::from_xyzw(0.5, -0.5, 0.5, 0.5));
}

#[test]
fn matrices_are_supported() {
    assert_supported(Mat2::from_cols_array(&[1.0, 2.0, 3.0, 4.0]));
    assert_supported(Mat3::from_cols_array(&[
        1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0, 9.0,
    ]));
    assert_supported(Mat3A::from_cols_array(&[
        1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0, 9.0,
    ]));
    assert_supported(Mat4::from_cols_array(&[
        1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0, 9.0, 10.0, 11.0, 12.0, 13.0, 14.0, 15.0, 16.0,
    ]));
    assert_supported(DMat2::from_cols_array(&[1.0, 2.0, 3.0, 4.0]));
    assert_supported(DMat3::from_cols_array(&[
        1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0, 9.0,
    ]));
    assert_supported(DMat4::from_cols_array(&[
        1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0, 9.0, 10.0, 11.0, 12.0, 13.0, 14.0, 15.0, 16.0,
    ]));
}

#[test]
fn affine_transforms_are_supported() {
    assert_supported(Affine2::from_cols_array(&[1.0, 0.5, -0.5, 1.0, 3.0, 4.0]));
    assert_supported(Affine3::from_cols_array(&[
        1.0, 0.0, 0.0, 0.0, 2.0, 0.0, 0.0, 0.0, 3.0, 4.0, 5.0, 6.0,
    ]));
    assert_supported(Affine3A::from_cols_array(&[
        1.0, 0.0, 0.0, 0.0, 2.0, 0.0, 0.0, 0.0, 3.0, 4.0, 5.0, 6.0,
    ]));
    assert_supported(DAffine2::from_cols_array(&[1.0, 0.5, -0.5, 1.0, 3.0, 4.0]));
    assert_supported(DAffine3::from_cols_array(&[
        1.0, 0.0, 0.0, 0.0, 2.0, 0.0, 0.0, 0.0, 3.0, 4.0, 5.0, 6.0,
    ]));
}

#[test]
fn euler_rot_is_supported() {
    // Every variant, so the tag mapping is checked against the traced schema's
    // variant order end to end.
    for rot in [
        EulerRot::ZYX,
        EulerRot::ZXY,
        EulerRot::YXZ,
        EulerRot::YZX,
        EulerRot::XYZ,
        EulerRot::XZY,
        EulerRot::ZYZ,
        EulerRot::ZXZ,
        EulerRot::YXY,
        EulerRot::YZY,
        EulerRot::XYX,
        EulerRot::XZX,
        EulerRot::ZYXEx,
        EulerRot::ZXYEx,
        EulerRot::YXZEx,
        EulerRot::YZXEx,
        EulerRot::XYZEx,
        EulerRot::XZYEx,
        EulerRot::ZYZEx,
        EulerRot::ZXZEx,
        EulerRot::YXYEx,
        EulerRot::YZYEx,
        EulerRot::XYXEx,
        EulerRot::XZXEx,
    ] {
        assert_supported(rot);
    }
}

#[test]
fn glam_types_nest_in_containers() {
    assert_supported(vec![Vec3::new(1.0, 2.0, 3.0), Vec3::new(-1.0, -2.0, -3.0)]);
    assert_supported(Some(Quat::IDENTITY));
    assert_supported((Vec2::ONE, Mat2::IDENTITY, EulerRot::XYZ));
    assert_supported(vec![Option::<IVec2>::None, Some(IVec2::new(3, 4))]);
}

/// The point of the feature: a derived type holds glam fields directly, with
/// no `#[carbonite(serde)]` and so no serde dispatch for them.
#[test]
fn derived_types_hold_glam_fields() {
    #[derive(Serialize, Deserialize, carbonite::Schema, PartialEq, Debug, Clone)]
    struct Transform {
        translation: Vec3,
        rotation: Quat,
        scale: Vec3,
    }

    #[derive(Serialize, Deserialize, carbonite::Schema, PartialEq, Debug, Clone)]
    struct Entity {
        id: u32,
        transform: Transform,
        tint: U8Vec4,
        visible: BVec3,
        matrix: Affine3A,
        order: EulerRot,
    }

    let entity = Entity {
        id: 7,
        transform: Transform {
            translation: Vec3::new(1.0, 2.0, 3.0),
            rotation: Quat::from_xyzw(0.0, 0.0, 0.0, 1.0),
            scale: Vec3::splat(2.0),
        },
        tint: U8Vec4::new(255, 128, 64, 32),
        visible: BVec3::new(true, false, true),
        matrix: Affine3A::from_scale(Vec3::splat(0.5)),
        order: EulerRot::YXZ,
    };

    assert_supported(entity.clone());
    assert_supported(vec![entity.clone(), entity]);
}

/// A `Vec<Vec3>` is what the fast path is for: three contiguous `f32` runs,
/// one column each, and no schema interpretation per element.
#[test]
fn vectors_of_vectors_are_column_oriented() {
    let points: Vec<Vec3> = (0..64)
        .map(|i| Vec3::new(i as f32, i as f32 * 2.0, i as f32 * 3.0))
        .collect();

    let schema = <Vec<Vec3>>::schema();
    // Length column plus one column per component.
    assert_eq!(<Vec<Vec3>>::columns(), 4);

    let blob = Serializer::new(&schema).to_vec_columns(&points).unwrap();
    let back: Vec<Vec3> = Deserializer::new_static(schema)
        .from_slice_columns(&blob)
        .unwrap();
    assert_eq!(back, points);
}

/// `BVec3A` and `BVec4A` are the two types carbonite leaves alone, because
/// glam's hand-written impls for them are inconsistent. This pins the reason:
/// when an assertion here fails, glam has fixed the bug and the impls can be
/// added.
#[test]
fn the_bvec_a_masks_are_left_to_glam() {
    // BVec3A's Deserialize names the tuple struct `$vec3` — a `stringify!`
    // that escaped its macro — while its Serialize says `BVec3A`. The two
    // directions describe different types, so carbonite's serde path rejects
    // the pair and no schema can serve both.
    let schema = Schema::<BVec3A>::new().unwrap();
    assert_eq!(
        schema.to_string(),
        "tuple struct `$vec3`",
        "glam has fixed BVec3A's Deserialize; carbonite can implement it now"
    );
    assert!(
        Serializer::new(&schema)
            .to_vec(&BVec3A::new(true, false, true))
            .is_err(),
        "glam has fixed BVec3A; carbonite can implement it now"
    );

    // BVec4A's Serialize writes `z` twice and never `w`, so an implementation
    // that carries all four components cannot match its bytes.
    let value = BVec4A::new(true, false, false, true);
    let json = serde_json::to_string(&value).unwrap();
    assert_eq!(
        serde_json::from_str::<BVec4A>(&json).unwrap(),
        BVec4A::new(true, false, false, false),
        "glam has fixed BVec4A's Serialize; carbonite can implement it now"
    );
}

/// Nothing glam-specific reaches the data layer: a `Vec3` occupies the same
/// columns and bytes as any other three-`f32` tuple struct, so a peer that has
/// never heard of glam reads the blob with its own type.
#[test]
fn blobs_carry_nothing_glam_specific() {
    #[derive(Serialize, Deserialize, carbonite::Schema, PartialEq, Debug)]
    struct Point(f32, f32, f32);

    let from_glam = Serializer::new(Vec3::schema())
        .to_vec_columns(&Vec3::new(1.5, -2.5, 3.5))
        .unwrap();
    let from_plain = Serializer::new(Point::schema())
        .to_vec_columns(&Point(1.5, -2.5, 3.5))
        .unwrap();
    assert_eq!(from_glam, from_plain);

    let point: Point = Deserializer::new_static(Point::schema())
        .from_slice_columns(&from_glam)
        .unwrap();
    assert_eq!(point, Point(1.5, -2.5, 3.5));
}

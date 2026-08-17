//! Compile-time schemas and columnar fast paths for `glam`, behind the `glam`
//! feature.
//!
//! glam writes its `Serialize`/`Deserialize` impls by hand instead of deriving
//! them, so the [tracing workaround](crate#fields-whose-types-come-from-another-crate)
//! for foreign types is all its users can reach — and math types are exactly
//! the ones a columnar format most wants on the fast path, since a
//! `Vec<Vec3>` becomes three contiguous `f32` runs. The orphan rule leaves
//! these impls to carbonite, so they live here.
//!
//! Every glam type reaches the wire as a tuple struct named after the Rust
//! type, holding one field per component in glam's own order: `x, y, z, w` for
//! vectors and quaternions, column-major for matrices and affine transforms.
//! `EulerRot` is a unit-only enum. That is the shape glam's own serde impls
//! describe, so the schemas here are identical to what
//! [`Schema::new`](crate::Schema::new) traces, and the columnar bytes are
//! identical to what the serde path writes.
//!
//! # Coverage
//!
//! Every glam type with serde impls, except two: vectors (`Vec2`, `Vec3`,
//! `Vec3A`, `Vec4` and their `D`/`I`/`U`/`B`-prefixed siblings), quaternions,
//! matrices, affine transforms, and `EulerRot`.
//!
//! `BVec3A` and `BVec4A` are left out, because carbonite cannot agree with
//! glam's impls for them:
//!
//! - `BVec3A`'s `Deserialize` names the tuple struct `$vec3` — a `stringify!`
//!   that escaped its macro — so the name a trace records is not the one its
//!   `Serialize` reports, and the two directions do not describe the same
//!   type.
//! - `BVec4A`'s `Serialize` writes the `z` component twice and never `w`, so
//!   an implementation that carries all four components cannot be
//!   byte-compatible with it.
//!
//! Both are glam bugs rather than encoding limits; a mask vector can travel as
//! `BVec3`/`BVec4` in the meantime.

use glam::{
    Affine2, Affine3, Affine3A, BVec2, BVec3, BVec4, DAffine2, DAffine3, DMat2, DMat3, DMat4,
    DQuat, DVec2, DVec3, DVec4, EulerRot, I8Vec2, I8Vec3, I8Vec4, I16Vec2, I16Vec3, I16Vec4,
    I64Vec2, I64Vec3, I64Vec4, ISizeVec2, ISizeVec3, ISizeVec4, IVec2, IVec3, IVec4, Mat2, Mat3,
    Mat3A, Mat4, Quat, U8Vec2, U8Vec3, U8Vec4, U16Vec2, U16Vec3, U16Vec4, U64Vec2, U64Vec3,
    U64Vec4, USizeVec2, USizeVec3, USizeVec4, UVec2, UVec3, UVec4, Vec2, Vec3, Vec3A, Vec4,
};

use crate::columnar::{
    __invalid_variant, __split, ColumnCursor, DeserializeColumns, SerializeColumns, write_varint,
};
use crate::error::Result;
use crate::schema::{SchemaNode, VariantNode};
use crate::static_schema::StaticSchema;

/// Writes one component per column, in wire order.
fn write_components<T: SerializeColumns, const N: usize>(
    components: &[T; N],
    columns: &mut [Vec<u8>],
) -> Result<()> {
    let mut rest = columns;
    for component in components {
        component.serialize_columns(__split(&mut rest, T::columns()))?;
    }
    Ok(())
}

/// Reads one component per column, in wire order.
fn read_components<'de, T, const N: usize>(cursors: &mut [ColumnCursor<'de>]) -> Result<[T; N]>
where
    T: DeserializeColumns<'de> + Copy + Default,
{
    let mut rest = &mut *cursors;
    let mut components = [T::default(); N];
    for component in &mut components {
        *component = T::deserialize_columns(__split(&mut rest, T::columns()))?;
    }
    Ok(components)
}

/// The schema half: a tuple struct named after the type, with `$n` `$comp`
/// fields. `stringify!` on the type name is how glam names them too.
macro_rules! tuple_struct_schema {
    ($ty:ident, $comp:ty, $n:literal) => {
        impl StaticSchema for $ty {
            fn schema_node() -> SchemaNode {
                SchemaNode::TupleStruct {
                    name: stringify!($ty).to_owned(),
                    fields: vec![<$comp as StaticSchema>::schema_node(); $n],
                }
            }

            #[inline]
            fn columns() -> usize {
                $n * <$comp as StaticSchema>::columns()
            }
        }
    };
}

/// Vectors, including the `bool` masks: every one converts to and from an
/// array of its components.
macro_rules! vector_impls {
    ($($ty:ident: $comp:ty, $n:literal;)*) => {$(
        tuple_struct_schema!($ty, $comp, $n);

        impl SerializeColumns for $ty {
            fn serialize_columns(&self, columns: &mut [Vec<u8>]) -> Result<()> {
                let components: [$comp; $n] = (*self).into();
                write_components(&components, columns)
            }
        }

        impl<'de> DeserializeColumns<'de> for $ty {
            fn deserialize_columns(cursors: &mut [ColumnCursor<'de>]) -> Result<Self> {
                Ok(<$ty>::from(read_components::<$comp, $n>(cursors)?))
            }
        }
    )*};
}

vector_impls! {
    Vec2: f32, 2;
    Vec3: f32, 3;
    Vec3A: f32, 3;
    Vec4: f32, 4;
    DVec2: f64, 2;
    DVec3: f64, 3;
    DVec4: f64, 4;
    I8Vec2: i8, 2;
    I8Vec3: i8, 3;
    I8Vec4: i8, 4;
    I16Vec2: i16, 2;
    I16Vec3: i16, 3;
    I16Vec4: i16, 4;
    IVec2: i32, 2;
    IVec3: i32, 3;
    IVec4: i32, 4;
    I64Vec2: i64, 2;
    I64Vec3: i64, 3;
    I64Vec4: i64, 4;
    ISizeVec2: isize, 2;
    ISizeVec3: isize, 3;
    ISizeVec4: isize, 4;
    U8Vec2: u8, 2;
    U8Vec3: u8, 3;
    U8Vec4: u8, 4;
    U16Vec2: u16, 2;
    U16Vec3: u16, 3;
    U16Vec4: u16, 4;
    UVec2: u32, 2;
    UVec3: u32, 3;
    UVec4: u32, 4;
    U64Vec2: u64, 2;
    U64Vec3: u64, 3;
    U64Vec4: u64, 4;
    USizeVec2: usize, 2;
    USizeVec3: usize, 3;
    USizeVec4: usize, 4;
    BVec2: bool, 2;
    BVec3: bool, 3;
    BVec4: bool, 4;
}

/// Quaternions: four components in `x, y, z, w` order.
macro_rules! quaternion_impls {
    ($($ty:ident: $comp:ty;)*) => {$(
        tuple_struct_schema!($ty, $comp, 4);

        impl SerializeColumns for $ty {
            fn serialize_columns(&self, columns: &mut [Vec<u8>]) -> Result<()> {
                write_components(&self.to_array(), columns)
            }
        }

        impl<'de> DeserializeColumns<'de> for $ty {
            fn deserialize_columns(cursors: &mut [ColumnCursor<'de>]) -> Result<Self> {
                Ok(<$ty>::from_array(read_components::<$comp, 4>(cursors)?))
            }
        }
    )*};
}

quaternion_impls! {
    Quat: f32;
    DQuat: f64;
}

/// Matrices and affine transforms: components column-major, which is the
/// order of glam's own cols-array conversions.
macro_rules! matrix_impls {
    ($($ty:ident: $comp:ty, $n:literal;)*) => {$(
        tuple_struct_schema!($ty, $comp, $n);

        impl SerializeColumns for $ty {
            fn serialize_columns(&self, columns: &mut [Vec<u8>]) -> Result<()> {
                write_components(&self.to_cols_array(), columns)
            }
        }

        impl<'de> DeserializeColumns<'de> for $ty {
            fn deserialize_columns(cursors: &mut [ColumnCursor<'de>]) -> Result<Self> {
                Ok(<$ty>::from_cols_array(&read_components::<$comp, $n>(cursors)?))
            }
        }
    )*};
}

matrix_impls! {
    Mat2: f32, 4;
    Mat3: f32, 9;
    Mat3A: f32, 9;
    Mat4: f32, 16;
    Affine2: f32, 6;
    Affine3: f32, 12;
    Affine3A: f32, 12;
    DMat2: f64, 4;
    DMat3: f64, 9;
    DMat4: f64, 16;
    DAffine2: f64, 6;
    DAffine3: f64, 12;
}

/// `EulerRot`: a unit-only enum, so one varint tag column and no payload.
///
/// A variant's tag is its position in the list below, which has to stay in
/// glam's declaration order — that is the order its `Deserialize` reports, and
/// so the order a trace records.
macro_rules! euler_rot_impls {
    ($($variant:ident),* $(,)?) => {
        /// Variants in wire order; the tag is an index into this.
        const EULER_ROT_ORDER: [EulerRot; 24] = [$(EulerRot::$variant),*];

        /// Proof that the list is complete: a variant added to glam makes this
        /// match non-exhaustive, rather than leaving a schema that silently
        /// disagrees with a trace. Never called — it exists to be checked.
        #[allow(dead_code)]
        fn assert_every_variant_listed(rot: EulerRot) {
            match rot {
                $(EulerRot::$variant => {})*
            }
        }

        impl StaticSchema for EulerRot {
            fn schema_node() -> SchemaNode {
                SchemaNode::Enum {
                    name: "EulerRot".to_owned(),
                    variants: vec![
                        $((stringify!($variant).to_owned(), VariantNode::Unit)),*
                    ],
                }
            }

            #[inline]
            fn columns() -> usize {
                1
            }
        }
    };
}

euler_rot_impls!(
    ZYX, ZXY, YXZ, YZX, XYZ, XZY, ZYZ, ZXZ, YXY, YZY, XYX, XZX, ZYXEx, ZXYEx, YXZEx, YZXEx, XYZEx,
    XZYEx, ZYZEx, ZXZEx, YXYEx, YZYEx, XYXEx, XZXEx,
);

impl SerializeColumns for EulerRot {
    fn serialize_columns(&self, columns: &mut [Vec<u8>]) -> Result<()> {
        let tag = EULER_ROT_ORDER
            .iter()
            .position(|rot| rot == self)
            .expect("every EulerRot variant is listed");
        write_varint(&mut columns[0], tag as u64);
        Ok(())
    }
}

impl<'de> DeserializeColumns<'de> for EulerRot {
    fn deserialize_columns(cursors: &mut [ColumnCursor<'de>]) -> Result<Self> {
        let tag = cursors[0].varint()?;
        usize::try_from(tag)
            .ok()
            .and_then(|index| EULER_ROT_ORDER.get(index).copied())
            .ok_or_else(|| __invalid_variant(tag))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::Layout;
    use crate::schema::Primitive;

    /// `columns()` must equal what the layout builder derives from the same
    /// type's schema tree — the invariant the columnar paths index off.
    fn assert_columns_agree<T: StaticSchema>() {
        assert_eq!(
            T::columns(),
            Layout::new(&T::schema_node()).columns,
            "columns() disagrees with the layout for {}",
            std::any::type_name::<T>(),
        );
    }

    #[test]
    fn column_counts_match_the_layout() {
        assert_columns_agree::<Vec2>();
        assert_columns_agree::<Vec3A>();
        assert_columns_agree::<DVec4>();
        assert_columns_agree::<BVec3>();
        assert_columns_agree::<USizeVec2>();
        assert_columns_agree::<Quat>();
        assert_columns_agree::<Mat4>();
        assert_columns_agree::<Affine3A>();
        assert_columns_agree::<EulerRot>();
    }

    #[test]
    fn vectors_are_tuple_structs_of_components() {
        assert_eq!(
            Vec3::schema_node(),
            SchemaNode::TupleStruct {
                name: "Vec3".to_owned(),
                fields: vec![SchemaNode::Primitive(Primitive::F32); 3],
            }
        );
        assert_eq!(Vec3::columns(), 3);

        // serde puts usize on the wire as u64, so the mirror does too.
        assert_eq!(
            USizeVec2::schema_node(),
            SchemaNode::TupleStruct {
                name: "USizeVec2".to_owned(),
                fields: vec![SchemaNode::Primitive(Primitive::U64); 2],
            }
        );
    }

    #[test]
    fn euler_rot_is_a_unit_only_enum() {
        let SchemaNode::Enum { name, variants } = EulerRot::schema_node() else {
            panic!("expected an enum schema");
        };
        assert_eq!(name, "EulerRot");
        assert_eq!(variants.len(), 24);
        assert_eq!(variants[0].0, "ZYX");
        assert_eq!(variants[23].0, "XZXEx");
        assert!(
            variants
                .iter()
                .all(|(_, shape)| *shape == VariantNode::Unit)
        );
    }

    /// Tags are positions in `EULER_ROT_ORDER`, so the two directions have to
    /// agree on every one of them.
    #[test]
    fn euler_rot_tags_round_trip() {
        for (index, rot) in EULER_ROT_ORDER.iter().enumerate() {
            let mut columns = vec![Vec::new()];
            rot.serialize_columns(&mut columns).unwrap();
            assert_eq!(columns[0], [index as u8]);

            let mut cursors = [ColumnCursor::new(&columns[0])];
            assert_eq!(EulerRot::deserialize_columns(&mut cursors).unwrap(), *rot);
        }
    }
}

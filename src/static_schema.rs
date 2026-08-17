//! Compile-time schemas: the [`StaticSchema`] trait and its std impls.

use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet, BinaryHeap, HashMap, HashSet, LinkedList, VecDeque};
use std::marker::PhantomData;

use crate::schema::{Primitive, Schema, SchemaNode, VariantNode};

/// Types whose carbonite schema is known at compile time.
///
/// Implemented here for primitives and common std containers; implement it
/// for your own types with `#[derive(Schema)]` (the `derive` feature, on by
/// default). A derived schema is **identical** to what runtime tracing
/// ([`Schema::new`]) would discover, so the two are interchangeable — the
/// derive just skips the runtime cost and, unlike tracing, also works for
/// types that borrow from their input.
///
/// # Examples
///
/// ```
/// use serde::{Serialize, Deserialize};
///
/// #[derive(Serialize, Deserialize, carbonite::Schema)]
/// struct Pixel {
///     x: u16,
///     y: u16,
///     luma: f32,
/// }
///
/// use carbonite::StaticSchema;
/// let schema = Pixel::schema();
/// assert_eq!(schema, carbonite::Schema::<Pixel>::new()?);
/// # Ok::<(), carbonite::Error>(())
/// ```
pub trait StaticSchema {
    /// The untyped schema tree for this type.
    fn schema_node() -> SchemaNode;

    /// The typed schema for this type.
    #[must_use]
    fn schema() -> Schema<Self> {
        Schema::from_node(Self::schema_node())
    }
}

macro_rules! primitive_impls {
    ($($ty:ty => $prim:ident,)*) => {$(
        impl StaticSchema for $ty {
            fn schema_node() -> SchemaNode {
                SchemaNode::Primitive(Primitive::$prim)
            }
        }
    )*};
}

primitive_impls! {
    bool => Bool,
    i8 => I8,
    i16 => I16,
    i32 => I32,
    i64 => I64,
    i128 => I128,
    // serde puts usize/isize on the wire as u64/i64.
    isize => I64,
    u8 => U8,
    u16 => U16,
    u32 => U32,
    u64 => U64,
    u128 => U128,
    usize => U64,
    f32 => F32,
    f64 => F64,
    char => Char,
    std::num::NonZeroI8 => I8,
    std::num::NonZeroI16 => I16,
    std::num::NonZeroI32 => I32,
    std::num::NonZeroI64 => I64,
    std::num::NonZeroI128 => I128,
    std::num::NonZeroIsize => I64,
    std::num::NonZeroU8 => U8,
    std::num::NonZeroU16 => U16,
    std::num::NonZeroU32 => U32,
    std::num::NonZeroU64 => U64,
    std::num::NonZeroU128 => U128,
    std::num::NonZeroUsize => U64,
}

impl StaticSchema for () {
    fn schema_node() -> SchemaNode {
        SchemaNode::Unit
    }
}

impl StaticSchema for String {
    fn schema_node() -> SchemaNode {
        SchemaNode::String
    }
}

impl StaticSchema for str {
    fn schema_node() -> SchemaNode {
        SchemaNode::String
    }
}

impl<T: StaticSchema> StaticSchema for Option<T> {
    fn schema_node() -> SchemaNode {
        SchemaNode::Option(Box::new(T::schema_node()))
    }
}

macro_rules! seq_impls {
    ($($ty:ident $(: $extra:path)?,)*) => {$(
        impl<T: StaticSchema $(+ $extra)?> StaticSchema for $ty<T> {
            fn schema_node() -> SchemaNode {
                SchemaNode::Seq(Box::new(T::schema_node()))
            }
        }
    )*};
}

seq_impls! {
    Vec,
    VecDeque,
    LinkedList,
    BinaryHeap: Ord,
    BTreeSet: Ord,
}

impl<T: StaticSchema, S> StaticSchema for HashSet<T, S> {
    fn schema_node() -> SchemaNode {
        SchemaNode::Seq(Box::new(T::schema_node()))
    }
}

impl<T: StaticSchema, const N: usize> StaticSchema for [T; N] {
    fn schema_node() -> SchemaNode {
        // serde treats arrays as N-tuples.
        SchemaNode::Tuple(vec![T::schema_node(); N])
    }
}

impl<K: StaticSchema, V: StaticSchema, S> StaticSchema for HashMap<K, V, S> {
    fn schema_node() -> SchemaNode {
        SchemaNode::Map {
            key: Box::new(K::schema_node()),
            value: Box::new(V::schema_node()),
        }
    }
}

impl<K: StaticSchema, V: StaticSchema> StaticSchema for BTreeMap<K, V> {
    fn schema_node() -> SchemaNode {
        SchemaNode::Map {
            key: Box::new(K::schema_node()),
            value: Box::new(V::schema_node()),
        }
    }
}

macro_rules! tuple_impls {
    ($($($name:ident)+,)*) => {$(
        impl<$($name: StaticSchema),+> StaticSchema for ($($name,)+) {
            fn schema_node() -> SchemaNode {
                SchemaNode::Tuple(vec![$(<$name>::schema_node()),+])
            }
        }
    )*};
}

tuple_impls! {
    T0,
    T0 T1,
    T0 T1 T2,
    T0 T1 T2 T3,
    T0 T1 T2 T3 T4,
    T0 T1 T2 T3 T4 T5,
    T0 T1 T2 T3 T4 T5 T6,
    T0 T1 T2 T3 T4 T5 T6 T7,
    T0 T1 T2 T3 T4 T5 T6 T7 T8,
    T0 T1 T2 T3 T4 T5 T6 T7 T8 T9,
    T0 T1 T2 T3 T4 T5 T6 T7 T8 T9 T10,
    T0 T1 T2 T3 T4 T5 T6 T7 T8 T9 T10 T11,
    T0 T1 T2 T3 T4 T5 T6 T7 T8 T9 T10 T11 T12,
    T0 T1 T2 T3 T4 T5 T6 T7 T8 T9 T10 T11 T12 T13,
    T0 T1 T2 T3 T4 T5 T6 T7 T8 T9 T10 T11 T12 T13 T14,
    T0 T1 T2 T3 T4 T5 T6 T7 T8 T9 T10 T11 T12 T13 T14 T15,
}

impl<T: StaticSchema + ?Sized> StaticSchema for &T {
    fn schema_node() -> SchemaNode {
        T::schema_node()
    }
}

impl<T: StaticSchema + ?Sized> StaticSchema for &mut T {
    fn schema_node() -> SchemaNode {
        T::schema_node()
    }
}

impl<T: StaticSchema + ?Sized> StaticSchema for Box<T> {
    fn schema_node() -> SchemaNode {
        T::schema_node()
    }
}

impl<T: StaticSchema + ?Sized> StaticSchema for std::rc::Rc<T> {
    fn schema_node() -> SchemaNode {
        T::schema_node()
    }
}

impl<T: StaticSchema + ?Sized> StaticSchema for std::sync::Arc<T> {
    fn schema_node() -> SchemaNode {
        T::schema_node()
    }
}

impl<T: StaticSchema + ToOwned + ?Sized> StaticSchema for Cow<'_, T> {
    fn schema_node() -> SchemaNode {
        T::schema_node()
    }
}

impl<T: StaticSchema, E: StaticSchema> StaticSchema for Result<T, E> {
    fn schema_node() -> SchemaNode {
        SchemaNode::Enum {
            name: "Result".to_owned(),
            variants: vec![
                (
                    "Ok".to_owned(),
                    VariantNode::Newtype(Box::new(T::schema_node())),
                ),
                (
                    "Err".to_owned(),
                    VariantNode::Newtype(Box::new(E::schema_node())),
                ),
            ],
        }
    }
}

impl<T: ?Sized> StaticSchema for PhantomData<T> {
    fn schema_node() -> SchemaNode {
        // The name serde's impl passes to serialize_unit_struct.
        SchemaNode::UnitStruct {
            name: "PhantomData".to_owned(),
        }
    }
}

impl StaticSchema for std::time::Duration {
    fn schema_node() -> SchemaNode {
        SchemaNode::Struct {
            name: "Duration".to_owned(),
            fields: vec![
                ("secs".to_owned(), SchemaNode::Primitive(Primitive::U64)),
                ("nanos".to_owned(), SchemaNode::Primitive(Primitive::U32)),
            ],
        }
    }
}

impl StaticSchema for std::time::SystemTime {
    fn schema_node() -> SchemaNode {
        SchemaNode::Struct {
            name: "SystemTime".to_owned(),
            fields: vec![
                (
                    "secs_since_epoch".to_owned(),
                    SchemaNode::Primitive(Primitive::U64),
                ),
                (
                    "nanos_since_epoch".to_owned(),
                    SchemaNode::Primitive(Primitive::U32),
                ),
            ],
        }
    }
}

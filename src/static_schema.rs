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
/// [`StaticSchema::schema`] is the method you call. The rest of the trait —
/// the schema tree and the column count — is carbonite's internal surface,
/// shared by the [`SerializeColumns`](crate::SerializeColumns) and
/// [`DeserializeColumns`](crate::DeserializeColumns) fast paths so the two
/// directions can never disagree about a type's layout.
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
    /// The untyped schema tree for this type. Internal; see
    /// [`SchemaNode`].
    #[doc(hidden)]
    fn schema_node() -> SchemaNode;

    /// Number of columns this type's schema occupies. Internal.
    ///
    /// A function rather than an associated constant because a field marked
    /// `#[carbonite(serde)]` takes its schema from a runtime trace of the
    /// field type, so any type containing one has a column count that is not
    /// a constant expression. Every implementation here is a chain of
    /// `#[inline]` literals, so the common case still folds to a constant.
    #[doc(hidden)]
    fn columns() -> usize;

    /// `Some(width)` iff this type is a single fixed-width column, which lets
    /// sequences bulk-reserve. Internal.
    #[doc(hidden)]
    const FIXED_WIDTH: Option<usize> = None;

    /// The typed schema for this type.
    #[must_use]
    fn schema() -> Schema<Self> {
        Schema::from_node(Self::schema_node())
    }
}

macro_rules! primitive_impls {
    ($($ty:ty => $prim:ident, $width:expr;)*) => {$(
        impl StaticSchema for $ty {
            fn schema_node() -> SchemaNode {
                SchemaNode::Primitive(Primitive::$prim)
            }
            #[inline]
            fn columns() -> usize {
                1
            }
            const FIXED_WIDTH: Option<usize> = Some($width);
        }
    )*};
}

primitive_impls! {
    bool => Bool, 1;
    i8 => I8, 1;
    i16 => I16, 2;
    i32 => I32, 4;
    i64 => I64, 8;
    i128 => I128, 16;
    // serde puts usize/isize on the wire as u64/i64.
    isize => I64, 8;
    u8 => U8, 1;
    u16 => U16, 2;
    u32 => U32, 4;
    u64 => U64, 8;
    u128 => U128, 16;
    usize => U64, 8;
    f32 => F32, 4;
    f64 => F64, 8;
    char => Char, 4;
    std::num::NonZeroI8 => I8, 1;
    std::num::NonZeroI16 => I16, 2;
    std::num::NonZeroI32 => I32, 4;
    std::num::NonZeroI64 => I64, 8;
    std::num::NonZeroI128 => I128, 16;
    std::num::NonZeroIsize => I64, 8;
    std::num::NonZeroU8 => U8, 1;
    std::num::NonZeroU16 => U16, 2;
    std::num::NonZeroU32 => U32, 4;
    std::num::NonZeroU64 => U64, 8;
    std::num::NonZeroU128 => U128, 16;
    std::num::NonZeroUsize => U64, 8;
}

impl StaticSchema for () {
    fn schema_node() -> SchemaNode {
        SchemaNode::Unit
    }
    #[inline]
    fn columns() -> usize {
        0
    }
}

impl StaticSchema for String {
    fn schema_node() -> SchemaNode {
        SchemaNode::String
    }
    #[inline]
    fn columns() -> usize {
        2
    }
}

impl StaticSchema for str {
    fn schema_node() -> SchemaNode {
        SchemaNode::String
    }
    #[inline]
    fn columns() -> usize {
        2
    }
}

impl<T: StaticSchema> StaticSchema for Option<T> {
    fn schema_node() -> SchemaNode {
        SchemaNode::Option(Box::new(T::schema_node()))
    }
    #[inline]
    fn columns() -> usize {
        1 + T::columns()
    }
}

impl<T: StaticSchema> StaticSchema for [T] {
    fn schema_node() -> SchemaNode {
        SchemaNode::Seq(Box::new(T::schema_node()))
    }
    #[inline]
    fn columns() -> usize {
        1 + T::columns()
    }
}

macro_rules! seq_impls {
    ($($ty:ident $(: $extra:path)?,)*) => {$(
        impl<T: StaticSchema $(+ $extra)?> StaticSchema for $ty<T> {
            fn schema_node() -> SchemaNode {
                SchemaNode::Seq(Box::new(T::schema_node()))
            }
            #[inline]
            fn columns() -> usize {
                1 + T::columns()
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
    #[inline]
    fn columns() -> usize {
        1 + T::columns()
    }
}

impl<T: StaticSchema, const N: usize> StaticSchema for [T; N] {
    fn schema_node() -> SchemaNode {
        // serde treats arrays as N-tuples.
        SchemaNode::Tuple(vec![T::schema_node(); N])
    }
    #[inline]
    fn columns() -> usize {
        N * T::columns()
    }
}

impl<K: StaticSchema, V: StaticSchema, S> StaticSchema for HashMap<K, V, S> {
    fn schema_node() -> SchemaNode {
        SchemaNode::Map {
            key: Box::new(K::schema_node()),
            value: Box::new(V::schema_node()),
        }
    }
    #[inline]
    fn columns() -> usize {
        1 + K::columns() + V::columns()
    }
}

impl<K: StaticSchema, V: StaticSchema> StaticSchema for BTreeMap<K, V> {
    fn schema_node() -> SchemaNode {
        SchemaNode::Map {
            key: Box::new(K::schema_node()),
            value: Box::new(V::schema_node()),
        }
    }
    #[inline]
    fn columns() -> usize {
        1 + K::columns() + V::columns()
    }
}

macro_rules! tuple_impls {
    ($($($name:ident)+,)*) => {$(
        impl<$($name: StaticSchema),+> StaticSchema for ($($name,)+) {
            fn schema_node() -> SchemaNode {
                SchemaNode::Tuple(vec![$(<$name>::schema_node()),+])
            }
            #[inline]
            fn columns() -> usize {
                0 $(+ $name::columns())+
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

macro_rules! transparent_impls {
    ($($ty:ty),* $(,)?) => {$(
        impl<T: StaticSchema + ?Sized> StaticSchema for $ty {
            fn schema_node() -> SchemaNode {
                T::schema_node()
            }
            #[inline]
            fn columns() -> usize {
                T::columns()
            }
            const FIXED_WIDTH: Option<usize> = T::FIXED_WIDTH;
        }
    )*};
}

transparent_impls!(&T, &mut T, Box<T>, std::rc::Rc<T>, std::sync::Arc<T>);

impl<T: StaticSchema + ToOwned + ?Sized> StaticSchema for Cow<'_, T> {
    fn schema_node() -> SchemaNode {
        T::schema_node()
    }
    #[inline]
    fn columns() -> usize {
        T::columns()
    }
    const FIXED_WIDTH: Option<usize> = T::FIXED_WIDTH;
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
    #[inline]
    fn columns() -> usize {
        1 + T::columns() + E::columns()
    }
}

impl<T: ?Sized> StaticSchema for PhantomData<T> {
    fn schema_node() -> SchemaNode {
        // The name serde's impl passes to serialize_unit_struct.
        SchemaNode::UnitStruct {
            name: "PhantomData".to_owned(),
        }
    }
    #[inline]
    fn columns() -> usize {
        0
    }
}

impl StaticSchema for std::path::Path {
    fn schema_node() -> SchemaNode {
        // serde puts paths on the wire as strings (and errors on non-UTF-8).
        SchemaNode::String
    }
    #[inline]
    fn columns() -> usize {
        2
    }
}

impl StaticSchema for std::path::PathBuf {
    fn schema_node() -> SchemaNode {
        SchemaNode::String
    }
    #[inline]
    fn columns() -> usize {
        2
    }
}

impl<T: StaticSchema> StaticSchema for std::num::Wrapping<T> {
    fn schema_node() -> SchemaNode {
        // serde's impl is transparent: the wrapper never reaches the wire.
        T::schema_node()
    }
    #[inline]
    fn columns() -> usize {
        T::columns()
    }
    const FIXED_WIDTH: Option<usize> = T::FIXED_WIDTH;
}

/// serde serializes each `Range*` type as a struct of its bounds.
macro_rules! range_impls {
    ($($ty:ident { $($field:literal),+ },)*) => {$(
        impl<T: StaticSchema> StaticSchema for std::ops::$ty<T> {
            fn schema_node() -> SchemaNode {
                SchemaNode::Struct {
                    name: stringify!($ty).to_owned(),
                    fields: vec![$(($field.to_owned(), T::schema_node())),+],
                }
            }
            #[inline]
            fn columns() -> usize {
                [$($field),+].len() * T::columns()
            }
        }
    )*};
}

range_impls! {
    Range { "start", "end" },
    RangeInclusive { "start", "end" },
}

impl<T: StaticSchema> StaticSchema for std::ops::Bound<T> {
    fn schema_node() -> SchemaNode {
        SchemaNode::Enum {
            name: "Bound".to_owned(),
            variants: vec![
                ("Unbounded".to_owned(), VariantNode::Unit),
                (
                    "Included".to_owned(),
                    VariantNode::Newtype(Box::new(T::schema_node())),
                ),
                (
                    "Excluded".to_owned(),
                    VariantNode::Newtype(Box::new(T::schema_node())),
                ),
            ],
        }
    }
    #[inline]
    fn columns() -> usize {
        1 + 2 * T::columns()
    }
}

// serde's non-human-readable network forms: addresses are their octet
// arrays, socket addresses are `(ip, port)` tuples, and the version-agnostic
// types are externally tagged enums over the two.

impl StaticSchema for std::net::Ipv4Addr {
    fn schema_node() -> SchemaNode {
        <[u8; 4]>::schema_node()
    }
    #[inline]
    fn columns() -> usize {
        4
    }
}

impl StaticSchema for std::net::Ipv6Addr {
    fn schema_node() -> SchemaNode {
        <[u8; 16]>::schema_node()
    }
    #[inline]
    fn columns() -> usize {
        16
    }
}

impl StaticSchema for std::net::IpAddr {
    fn schema_node() -> SchemaNode {
        SchemaNode::Enum {
            name: "IpAddr".to_owned(),
            variants: vec![
                (
                    "V4".to_owned(),
                    VariantNode::Newtype(Box::new(std::net::Ipv4Addr::schema_node())),
                ),
                (
                    "V6".to_owned(),
                    VariantNode::Newtype(Box::new(std::net::Ipv6Addr::schema_node())),
                ),
            ],
        }
    }
    #[inline]
    fn columns() -> usize {
        1 + std::net::Ipv4Addr::columns() + std::net::Ipv6Addr::columns()
    }
}

impl StaticSchema for std::net::SocketAddrV4 {
    fn schema_node() -> SchemaNode {
        <(std::net::Ipv4Addr, u16)>::schema_node()
    }
    #[inline]
    fn columns() -> usize {
        <(std::net::Ipv4Addr, u16)>::columns()
    }
}

impl StaticSchema for std::net::SocketAddrV6 {
    fn schema_node() -> SchemaNode {
        <(std::net::Ipv6Addr, u16)>::schema_node()
    }
    #[inline]
    fn columns() -> usize {
        <(std::net::Ipv6Addr, u16)>::columns()
    }
}

impl StaticSchema for std::net::SocketAddr {
    fn schema_node() -> SchemaNode {
        SchemaNode::Enum {
            name: "SocketAddr".to_owned(),
            variants: vec![
                (
                    "V4".to_owned(),
                    VariantNode::Newtype(Box::new(std::net::SocketAddrV4::schema_node())),
                ),
                (
                    "V6".to_owned(),
                    VariantNode::Newtype(Box::new(std::net::SocketAddrV6::schema_node())),
                ),
            ],
        }
    }
    #[inline]
    fn columns() -> usize {
        1 + std::net::SocketAddrV4::columns() + std::net::SocketAddrV6::columns()
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
    #[inline]
    fn columns() -> usize {
        2
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
    #[inline]
    fn columns() -> usize {
        2
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::Layout;

    /// Every `StaticSchema::columns()` must equal what the layout builder
    /// derives from the same type's schema tree — the invariant the columnar
    /// fast paths rely on for their column offsets.
    fn assert_columns_agree<T: StaticSchema + ?Sized>() {
        assert_eq!(
            T::columns(),
            Layout::new(&T::schema_node()).columns,
            "columns() disagrees with the layout for {}",
            std::any::type_name::<T>(),
        );
    }

    #[test]
    fn column_counts_match_the_layout() {
        assert_columns_agree::<bool>();
        assert_columns_agree::<u8>();
        assert_columns_agree::<i128>();
        assert_columns_agree::<char>();
        assert_columns_agree::<usize>();
        assert_columns_agree::<std::num::NonZeroU32>();
        assert_columns_agree::<()>();
        assert_columns_agree::<String>();
        assert_columns_agree::<str>();
        assert_columns_agree::<Option<u32>>();
        assert_columns_agree::<Option<Option<String>>>();
        assert_columns_agree::<Vec<u32>>();
        assert_columns_agree::<Vec<(u32, String)>>();
        assert_columns_agree::<VecDeque<u8>>();
        assert_columns_agree::<LinkedList<u8>>();
        assert_columns_agree::<BinaryHeap<u8>>();
        assert_columns_agree::<BTreeSet<u8>>();
        assert_columns_agree::<HashSet<u8>>();
        assert_columns_agree::<[u32; 4]>();
        assert_columns_agree::<[String; 3]>();
        assert_columns_agree::<[(); 5]>();
        assert_columns_agree::<HashMap<String, u32>>();
        assert_columns_agree::<BTreeMap<u8, Vec<f32>>>();
        assert_columns_agree::<(u8,)>();
        assert_columns_agree::<(u8, String, Option<f64>)>();
        assert_columns_agree::<&u32>();
        assert_columns_agree::<&str>();
        assert_columns_agree::<Box<Vec<u8>>>();
        assert_columns_agree::<std::rc::Rc<String>>();
        assert_columns_agree::<std::sync::Arc<u64>>();
        assert_columns_agree::<Cow<'_, str>>();
        assert_columns_agree::<Result<u32, String>>();
        assert_columns_agree::<PhantomData<u8>>();
        assert_columns_agree::<std::time::Duration>();
        assert_columns_agree::<std::time::SystemTime>();
        assert_columns_agree::<[u8]>();
        assert_columns_agree::<Box<[u16]>>();
        assert_columns_agree::<Box<str>>();
        assert_columns_agree::<std::path::Path>();
        assert_columns_agree::<std::path::PathBuf>();
        assert_columns_agree::<std::num::Wrapping<u32>>();
        assert_columns_agree::<std::ops::Range<u8>>();
        assert_columns_agree::<std::ops::RangeInclusive<String>>();
        assert_columns_agree::<std::ops::Bound<u32>>();
        assert_columns_agree::<std::net::Ipv4Addr>();
        assert_columns_agree::<std::net::Ipv6Addr>();
        assert_columns_agree::<std::net::IpAddr>();
        assert_columns_agree::<std::net::SocketAddrV4>();
        assert_columns_agree::<std::net::SocketAddrV6>();
        assert_columns_agree::<std::net::SocketAddr>();
    }
}

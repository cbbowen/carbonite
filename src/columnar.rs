//! The monomorphized columnar fast path.
//!
//! [`SerializeColumns`] and [`DeserializeColumns`] read and write the *exact*
//! byte layout of the serde-driven path, but as generated straight-line code
//! with column offsets known at compile time — no schema interpretation per
//! value. Implementations come from `#[derive(Schema)]`; the impls in this
//! module cover primitives and std containers.
//!
//! The fast path only applies when the wire schema equals the type's own
//! [`StaticSchema`]; for any other schema (older files, foreign writers),
//! use the serde path, which handles evolution.
//!
//! # Contract for manual implementations
//!
//! Prefer the derive. A manual implementation must reproduce, byte for byte,
//! what carbonite's serde-driven path produces for the same `Serialize` /
//! `Deserialize` impls, over the column layout of the type's
//! [`StaticSchema::schema_node`]: columns are assigned depth-first, a node's
//! own columns (lengths, presence bytes, tags) before its children's. The
//! column slice passed to each call is exactly `Self::COLUMNS` long.

use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet, BinaryHeap, HashMap, HashSet, LinkedList, VecDeque};
use std::hash::{BuildHasher, Hash};
use std::marker::PhantomData;

use crate::error::{Error, Result};
use crate::static_schema::StaticSchema;
use crate::varint;

/// A reading position within one column's bytes.
pub struct ColumnCursor<'de> {
    pub(crate) buf: &'de [u8],
    pub(crate) pos: usize,
}

impl<'de> ColumnCursor<'de> {
    /// Takes the next `n` bytes.
    ///
    /// # Errors
    ///
    /// [`Error::UnexpectedEof`] if the column is exhausted.
    #[inline]
    pub fn take(&mut self, n: usize) -> Result<&'de [u8]> {
        let end = self.pos.checked_add(n).ok_or(Error::UnexpectedEof)?;
        let slice = self.buf.get(self.pos..end).ok_or(Error::UnexpectedEof)?;
        self.pos = end;
        Ok(slice)
    }

    /// Takes the next `N` bytes as an array.
    ///
    /// # Errors
    ///
    /// [`Error::UnexpectedEof`] if the column is exhausted.
    #[inline]
    pub fn fixed<const N: usize>(&mut self) -> Result<[u8; N]> {
        let slice = self.take(N)?;
        Ok(<[u8; N]>::try_from(slice).expect("take returned exactly N bytes"))
    }

    /// Takes the next byte.
    ///
    /// # Errors
    ///
    /// [`Error::UnexpectedEof`] if the column is exhausted.
    #[inline]
    pub fn byte(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }

    /// Reads an unsigned LEB128 varint.
    ///
    /// # Errors
    ///
    /// [`Error::UnexpectedEof`] / [`Error::InvalidVarint`] on truncated or
    /// malformed input.
    #[inline]
    pub fn varint(&mut self) -> Result<u64> {
        let (value, used) = varint::read(&self.buf[self.pos..])?;
        self.pos += used;
        Ok(value)
    }

    /// Reads a varint and converts it to a length.
    ///
    /// # Errors
    ///
    /// As [`Self::varint`], plus [`Error::Malformed`] if the value overflows
    /// `usize`.
    #[inline]
    pub fn varint_len(&mut self) -> Result<usize> {
        usize::try_from(self.varint()?).map_err(|_| Error::Malformed("length overflows usize"))
    }

    /// Whether every byte of this column has been consumed.
    #[must_use]
    #[inline]
    pub fn at_end(&self) -> bool {
        self.pos == self.buf.len()
    }

    /// Bytes not yet consumed.
    #[inline]
    fn remaining(&self) -> usize {
        self.buf.len() - self.pos
    }
}

/// Appends an unsigned LEB128 varint to a column.
#[inline]
pub fn write_varint(column: &mut Vec<u8>, value: u64) {
    varint::write(column, value);
}

/// Splits the first `n` columns off `rest`, advancing it. Used by generated
/// code to hand each field its column subslice.
#[doc(hidden)]
#[inline]
pub fn __split<'a, C>(rest: &mut &'a mut [C], n: usize) -> &'a mut [C] {
    let (head, tail) = std::mem::take(rest).split_at_mut(n);
    *rest = tail;
    head
}

#[doc(hidden)]
pub fn __invalid_variant(tag: u64) -> Error {
    Error::InvalidTag {
        what: "enum variant",
        value: tag,
    }
}

#[doc(hidden)]
pub fn __skipped_variant(name: &'static str) -> Error {
    Error::Message(format!(
        "variant `{name}` is marked serde(skip) and cannot be serialized"
    ))
}

/// Cap for length-hint preallocation, so malformed input can't trigger a
/// huge allocation before the data proves itself.
const CAUTIOUS_CAPACITY: usize = 4096;

/// Preallocation bound for `len` claimed elements. A value occupying at
/// least one column always consumes at least one byte, so the remaining
/// input bytes bound the element count; zero-column element types fall back
/// to a fixed cap (zero-column non-ZSTs exist via `serde(skip)`).
#[inline]
fn cautious_capacity(element_columns: usize, len: usize, cursors: &[ColumnCursor<'_>]) -> usize {
    if element_columns == 0 {
        len.min(CAUTIOUS_CAPACITY)
    } else {
        len.min(cursors.iter().map(ColumnCursor::remaining).sum())
    }
}

/// Writes `self`'s leaves directly into column buffers.
///
/// See the [module docs](self) for the layout contract. Implement via
/// `#[derive(Schema)]`.
pub trait SerializeColumns: StaticSchema {
    /// Number of columns this type's schema occupies.
    const COLUMNS: usize;
    /// `Some(width)` iff this type is a single fixed-width column; lets
    /// sequences bulk-reserve.
    const FIXED_WIDTH: Option<usize> = None;

    /// Appends one value to `columns`, which is exactly `Self::COLUMNS` long.
    ///
    /// # Errors
    ///
    /// Implementations report unrepresentable values (e.g. a `serde(skip)`
    /// enum variant, a pre-epoch `SystemTime`).
    fn serialize_columns(&self, columns: &mut [Vec<u8>]) -> Result<()>;
}

/// Reads `Self` directly from column cursors.
///
/// See the [module docs](self) for the layout contract. Implement via
/// `#[derive(Schema)]`.
pub trait DeserializeColumns<'de>: StaticSchema + Sized {
    /// Number of columns this type's schema occupies.
    const COLUMNS: usize;

    /// Reads one value from `cursors`, which is exactly `Self::COLUMNS` long.
    ///
    /// # Errors
    ///
    /// Any decoding failure: truncated columns, invalid tags or presence
    /// bytes, invalid UTF-8, or domain errors (`NonZero*` zero, invalid
    /// `char`).
    fn deserialize_columns(cursors: &mut [ColumnCursor<'de>]) -> Result<Self>;
}

// ---------------------------------------------------------------------------
// Primitives.
// ---------------------------------------------------------------------------

macro_rules! primitive_columns {
    ($($ty:ty => $width:expr,)*) => {$(
        impl SerializeColumns for $ty {
            const COLUMNS: usize = 1;
            const FIXED_WIDTH: Option<usize> = Some($width);

            #[inline]
            fn serialize_columns(&self, columns: &mut [Vec<u8>]) -> Result<()> {
                columns[0].extend_from_slice(&self.to_le_bytes());
                Ok(())
            }
        }

        impl<'de> DeserializeColumns<'de> for $ty {
            const COLUMNS: usize = 1;

            #[inline]
            fn deserialize_columns(cursors: &mut [ColumnCursor<'de>]) -> Result<Self> {
                Ok(<$ty>::from_le_bytes(cursors[0].fixed()?))
            }
        }
    )*};
}

primitive_columns! {
    i8 => 1, i16 => 2, i32 => 4, i64 => 8, i128 => 16,
    u8 => 1, u16 => 2, u32 => 4, u64 => 8, u128 => 16,
    f32 => 4, f64 => 8,
}

impl SerializeColumns for bool {
    const COLUMNS: usize = 1;
    const FIXED_WIDTH: Option<usize> = Some(1);

    #[inline]
    fn serialize_columns(&self, columns: &mut [Vec<u8>]) -> Result<()> {
        columns[0].push(u8::from(*self));
        Ok(())
    }
}

impl<'de> DeserializeColumns<'de> for bool {
    const COLUMNS: usize = 1;

    #[inline]
    fn deserialize_columns(cursors: &mut [ColumnCursor<'de>]) -> Result<Self> {
        match cursors[0].byte()? {
            0 => Ok(false),
            1 => Ok(true),
            other => Err(Error::InvalidTag {
                what: "bool",
                value: u64::from(other),
            }),
        }
    }
}

impl SerializeColumns for char {
    const COLUMNS: usize = 1;
    const FIXED_WIDTH: Option<usize> = Some(4);

    #[inline]
    fn serialize_columns(&self, columns: &mut [Vec<u8>]) -> Result<()> {
        columns[0].extend_from_slice(&(*self as u32).to_le_bytes());
        Ok(())
    }
}

impl<'de> DeserializeColumns<'de> for char {
    const COLUMNS: usize = 1;

    #[inline]
    fn deserialize_columns(cursors: &mut [ColumnCursor<'de>]) -> Result<Self> {
        let scalar = u32::from_le_bytes(cursors[0].fixed()?);
        char::from_u32(scalar).ok_or(Error::InvalidTag {
            what: "char",
            value: u64::from(scalar),
        })
    }
}

// serde puts usize/isize on the wire as u64/i64.
impl SerializeColumns for usize {
    const COLUMNS: usize = 1;
    const FIXED_WIDTH: Option<usize> = Some(8);

    #[inline]
    fn serialize_columns(&self, columns: &mut [Vec<u8>]) -> Result<()> {
        columns[0].extend_from_slice(&(*self as u64).to_le_bytes());
        Ok(())
    }
}

impl<'de> DeserializeColumns<'de> for usize {
    const COLUMNS: usize = 1;

    #[inline]
    fn deserialize_columns(cursors: &mut [ColumnCursor<'de>]) -> Result<Self> {
        usize::try_from(u64::from_le_bytes(cursors[0].fixed()?))
            .map_err(|_| Error::Malformed("value overflows usize"))
    }
}

impl SerializeColumns for isize {
    const COLUMNS: usize = 1;
    const FIXED_WIDTH: Option<usize> = Some(8);

    #[inline]
    fn serialize_columns(&self, columns: &mut [Vec<u8>]) -> Result<()> {
        columns[0].extend_from_slice(&(*self as i64).to_le_bytes());
        Ok(())
    }
}

impl<'de> DeserializeColumns<'de> for isize {
    const COLUMNS: usize = 1;

    #[inline]
    fn deserialize_columns(cursors: &mut [ColumnCursor<'de>]) -> Result<Self> {
        isize::try_from(i64::from_le_bytes(cursors[0].fixed()?))
            .map_err(|_| Error::Malformed("value overflows isize"))
    }
}

macro_rules! nonzero_columns {
    ($($ty:ty => $prim:ty,)*) => {$(
        impl SerializeColumns for $ty {
            const COLUMNS: usize = 1;
            const FIXED_WIDTH: Option<usize> = <$prim as SerializeColumns>::FIXED_WIDTH;

            #[inline]
            fn serialize_columns(&self, columns: &mut [Vec<u8>]) -> Result<()> {
                self.get().serialize_columns(columns)
            }
        }

        impl<'de> DeserializeColumns<'de> for $ty {
            const COLUMNS: usize = 1;

            #[inline]
            fn deserialize_columns(cursors: &mut [ColumnCursor<'de>]) -> Result<Self> {
                let value = <$prim as DeserializeColumns>::deserialize_columns(cursors)?;
                <$ty>::new(value)
                    .ok_or_else(|| Error::Message("expected a non-zero value".to_owned()))
            }
        }
    )*};
}

nonzero_columns! {
    std::num::NonZeroI8 => i8,
    std::num::NonZeroI16 => i16,
    std::num::NonZeroI32 => i32,
    std::num::NonZeroI64 => i64,
    std::num::NonZeroI128 => i128,
    std::num::NonZeroIsize => isize,
    std::num::NonZeroU8 => u8,
    std::num::NonZeroU16 => u16,
    std::num::NonZeroU32 => u32,
    std::num::NonZeroU64 => u64,
    std::num::NonZeroU128 => u128,
    std::num::NonZeroUsize => usize,
}

// ---------------------------------------------------------------------------
// Unit, strings.
// ---------------------------------------------------------------------------

impl SerializeColumns for () {
    const COLUMNS: usize = 0;

    #[inline]
    fn serialize_columns(&self, _columns: &mut [Vec<u8>]) -> Result<()> {
        Ok(())
    }
}

impl<'de> DeserializeColumns<'de> for () {
    const COLUMNS: usize = 0;

    #[inline]
    fn deserialize_columns(_cursors: &mut [ColumnCursor<'de>]) -> Result<Self> {
        Ok(())
    }
}

impl SerializeColumns for str {
    const COLUMNS: usize = 2;

    #[inline]
    fn serialize_columns(&self, columns: &mut [Vec<u8>]) -> Result<()> {
        write_varint(&mut columns[0], self.len() as u64);
        columns[1].extend_from_slice(self.as_bytes());
        Ok(())
    }
}

impl SerializeColumns for String {
    const COLUMNS: usize = 2;

    #[inline]
    fn serialize_columns(&self, columns: &mut [Vec<u8>]) -> Result<()> {
        self.as_str().serialize_columns(columns)
    }
}

impl<'de> DeserializeColumns<'de> for String {
    const COLUMNS: usize = 2;

    #[inline]
    fn deserialize_columns(cursors: &mut [ColumnCursor<'de>]) -> Result<Self> {
        let len = cursors[0].varint_len()?;
        let bytes = cursors[1].take(len)?;
        String::from_utf8(bytes.to_vec()).map_err(|_| Error::InvalidUtf8)
    }
}

impl<'de: 'a, 'a> DeserializeColumns<'de> for &'a str {
    const COLUMNS: usize = 2;

    #[inline]
    fn deserialize_columns(cursors: &mut [ColumnCursor<'de>]) -> Result<Self> {
        let len = cursors[0].varint_len()?;
        let bytes = cursors[1].take(len)?;
        std::str::from_utf8(bytes).map_err(|_| Error::InvalidUtf8)
    }
}

impl<'de: 'a, 'a> DeserializeColumns<'de> for Cow<'a, str> {
    const COLUMNS: usize = 2;

    #[inline]
    fn deserialize_columns(cursors: &mut [ColumnCursor<'de>]) -> Result<Self> {
        Ok(Cow::Borrowed(<&str>::deserialize_columns(cursors)?))
    }
}

// ---------------------------------------------------------------------------
// Option, sequences, arrays, tuples, maps.
// ---------------------------------------------------------------------------

impl<T: SerializeColumns> SerializeColumns for Option<T> {
    const COLUMNS: usize = 1 + T::COLUMNS;

    fn serialize_columns(&self, columns: &mut [Vec<u8>]) -> Result<()> {
        match self {
            None => {
                columns[0].push(0);
                Ok(())
            }
            Some(value) => {
                columns[0].push(1);
                value.serialize_columns(&mut columns[1..])
            }
        }
    }
}

impl<'de, T: DeserializeColumns<'de>> DeserializeColumns<'de> for Option<T> {
    const COLUMNS: usize = 1 + T::COLUMNS;

    fn deserialize_columns(cursors: &mut [ColumnCursor<'de>]) -> Result<Self> {
        match cursors[0].byte()? {
            0 => Ok(None),
            1 => Ok(Some(T::deserialize_columns(&mut cursors[1..])?)),
            other => Err(Error::InvalidTag {
                what: "presence",
                value: u64::from(other),
            }),
        }
    }
}

impl<T: SerializeColumns> SerializeColumns for Vec<T> {
    const COLUMNS: usize = 1 + T::COLUMNS;

    fn serialize_columns(&self, columns: &mut [Vec<u8>]) -> Result<()> {
        write_varint(&mut columns[0], self.len() as u64);
        let elems = &mut columns[1..];
        // Const-folds away for non-primitive element types.
        if let Some(width) = T::FIXED_WIDTH {
            elems[0].reserve(self.len() * width);
        }
        for item in self {
            item.serialize_columns(&mut *elems)?;
        }
        Ok(())
    }
}

impl<'de, T: DeserializeColumns<'de>> DeserializeColumns<'de> for Vec<T> {
    const COLUMNS: usize = 1 + T::COLUMNS;

    fn deserialize_columns(cursors: &mut [ColumnCursor<'de>]) -> Result<Self> {
        let len = cursors[0].varint_len()?;
        let elems = &mut cursors[1..];
        let mut out = Vec::with_capacity(cautious_capacity(T::COLUMNS, len, elems));
        for _ in 0..len {
            out.push(T::deserialize_columns(&mut *elems)?);
        }
        Ok(out)
    }
}

macro_rules! seq_columns {
    ($($ty:ident => [$($bound:path),*],)*) => {$(
        impl<T: SerializeColumns $(+ $bound)*> SerializeColumns for $ty<T> {
            const COLUMNS: usize = 1 + T::COLUMNS;

            fn serialize_columns(&self, columns: &mut [Vec<u8>]) -> Result<()> {
                write_varint(&mut columns[0], self.len() as u64);
                let elems = &mut columns[1..];
                for item in self {
                    item.serialize_columns(&mut *elems)?;
                }
                Ok(())
            }
        }

        impl<'de, T: DeserializeColumns<'de> $(+ $bound)*> DeserializeColumns<'de> for $ty<T> {
            const COLUMNS: usize = 1 + T::COLUMNS;

            fn deserialize_columns(cursors: &mut [ColumnCursor<'de>]) -> Result<Self> {
                let len = cursors[0].varint_len()?;
                let elems = &mut cursors[1..];
                (0..len).map(|_| T::deserialize_columns(&mut *elems)).collect()
            }
        }
    )*};
}

seq_columns! {
    VecDeque => [],
    LinkedList => [],
    BinaryHeap => [Ord],
    BTreeSet => [Ord],
}

impl<T: SerializeColumns, S> SerializeColumns for HashSet<T, S> {
    const COLUMNS: usize = 1 + T::COLUMNS;

    fn serialize_columns(&self, columns: &mut [Vec<u8>]) -> Result<()> {
        write_varint(&mut columns[0], self.len() as u64);
        let elems = &mut columns[1..];
        for item in self {
            item.serialize_columns(&mut *elems)?;
        }
        Ok(())
    }
}

impl<'de, T, S> DeserializeColumns<'de> for HashSet<T, S>
where
    T: DeserializeColumns<'de> + Hash + Eq,
    S: BuildHasher + Default,
{
    const COLUMNS: usize = 1 + T::COLUMNS;

    fn deserialize_columns(cursors: &mut [ColumnCursor<'de>]) -> Result<Self> {
        let len = cursors[0].varint_len()?;
        let elems = &mut cursors[1..];
        (0..len)
            .map(|_| T::deserialize_columns(&mut *elems))
            .collect()
    }
}

impl<T: SerializeColumns, const N: usize> SerializeColumns for [T; N] {
    const COLUMNS: usize = N * T::COLUMNS;

    fn serialize_columns(&self, columns: &mut [Vec<u8>]) -> Result<()> {
        let mut rest = columns;
        for item in self {
            item.serialize_columns(__split(&mut rest, T::COLUMNS))?;
        }
        Ok(())
    }
}

impl<'de, T: DeserializeColumns<'de>, const N: usize> DeserializeColumns<'de> for [T; N] {
    const COLUMNS: usize = N * T::COLUMNS;

    fn deserialize_columns(cursors: &mut [ColumnCursor<'de>]) -> Result<Self> {
        // Build in place via `array::from_fn` — no heap allocation. The first
        // error short-circuits the remaining reads and is returned after.
        let mut rest = &mut *cursors;
        let mut first_error = None;
        let items: [Option<T>; N] = std::array::from_fn(|_| {
            if first_error.is_some() {
                return None;
            }
            match T::deserialize_columns(__split(&mut rest, T::COLUMNS)) {
                Ok(item) => Some(item),
                Err(err) => {
                    first_error = Some(err);
                    None
                }
            }
        });
        match first_error {
            None => Ok(items.map(|item| item.expect("all items read without error"))),
            Some(err) => Err(err),
        }
    }
}

macro_rules! tuple_columns {
    ($(($($idx:tt $name:ident)+),)*) => {$(
        impl<$($name: SerializeColumns),+> SerializeColumns for ($($name,)+) {
            const COLUMNS: usize = 0 $(+ $name::COLUMNS)+;

            fn serialize_columns(&self, columns: &mut [Vec<u8>]) -> Result<()> {
                let mut rest = columns;
                $(
                    self.$idx.serialize_columns(__split(&mut rest, $name::COLUMNS))?;
                )+
                Ok(())
            }
        }

        impl<'de, $($name: DeserializeColumns<'de>),+> DeserializeColumns<'de> for ($($name,)+) {
            const COLUMNS: usize = 0 $(+ $name::COLUMNS)+;

            fn deserialize_columns(cursors: &mut [ColumnCursor<'de>]) -> Result<Self> {
                let mut rest = cursors;
                Ok(($(
                    $name::deserialize_columns(__split(&mut rest, $name::COLUMNS))?,
                )+))
            }
        }
    )*};
}

tuple_columns! {
    (0 T0),
    (0 T0 1 T1),
    (0 T0 1 T1 2 T2),
    (0 T0 1 T1 2 T2 3 T3),
    (0 T0 1 T1 2 T2 3 T3 4 T4),
    (0 T0 1 T1 2 T2 3 T3 4 T4 5 T5),
    (0 T0 1 T1 2 T2 3 T3 4 T4 5 T5 6 T6),
    (0 T0 1 T1 2 T2 3 T3 4 T4 5 T5 6 T6 7 T7),
    (0 T0 1 T1 2 T2 3 T3 4 T4 5 T5 6 T6 7 T7 8 T8),
    (0 T0 1 T1 2 T2 3 T3 4 T4 5 T5 6 T6 7 T7 8 T8 9 T9),
    (0 T0 1 T1 2 T2 3 T3 4 T4 5 T5 6 T6 7 T7 8 T8 9 T9 10 T10),
    (0 T0 1 T1 2 T2 3 T3 4 T4 5 T5 6 T6 7 T7 8 T8 9 T9 10 T10 11 T11),
    (0 T0 1 T1 2 T2 3 T3 4 T4 5 T5 6 T6 7 T7 8 T8 9 T9 10 T10 11 T11 12 T12),
    (0 T0 1 T1 2 T2 3 T3 4 T4 5 T5 6 T6 7 T7 8 T8 9 T9 10 T10 11 T11 12 T12 13 T13),
    (0 T0 1 T1 2 T2 3 T3 4 T4 5 T5 6 T6 7 T7 8 T8 9 T9 10 T10 11 T11 12 T12 13 T13 14 T14),
    (0 T0 1 T1 2 T2 3 T3 4 T4 5 T5 6 T6 7 T7 8 T8 9 T9 10 T10 11 T11 12 T12 13 T13 14 T14 15 T15),
}

macro_rules! map_columns {
    ($ty:ident, $($de_key_bound:path),+) => {
        impl<'de, K, V> DeserializeColumns<'de> for $ty<K, V>
        where
            K: DeserializeColumns<'de> $(+ $de_key_bound)+,
            V: DeserializeColumns<'de>,
        {
            const COLUMNS: usize = 1 + K::COLUMNS + V::COLUMNS;

            fn deserialize_columns(cursors: &mut [ColumnCursor<'de>]) -> Result<Self> {
                let len = cursors[0].varint_len()?;
                let (keys, values) = cursors[1..].split_at_mut(K::COLUMNS);
                (0..len)
                    .map(|_| {
                        Ok((K::deserialize_columns(&mut *keys)?, V::deserialize_columns(&mut *values)?))
                    })
                    .collect()
            }
        }
    };
}

impl<K: SerializeColumns, V: SerializeColumns, S> SerializeColumns for HashMap<K, V, S> {
    const COLUMNS: usize = 1 + K::COLUMNS + V::COLUMNS;

    fn serialize_columns(&self, columns: &mut [Vec<u8>]) -> Result<()> {
        write_varint(&mut columns[0], self.len() as u64);
        let (keys, values) = columns[1..].split_at_mut(K::COLUMNS);
        for (key, value) in self {
            key.serialize_columns(&mut *keys)?;
            value.serialize_columns(&mut *values)?;
        }
        Ok(())
    }
}

impl<K: SerializeColumns, V: SerializeColumns> SerializeColumns for BTreeMap<K, V> {
    const COLUMNS: usize = 1 + K::COLUMNS + V::COLUMNS;

    fn serialize_columns(&self, columns: &mut [Vec<u8>]) -> Result<()> {
        write_varint(&mut columns[0], self.len() as u64);
        let (keys, values) = columns[1..].split_at_mut(K::COLUMNS);
        for (key, value) in self {
            key.serialize_columns(&mut *keys)?;
            value.serialize_columns(&mut *values)?;
        }
        Ok(())
    }
}

map_columns!(BTreeMap, Ord);

impl<'de, K, V, S> DeserializeColumns<'de> for HashMap<K, V, S>
where
    K: DeserializeColumns<'de> + Hash + Eq,
    V: DeserializeColumns<'de>,
    S: BuildHasher + Default,
{
    const COLUMNS: usize = 1 + K::COLUMNS + V::COLUMNS;

    fn deserialize_columns(cursors: &mut [ColumnCursor<'de>]) -> Result<Self> {
        let len = cursors[0].varint_len()?;
        let (keys, values) = cursors[1..].split_at_mut(K::COLUMNS);
        (0..len)
            .map(|_| {
                Ok((
                    K::deserialize_columns(&mut *keys)?,
                    V::deserialize_columns(&mut *values)?,
                ))
            })
            .collect()
    }
}

// ---------------------------------------------------------------------------
// Pointers and wrappers.
// ---------------------------------------------------------------------------

impl<T: SerializeColumns + ?Sized> SerializeColumns for &T {
    const COLUMNS: usize = T::COLUMNS;
    const FIXED_WIDTH: Option<usize> = T::FIXED_WIDTH;

    #[inline]
    fn serialize_columns(&self, columns: &mut [Vec<u8>]) -> Result<()> {
        (**self).serialize_columns(columns)
    }
}

impl<T: SerializeColumns + ?Sized> SerializeColumns for &mut T {
    const COLUMNS: usize = T::COLUMNS;
    const FIXED_WIDTH: Option<usize> = T::FIXED_WIDTH;

    #[inline]
    fn serialize_columns(&self, columns: &mut [Vec<u8>]) -> Result<()> {
        (**self).serialize_columns(columns)
    }
}

impl<T: SerializeColumns + ?Sized> SerializeColumns for Box<T> {
    const COLUMNS: usize = T::COLUMNS;
    const FIXED_WIDTH: Option<usize> = T::FIXED_WIDTH;

    #[inline]
    fn serialize_columns(&self, columns: &mut [Vec<u8>]) -> Result<()> {
        (**self).serialize_columns(columns)
    }
}

impl<'de, T: DeserializeColumns<'de>> DeserializeColumns<'de> for Box<T> {
    const COLUMNS: usize = T::COLUMNS;

    #[inline]
    fn deserialize_columns(cursors: &mut [ColumnCursor<'de>]) -> Result<Self> {
        Ok(Box::new(T::deserialize_columns(cursors)?))
    }
}

impl<T: SerializeColumns + ?Sized> SerializeColumns for std::rc::Rc<T> {
    const COLUMNS: usize = T::COLUMNS;
    const FIXED_WIDTH: Option<usize> = T::FIXED_WIDTH;

    #[inline]
    fn serialize_columns(&self, columns: &mut [Vec<u8>]) -> Result<()> {
        (**self).serialize_columns(columns)
    }
}

impl<'de, T: DeserializeColumns<'de>> DeserializeColumns<'de> for std::rc::Rc<T> {
    const COLUMNS: usize = T::COLUMNS;

    #[inline]
    fn deserialize_columns(cursors: &mut [ColumnCursor<'de>]) -> Result<Self> {
        Ok(std::rc::Rc::new(T::deserialize_columns(cursors)?))
    }
}

impl<T: SerializeColumns + ?Sized> SerializeColumns for std::sync::Arc<T> {
    const COLUMNS: usize = T::COLUMNS;
    const FIXED_WIDTH: Option<usize> = T::FIXED_WIDTH;

    #[inline]
    fn serialize_columns(&self, columns: &mut [Vec<u8>]) -> Result<()> {
        (**self).serialize_columns(columns)
    }
}

impl<'de, T: DeserializeColumns<'de>> DeserializeColumns<'de> for std::sync::Arc<T> {
    const COLUMNS: usize = T::COLUMNS;

    #[inline]
    fn deserialize_columns(cursors: &mut [ColumnCursor<'de>]) -> Result<Self> {
        Ok(std::sync::Arc::new(T::deserialize_columns(cursors)?))
    }
}

impl<T: SerializeColumns + ToOwned + ?Sized> SerializeColumns for Cow<'_, T> {
    const COLUMNS: usize = T::COLUMNS;
    const FIXED_WIDTH: Option<usize> = T::FIXED_WIDTH;

    #[inline]
    fn serialize_columns(&self, columns: &mut [Vec<u8>]) -> Result<()> {
        self.as_ref().serialize_columns(columns)
    }
}

// ---------------------------------------------------------------------------
// Result, PhantomData, time.
// ---------------------------------------------------------------------------

impl<T: SerializeColumns, E: SerializeColumns> SerializeColumns for Result<T, E> {
    const COLUMNS: usize = 1 + T::COLUMNS + E::COLUMNS;

    fn serialize_columns(&self, columns: &mut [Vec<u8>]) -> Result<()> {
        match self {
            Ok(value) => {
                write_varint(&mut columns[0], 0);
                value.serialize_columns(&mut columns[1..1 + T::COLUMNS])
            }
            Err(err) => {
                write_varint(&mut columns[0], 1);
                err.serialize_columns(&mut columns[1 + T::COLUMNS..])
            }
        }
    }
}

impl<'de, T: DeserializeColumns<'de>, E: DeserializeColumns<'de>> DeserializeColumns<'de>
    for Result<T, E>
{
    const COLUMNS: usize = 1 + T::COLUMNS + E::COLUMNS;

    fn deserialize_columns(cursors: &mut [ColumnCursor<'de>]) -> Result<Self> {
        match cursors[0].varint()? {
            0 => Ok(Ok(T::deserialize_columns(&mut cursors[1..1 + T::COLUMNS])?)),
            1 => Ok(Err(E::deserialize_columns(&mut cursors[1 + T::COLUMNS..])?)),
            tag => Err(__invalid_variant(tag)),
        }
    }
}

impl<T: ?Sized> SerializeColumns for PhantomData<T> {
    const COLUMNS: usize = 0;

    #[inline]
    fn serialize_columns(&self, _columns: &mut [Vec<u8>]) -> Result<()> {
        Ok(())
    }
}

impl<'de, T: ?Sized> DeserializeColumns<'de> for PhantomData<T> {
    const COLUMNS: usize = 0;

    #[inline]
    fn deserialize_columns(_cursors: &mut [ColumnCursor<'de>]) -> Result<Self> {
        Ok(PhantomData)
    }
}

impl SerializeColumns for std::time::Duration {
    const COLUMNS: usize = 2;

    fn serialize_columns(&self, columns: &mut [Vec<u8>]) -> Result<()> {
        columns[0].extend_from_slice(&self.as_secs().to_le_bytes());
        columns[1].extend_from_slice(&self.subsec_nanos().to_le_bytes());
        Ok(())
    }
}

impl<'de> DeserializeColumns<'de> for std::time::Duration {
    const COLUMNS: usize = 2;

    fn deserialize_columns(cursors: &mut [ColumnCursor<'de>]) -> Result<Self> {
        let secs = u64::from_le_bytes(cursors[0].fixed()?);
        let nanos = u32::from_le_bytes(cursors[1].fixed()?);
        if nanos >= 1_000_000_000 {
            return Err(Error::Malformed("Duration nanoseconds out of range"));
        }
        Ok(std::time::Duration::new(secs, nanos))
    }
}

impl SerializeColumns for std::time::SystemTime {
    const COLUMNS: usize = 2;

    fn serialize_columns(&self, columns: &mut [Vec<u8>]) -> Result<()> {
        let since_epoch = self
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|_| Error::Message("SystemTime must be later than UNIX_EPOCH".to_owned()))?;
        since_epoch.serialize_columns(columns)
    }
}

impl<'de> DeserializeColumns<'de> for std::time::SystemTime {
    const COLUMNS: usize = 2;

    fn deserialize_columns(cursors: &mut [ColumnCursor<'de>]) -> Result<Self> {
        let since_epoch = std::time::Duration::deserialize_columns(cursors)?;
        std::time::UNIX_EPOCH
            .checked_add(since_epoch)
            .ok_or_else(|| {
                Error::Message("SystemTime overflows the representable range".to_owned())
            })
    }
}

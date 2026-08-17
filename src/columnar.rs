//! The monomorphized columnar fast path.
//!
//! [`SerializeColumns`] and [`DeserializeColumns`] read and write the *exact*
//! byte layout of the serde-driven path, but as generated straight-line code
//! with column offsets known at compile time — no schema interpretation per
//! value. Implementations come from `#[derive(Schema)]`; the impls in this
//! module cover primitives and std containers.
//!
//! The one exception is a field marked `#[carbonite(serde)]`, whose width is
//! only known at runtime and whose value routes back through the serde path;
//! every other field in the same type keeps its folded offsets.
//!
//! The fast path only applies when the wire schema equals the type's own
//! [`StaticSchema`]; for any other schema (older files, foreign writers),
//! use the serde path, which handles evolution.
//!
//! Both traits take their column count from
//! [`StaticSchema::columns`], so the read and write directions cannot disagree
//! about a type's layout.
//!
//! # Contract for manual implementations
//!
//! Prefer the derive. A manual implementation must reproduce, byte for byte,
//! what carbonite's serde-driven path produces for the same `Serialize` /
//! `Deserialize` impls, over the column layout of the type's
//! [`StaticSchema::schema_node`]: columns are assigned depth-first, a node's
//! own columns (lengths, presence bytes, tags) before its children's. The
//! column slice passed to each call is exactly `Self::columns()` long.
//!
//! Any length or count read from the input must be passed through
//! [`checked_count`] before it is used to size an allocation or drive a loop.
//! [`ColumnCursor::new`] builds a cursor over a byte slice so an
//! implementation can be tested directly.
//!
//! The two traits and [`ColumnCursor`] are re-exported at the crate root,
//! since they appear in each other's signatures. The rest of this module —
//! [`write_varint`], [`checked_count`], [`MAX_ZERO_COLUMN_REPEAT`] — is the
//! toolkit for writing an implementation by hand, and stays behind the module
//! path.

use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet, BinaryHeap, HashMap, HashSet, LinkedList, VecDeque};
use std::fmt;
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

impl fmt::Debug for ColumnCursor<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ColumnCursor")
            .field("read", &self.pos)
            .field("remaining", &self.remaining())
            .finish()
    }
}

impl<'de> ColumnCursor<'de> {
    /// Builds a cursor over one column's bytes, positioned at the start.
    #[must_use]
    #[inline]
    pub fn new(buf: &'de [u8]) -> Self {
        ColumnCursor { buf, pos: 0 }
    }

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
        <[u8; N]>::try_from(self.take(N)?).map_err(|_| Error::UnexpectedEof)
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
    /// [`Error::UnexpectedEof`] / [`Error::InvalidVarint`] on truncated,
    /// overlong, or non-canonical input.
    #[inline]
    pub fn varint(&mut self) -> Result<u64> {
        let (value, used) = varint::read(&self.buf[self.pos..])?;
        self.pos += used;
        Ok(value)
    }

    /// Reads a varint and converts it to a length.
    ///
    /// This does **not** check the length against the data available; use
    /// [`checked_count`] for anything that sizes an allocation or a loop.
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
    #[must_use]
    #[inline]
    pub fn remaining(&self) -> usize {
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

/// Reports a failed `#[carbonite(as = "...")]` conversion, with the same
/// message the serde path produces for a `serde(try_from)` failure.
#[doc(hidden)]
pub fn __conversion_failed<E: fmt::Display>(err: E) -> Error {
    Error::Message(err.to_string())
}

#[doc(hidden)]
pub fn __skipped_variant(name: &'static str) -> Error {
    Error::Message(format!(
        "variant `{name}` is marked serde(skip) and cannot be serialized"
    ))
}

/// Byte budget for speculative preallocation, mirroring serde's own caution.
/// A validated count still has only a one-byte-per-element floor behind it,
/// so trusting it to size a reservation would let a blob demand an allocation
/// `size_of::<T>()` times larger than itself.
const MAX_PREALLOC_BYTES: usize = 1 << 20;

/// Largest repetition count accepted for a value that occupies no columns.
///
/// Such a value consumes no bytes, so the input carries nothing to validate
/// its count against and a handful of header bytes could otherwise ask for an
/// unbounded number of copies. Types like `Vec<()>` are the only ones
/// affected, and only past a million elements.
pub const MAX_ZERO_COLUMN_REPEAT: u64 = 1 << 20;

/// Validates a repetition count read from the input against the bytes that
/// remain to satisfy it.
///
/// A value occupying at least one column always writes at least one byte, so
/// the unread bytes across `cursors` bound how many more can follow. When the
/// repeated value occupies no columns at all, the count is instead capped at
/// [`MAX_ZERO_COLUMN_REPEAT`].
///
/// # Errors
///
/// [`Error::LimitExceeded`] if `claimed` is larger than the input could
/// justify, or [`Error::Malformed`] if it overflows `usize`.
#[inline]
pub fn checked_count(
    what: &'static str,
    element_columns: usize,
    claimed: u64,
    cursors: &[ColumnCursor<'_>],
) -> Result<usize> {
    let limit = if element_columns == 0 {
        MAX_ZERO_COLUMN_REPEAT
    } else {
        cursors.iter().map(|c| c.remaining() as u64).sum()
    };
    if claimed > limit {
        return Err(Error::LimitExceeded {
            what,
            claimed,
            limit,
        });
    }
    usize::try_from(claimed).map_err(|_| Error::Malformed("length overflows usize"))
}

/// Preallocation bound for a count already validated by [`read_count`].
#[inline]
fn cautious_capacity<T: StaticSchema>(len: usize) -> usize {
    if T::FIXED_WIDTH.is_some() {
        // The count was bounded by the element's exact byte width, so the
        // reservation cannot outgrow the input.
        len
    } else {
        // The generic bound is one byte per element; reserve at most a
        // budget's worth and let the vector grow to the real length as
        // elements actually decode.
        len.min(MAX_PREALLOC_BYTES / std::mem::size_of::<T>().max(1))
    }
}

/// Reads a validated element count from a length column.
#[inline]
fn read_count<T: ?Sized + StaticSchema>(
    what: &'static str,
    len_column: &mut ColumnCursor<'_>,
    elements: &[ColumnCursor<'_>],
) -> Result<usize> {
    let claimed = len_column.varint()?;
    // A fixed-width element consumes exactly `width` bytes of its single
    // column, so the bound is exact rather than the one-byte floor
    // `checked_count` falls back on.
    if let Some(width) = T::FIXED_WIDTH {
        let available = elements.first().map_or(0, ColumnCursor::remaining);
        let limit = (available / width) as u64;
        if claimed > limit {
            return Err(Error::LimitExceeded {
                what,
                claimed,
                limit,
            });
        }
        return usize::try_from(claimed).map_err(|_| Error::Malformed("length overflows usize"));
    }
    checked_count(what, T::columns(), claimed, elements)
}

/// Writes `self`'s leaves directly into column buffers.
///
/// See the [module docs](self) for the layout contract. Implement via
/// `#[derive(Schema)]`.
pub trait SerializeColumns: StaticSchema {
    /// Appends one value to `columns`, which is exactly
    /// [`Self::columns()`](StaticSchema::columns) long.
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
    /// Reads one value from `cursors`, which is exactly
    /// [`Self::columns()`](StaticSchema::columns) long.
    ///
    /// # Errors
    ///
    /// Any decoding failure: truncated columns, invalid tags or presence
    /// bytes, invalid UTF-8, counts larger than the input can justify, or
    /// domain errors (`NonZero*` zero, invalid `char`).
    fn deserialize_columns(cursors: &mut [ColumnCursor<'de>]) -> Result<Self>;
}

// ---------------------------------------------------------------------------
// Primitives.
// ---------------------------------------------------------------------------

macro_rules! primitive_columns {
    ($($ty:ty,)*) => {$(
        impl SerializeColumns for $ty {
            #[inline]
            fn serialize_columns(&self, columns: &mut [Vec<u8>]) -> Result<()> {
                columns[0].extend_from_slice(&self.to_le_bytes());
                Ok(())
            }
        }

        impl<'de> DeserializeColumns<'de> for $ty {
            #[inline]
            fn deserialize_columns(cursors: &mut [ColumnCursor<'de>]) -> Result<Self> {
                Ok(<$ty>::from_le_bytes(cursors[0].fixed()?))
            }
        }
    )*};
}

primitive_columns! {
    i8, i16, i32, i64, i128,
    u8, u16, u32, u64, u128,
    f32, f64,
}

impl SerializeColumns for bool {
    #[inline]
    fn serialize_columns(&self, columns: &mut [Vec<u8>]) -> Result<()> {
        columns[0].push(u8::from(*self));
        Ok(())
    }
}

impl<'de> DeserializeColumns<'de> for bool {
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
    #[inline]
    fn serialize_columns(&self, columns: &mut [Vec<u8>]) -> Result<()> {
        columns[0].extend_from_slice(&(*self as u32).to_le_bytes());
        Ok(())
    }
}

impl<'de> DeserializeColumns<'de> for char {
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
    #[inline]
    fn serialize_columns(&self, columns: &mut [Vec<u8>]) -> Result<()> {
        columns[0].extend_from_slice(&(*self as u64).to_le_bytes());
        Ok(())
    }
}

impl<'de> DeserializeColumns<'de> for usize {
    #[inline]
    fn deserialize_columns(cursors: &mut [ColumnCursor<'de>]) -> Result<Self> {
        usize::try_from(u64::from_le_bytes(cursors[0].fixed()?))
            .map_err(|_| Error::Malformed("value overflows usize"))
    }
}

impl SerializeColumns for isize {
    #[inline]
    fn serialize_columns(&self, columns: &mut [Vec<u8>]) -> Result<()> {
        columns[0].extend_from_slice(&(*self as i64).to_le_bytes());
        Ok(())
    }
}

impl<'de> DeserializeColumns<'de> for isize {
    #[inline]
    fn deserialize_columns(cursors: &mut [ColumnCursor<'de>]) -> Result<Self> {
        isize::try_from(i64::from_le_bytes(cursors[0].fixed()?))
            .map_err(|_| Error::Malformed("value overflows isize"))
    }
}

macro_rules! nonzero_columns {
    ($($ty:ty => $prim:ty,)*) => {$(
        impl SerializeColumns for $ty {
            #[inline]
            fn serialize_columns(&self, columns: &mut [Vec<u8>]) -> Result<()> {
                self.get().serialize_columns(columns)
            }
        }

        impl<'de> DeserializeColumns<'de> for $ty {
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
    #[inline]
    fn serialize_columns(&self, _columns: &mut [Vec<u8>]) -> Result<()> {
        Ok(())
    }
}

impl<'de> DeserializeColumns<'de> for () {
    #[inline]
    fn deserialize_columns(_cursors: &mut [ColumnCursor<'de>]) -> Result<Self> {
        Ok(())
    }
}

impl SerializeColumns for str {
    #[inline]
    fn serialize_columns(&self, columns: &mut [Vec<u8>]) -> Result<()> {
        write_varint(&mut columns[0], self.len() as u64);
        columns[1].extend_from_slice(self.as_bytes());
        Ok(())
    }
}

impl SerializeColumns for String {
    #[inline]
    fn serialize_columns(&self, columns: &mut [Vec<u8>]) -> Result<()> {
        self.as_str().serialize_columns(columns)
    }
}

impl<'de> DeserializeColumns<'de> for String {
    #[inline]
    fn deserialize_columns(cursors: &mut [ColumnCursor<'de>]) -> Result<Self> {
        Ok(<&str>::deserialize_columns(cursors)?.to_owned())
    }
}

impl<'de: 'a, 'a> DeserializeColumns<'de> for &'a str {
    #[inline]
    fn deserialize_columns(cursors: &mut [ColumnCursor<'de>]) -> Result<Self> {
        let len = cursors[0].varint_len()?;
        let bytes = cursors[1].take(len)?;
        std::str::from_utf8(bytes).map_err(|_| Error::InvalidUtf8)
    }
}

impl<'de: 'a, 'a> DeserializeColumns<'de> for Cow<'a, str> {
    #[inline]
    fn deserialize_columns(cursors: &mut [ColumnCursor<'de>]) -> Result<Self> {
        Ok(Cow::Borrowed(<&str>::deserialize_columns(cursors)?))
    }
}

// ---------------------------------------------------------------------------
// Option, sequences, arrays, tuples, maps.
// ---------------------------------------------------------------------------

impl<T: SerializeColumns> SerializeColumns for Option<T> {
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
    fn deserialize_columns(cursors: &mut [ColumnCursor<'de>]) -> Result<Self> {
        let (len_column, elems) = cursors.split_at_mut(1);
        let len = read_count::<T>("sequence length", &mut len_column[0], elems)?;
        let mut out = Vec::with_capacity(cautious_capacity::<T>(len));
        for _ in 0..len {
            out.push(T::deserialize_columns(&mut *elems)?);
        }
        Ok(out)
    }
}

macro_rules! seq_columns {
    ($($ty:ident => [$($bound:path),*],)*) => {$(
        impl<T: SerializeColumns $(+ $bound)*> SerializeColumns for $ty<T> {
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
            fn deserialize_columns(cursors: &mut [ColumnCursor<'de>]) -> Result<Self> {
                let (len_column, elems) = cursors.split_at_mut(1);
                let len = read_count::<T>("sequence length", &mut len_column[0], elems)?;
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
    fn deserialize_columns(cursors: &mut [ColumnCursor<'de>]) -> Result<Self> {
        let (len_column, elems) = cursors.split_at_mut(1);
        let len = read_count::<T>("sequence length", &mut len_column[0], elems)?;
        (0..len)
            .map(|_| T::deserialize_columns(&mut *elems))
            .collect()
    }
}

impl<T: SerializeColumns, const N: usize> SerializeColumns for [T; N] {
    fn serialize_columns(&self, columns: &mut [Vec<u8>]) -> Result<()> {
        let mut rest = columns;
        for item in self {
            item.serialize_columns(__split(&mut rest, T::columns()))?;
        }
        Ok(())
    }
}

impl<'de, T: DeserializeColumns<'de>, const N: usize> DeserializeColumns<'de> for [T; N] {
    fn deserialize_columns(cursors: &mut [ColumnCursor<'de>]) -> Result<Self> {
        // Build in place via `array::from_fn` — no heap allocation. The first
        // error short-circuits the remaining reads and is returned after.
        let mut rest = &mut *cursors;
        let mut first_error = None;
        let items: [Option<T>; N] = std::array::from_fn(|_| {
            if first_error.is_some() {
                return None;
            }
            match T::deserialize_columns(__split(&mut rest, T::columns())) {
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
            fn serialize_columns(&self, columns: &mut [Vec<u8>]) -> Result<()> {
                let mut rest = columns;
                $(
                    self.$idx.serialize_columns(__split(&mut rest, $name::columns()))?;
                )+
                Ok(())
            }
        }

        impl<'de, $($name: DeserializeColumns<'de>),+> DeserializeColumns<'de> for ($($name,)+) {
            fn deserialize_columns(cursors: &mut [ColumnCursor<'de>]) -> Result<Self> {
                let mut rest = cursors;
                Ok(($(
                    $name::deserialize_columns(__split(&mut rest, $name::columns()))?,
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

impl<K: SerializeColumns, V: SerializeColumns, S> SerializeColumns for HashMap<K, V, S> {
    fn serialize_columns(&self, columns: &mut [Vec<u8>]) -> Result<()> {
        write_varint(&mut columns[0], self.len() as u64);
        let (keys, values) = columns[1..].split_at_mut(K::columns());
        for (key, value) in self {
            key.serialize_columns(&mut *keys)?;
            value.serialize_columns(&mut *values)?;
        }
        Ok(())
    }
}

impl<K: SerializeColumns, V: SerializeColumns> SerializeColumns for BTreeMap<K, V> {
    fn serialize_columns(&self, columns: &mut [Vec<u8>]) -> Result<()> {
        write_varint(&mut columns[0], self.len() as u64);
        let (keys, values) = columns[1..].split_at_mut(K::columns());
        for (key, value) in self {
            key.serialize_columns(&mut *keys)?;
            value.serialize_columns(&mut *values)?;
        }
        Ok(())
    }
}

impl<'de, K, V> DeserializeColumns<'de> for BTreeMap<K, V>
where
    K: DeserializeColumns<'de> + Ord,
    V: DeserializeColumns<'de>,
{
    fn deserialize_columns(cursors: &mut [ColumnCursor<'de>]) -> Result<Self> {
        let (len_column, entries) = cursors.split_at_mut(1);
        let claimed = len_column[0].varint()?;
        let len = checked_count("map length", K::columns() + V::columns(), claimed, entries)?;
        let (keys, values) = entries.split_at_mut(K::columns());
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

impl<'de, K, V, S> DeserializeColumns<'de> for HashMap<K, V, S>
where
    K: DeserializeColumns<'de> + Hash + Eq,
    V: DeserializeColumns<'de>,
    S: BuildHasher + Default,
{
    fn deserialize_columns(cursors: &mut [ColumnCursor<'de>]) -> Result<Self> {
        let (len_column, entries) = cursors.split_at_mut(1);
        let claimed = len_column[0].varint()?;
        let len = checked_count("map length", K::columns() + V::columns(), claimed, entries)?;
        let (keys, values) = entries.split_at_mut(K::columns());
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

macro_rules! deref_ser_columns {
    ($($ty:ty),* $(,)?) => {$(
        impl<T: SerializeColumns + ?Sized> SerializeColumns for $ty {
            #[inline]
            fn serialize_columns(&self, columns: &mut [Vec<u8>]) -> Result<()> {
                (**self).serialize_columns(columns)
            }
        }
    )*};
}

deref_ser_columns!(&T, &mut T, Box<T>, std::rc::Rc<T>, std::sync::Arc<T>);

macro_rules! wrap_de_columns {
    ($($ty:ty => $ctor:path),* $(,)?) => {$(
        impl<'de, T: DeserializeColumns<'de>> DeserializeColumns<'de> for $ty {
            #[inline]
            fn deserialize_columns(cursors: &mut [ColumnCursor<'de>]) -> Result<Self> {
                Ok($ctor(T::deserialize_columns(cursors)?))
            }
        }
    )*};
}

wrap_de_columns! {
    Box<T> => Box::new,
    std::rc::Rc<T> => std::rc::Rc::new,
    std::sync::Arc<T> => std::sync::Arc::new,
}

impl<T: SerializeColumns + ToOwned + ?Sized> SerializeColumns for Cow<'_, T> {
    #[inline]
    fn serialize_columns(&self, columns: &mut [Vec<u8>]) -> Result<()> {
        self.as_ref().serialize_columns(columns)
    }
}

// ---------------------------------------------------------------------------
// Result, PhantomData, time.
// ---------------------------------------------------------------------------

impl<T: SerializeColumns, E: SerializeColumns> SerializeColumns for Result<T, E> {
    fn serialize_columns(&self, columns: &mut [Vec<u8>]) -> Result<()> {
        match self {
            Ok(value) => {
                write_varint(&mut columns[0], 0);
                value.serialize_columns(&mut columns[1..1 + T::columns()])
            }
            Err(err) => {
                write_varint(&mut columns[0], 1);
                err.serialize_columns(&mut columns[1 + T::columns()..])
            }
        }
    }
}

impl<'de, T: DeserializeColumns<'de>, E: DeserializeColumns<'de>> DeserializeColumns<'de>
    for Result<T, E>
{
    fn deserialize_columns(cursors: &mut [ColumnCursor<'de>]) -> Result<Self> {
        match cursors[0].varint()? {
            0 => Ok(Ok(T::deserialize_columns(
                &mut cursors[1..1 + T::columns()],
            )?)),
            1 => Ok(Err(E::deserialize_columns(
                &mut cursors[1 + T::columns()..],
            )?)),
            tag => Err(__invalid_variant(tag)),
        }
    }
}

impl<T: ?Sized> SerializeColumns for PhantomData<T> {
    #[inline]
    fn serialize_columns(&self, _columns: &mut [Vec<u8>]) -> Result<()> {
        Ok(())
    }
}

impl<'de, T: ?Sized> DeserializeColumns<'de> for PhantomData<T> {
    #[inline]
    fn deserialize_columns(_cursors: &mut [ColumnCursor<'de>]) -> Result<Self> {
        Ok(PhantomData)
    }
}

impl SerializeColumns for std::time::Duration {
    fn serialize_columns(&self, columns: &mut [Vec<u8>]) -> Result<()> {
        columns[0].extend_from_slice(&self.as_secs().to_le_bytes());
        columns[1].extend_from_slice(&self.subsec_nanos().to_le_bytes());
        Ok(())
    }
}

impl<'de> DeserializeColumns<'de> for std::time::Duration {
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
    fn serialize_columns(&self, columns: &mut [Vec<u8>]) -> Result<()> {
        let since_epoch = self
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|_| Error::Message("SystemTime must be later than UNIX_EPOCH".to_owned()))?;
        since_epoch.serialize_columns(columns)
    }
}

impl<'de> DeserializeColumns<'de> for std::time::SystemTime {
    fn deserialize_columns(cursors: &mut [ColumnCursor<'de>]) -> Result<Self> {
        let since_epoch = std::time::Duration::deserialize_columns(cursors)?;
        std::time::UNIX_EPOCH
            .checked_add(since_epoch)
            .ok_or_else(|| {
                Error::Message("SystemTime overflows the representable range".to_owned())
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cursor_can_be_built_and_read_directly() {
        let mut cursor = ColumnCursor::new(&[0x2a, 0x00, 0x00, 0x00]);
        assert_eq!(cursor.remaining(), 4);
        assert_eq!(u32::from_le_bytes(cursor.fixed().unwrap()), 42);
        assert!(cursor.at_end());
        assert!(matches!(cursor.byte(), Err(Error::UnexpectedEof)));
    }

    #[test]
    fn checked_count_bounds_by_remaining_bytes() {
        let cursors = [ColumnCursor::new(&[0; 10])];
        assert_eq!(checked_count("test", 1, 10, &cursors).unwrap(), 10);
        assert!(matches!(
            checked_count("test", 1, 11, &cursors),
            Err(Error::LimitExceeded {
                claimed: 11,
                limit: 10,
                ..
            })
        ));
    }

    #[test]
    fn checked_count_caps_zero_column_elements() {
        let cursors: [ColumnCursor<'_>; 0] = [];
        assert_eq!(
            checked_count("test", 0, MAX_ZERO_COLUMN_REPEAT, &cursors).unwrap(),
            MAX_ZERO_COLUMN_REPEAT as usize
        );
        assert!(matches!(
            checked_count("test", 0, MAX_ZERO_COLUMN_REPEAT + 1, &cursors),
            Err(Error::LimitExceeded { .. })
        ));
    }
}

//! Columnar deserialization: a [`serde::Deserializer`] driven by the *file's*
//! schema, reading with one cursor per column.
//!
//! When the file schema is identical to the type's own (traced) schema,
//! structs decode positionally (`visit_seq`) with zero name matching. When
//! they differ — an older or newer file — structs decode as maps keyed by the
//! file's field names, so serde's usual evolution machinery applies:
//! unknown fields are ignored (their columns are skipped), missing fields use
//! `#[serde(default)]` or report `missing field`, and `#[serde(alias)]`
//! works.
//!
//! A file whose fields are *positional* (a tuple, tuple struct, or tuple
//! variant) can also be read into named fields, but only when the reader says
//! which position each field replaces, with `#[serde(alias = "0")]`. Matching
//! it by declaration order instead would not compose with reordering named
//! fields, which is already a no-op; see [`positional_names`] and
//! [`needs_positional_names`]. The reverse — a named file read into a tuple —
//! is refused, since a tuple has nowhere to put the declaration.

use std::borrow::Cow;
use std::fmt;
use std::iter::FusedIterator;
use std::marker::PhantomData;

use serde::de::value::StrDeserializer;
use serde::de::{
    self, Deserialize, DeserializeOwned, DeserializeSeed, EnumAccess, MapAccess, SeqAccess,
    VariantAccess, Visitor,
};

use crate::columnar::{ColumnCursor as Cursor, DeserializeColumns, checked_count};
use crate::error::{Error, Result};
use crate::layout::{Layout, Node, VariantKind};
use crate::schema::{Primitive, Schema};

/// Deserializes values of type `T` from blobs written with a given schema.
///
/// The schema passed in is the **writer's** schema — possibly from an older
/// version of `T`. It is taken [by value or by reference](Self::new): borrow
/// to share one long-lived schema across engines, hand it over when it was
/// parsed from the wire for this deserializer alone. One `Deserializer` can
/// decode any number of blobs.
///
/// # Untrusted input
///
/// Every blob is treated as hostile: the header's row count and every length
/// inside the data are validated against the bytes actually present before
/// they size an allocation or drive a loop, so decoding work stays
/// proportional to the input. Malformed input yields an [`Error`], never a
/// panic or an unbounded allocation.
pub struct Deserializer<'s, T: ?Sized> {
    schema: Cow<'s, Schema<T>>,
    layout: Layout,
    fast: bool,
}

impl<T: ?Sized> fmt::Debug for Deserializer<'_, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Deserializer")
            .field("schema", &format_args!("{}", self.schema))
            .field("columns", &self.layout.columns)
            .field("fast_path", &self.fast)
            .finish()
    }
}

impl<'s, T> Deserializer<'s, T> {
    /// Builds a deserializer, tracing `T` to detect whether the fast
    /// positional path can be used (it can whenever the writer's schema is
    /// identical to `T`'s own).
    ///
    /// If `T` cannot be traced at all — an untagged enum, a `flatten`ed
    /// field — this falls back to the name-matched path rather than failing,
    /// since that path can still decode the blob. [`Self::uses_fast_path`]
    /// reports which one was chosen; [`Schema::new`] surfaces the underlying
    /// tracing error if you want to see it.
    ///
    /// The fast path hands structs to the visitor *positionally*
    /// (`visit_seq`), which every derived `Deserialize` accepts. A
    /// hand-written impl whose visitor only accepts maps will trace
    /// successfully and then reject the sequence; give such a type
    /// [`Self::new_untraced`], which always uses the name-matched path.
    #[must_use]
    pub fn new(schema: impl Into<Cow<'s, Schema<T>>>) -> Self
    where
        T: DeserializeOwned,
    {
        let schema = schema.into();
        let fast = crate::trace::trace::<T>().is_ok_and(|local| local == *schema.node());
        Self::build(schema, fast)
    }
}

impl<'s, T: ?Sized> Deserializer<'s, T> {
    /// Like [`Self::new`], but uses the compile-time schema from
    /// `#[derive(Schema)]` for the fast-path check instead of tracing.
    /// Unlike [`Self::new`], this also works for types that borrow from the
    /// input.
    #[must_use]
    pub fn new_static(schema: impl Into<Cow<'s, Schema<T>>>) -> Self
    where
        T: crate::StaticSchema,
    {
        let schema = schema.into();
        let fast = T::schema_node() == *schema.node();
        Self::build(schema, fast)
    }

    /// Builds a deserializer without tracing `T`; always uses the name-matched
    /// path. Use this for types that borrow from the input (`&str` fields)
    /// and don't implement [`StaticSchema`](crate::StaticSchema).
    #[must_use]
    pub fn new_untraced(schema: impl Into<Cow<'s, Schema<T>>>) -> Self {
        Self::build(schema.into(), false)
    }

    pub(crate) fn build(schema: Cow<'s, Schema<T>>, fast: bool) -> Self {
        let layout = Layout::new(schema.node());
        Deserializer {
            schema,
            layout,
            fast,
        }
    }

    /// The writer's schema.
    #[must_use]
    pub fn schema(&self) -> &Schema<T> {
        &self.schema
    }

    /// Whether the writer's schema matched `T`'s own, so rows decode
    /// positionally instead of by field name.
    ///
    /// `false` means the blob is being reconciled against `T` — an older
    /// file, a foreign writer, or a `T` that could not be traced.
    #[must_use]
    pub fn uses_fast_path(&self) -> bool {
        self.fast
    }

    /// Deserializes a single-value blob (as produced by
    /// [`Serializer::to_vec`](crate::Serializer::to_vec)).
    ///
    /// # Errors
    ///
    /// Fails on malformed input, on schema/type mismatches serde cannot
    /// reconcile, or if the blob holds more than one row (use [`Self::rows`]).
    pub fn from_slice<'de>(&self, input: &'de [u8]) -> Result<T>
    where
        T: Deserialize<'de> + Sized,
    {
        let mut rows = self.rows(input)?;
        if rows.remaining() != 1 {
            return Err(Error::Malformed("expected a single-value blob"));
        }
        rows.next()
            .ok_or(Error::Malformed("expected a single-value blob"))?
    }

    /// Opens a blob and iterates its rows (as produced by
    /// [`Batch`](crate::Batch)).
    ///
    /// # Errors
    ///
    /// Fails if the header is malformed or the column layout does not match
    /// the schema.
    pub fn rows<'a, 'de>(&'a self, input: &'de [u8]) -> Result<Rows<'a, 'de, T>> {
        let (row_count, cursors) = self.open(input)?;
        Ok(Rows {
            de: self,
            cursors,
            remaining: row_count,
        })
    }

    /// Deserializes a single-value blob through the monomorphized columnar
    /// fast path from `#[derive(Schema)]` — no serde dispatch at all.
    ///
    /// Unlike [`Self::from_slice`], this path cannot reconcile schema
    /// differences: it requires the blob's schema to equal the type's own
    /// [`StaticSchema`](crate::StaticSchema). For older or foreign schemas,
    /// use [`Self::from_slice`].
    ///
    /// # Errors
    ///
    /// Fails with [`Error::SchemaMismatch`] when the schema differs from the
    /// type's, and on any of the malformed-input failures of
    /// [`Self::from_slice`].
    pub fn from_slice_columns<'de>(&self, input: &'de [u8]) -> Result<T>
    where
        T: DeserializeColumns<'de>,
    {
        let mut rows = self.rows_columns(input)?;
        if rows.remaining() != 1 {
            return Err(Error::Malformed("expected a single-value blob"));
        }
        rows.next()
            .ok_or(Error::Malformed("expected a single-value blob"))?
    }

    /// Opens a blob and iterates its rows through the monomorphized columnar
    /// fast path from `#[derive(Schema)]` — the reading counterpart of
    /// [`Batch::push_columns`](crate::Batch::push_columns).
    ///
    /// Like [`Self::from_slice_columns`], this path cannot reconcile schema
    /// differences: it requires the blob's schema to equal the type's own
    /// [`StaticSchema`](crate::StaticSchema). For older or foreign schemas,
    /// use [`Self::rows`].
    ///
    /// # Errors
    ///
    /// Fails with [`Error::SchemaMismatch`] when the schema differs from the
    /// type's, or if the header is malformed.
    pub fn rows_columns<'de>(&self, input: &'de [u8]) -> Result<RowsColumns<'de, T>>
    where
        T: DeserializeColumns<'de>,
    {
        if !self.fast && T::schema_node() != *self.schema.node() {
            return Err(columnar_schema_mismatch());
        }
        let (row_count, cursors) = self.open(input)?;
        Ok(RowsColumns {
            cursors,
            remaining: row_count,
            _marker: PhantomData,
        })
    }

    /// Parses the blob header and slices one cursor per column.
    fn open<'de>(&self, input: &'de [u8]) -> Result<(u64, Vec<Cursor<'de>>)> {
        let mut header = Cursor::new(input);
        let row_count = header.varint()?;
        let column_count = header.varint()?;
        if column_count != self.layout.columns as u64 {
            return Err(Error::Malformed("column count does not match schema"));
        }
        let mut lengths = Vec::with_capacity(self.layout.columns.min(1024));
        for _ in 0..self.layout.columns {
            lengths.push(header.varint_len()?);
        }
        let mut cursors = Vec::with_capacity(lengths.len());
        for length in lengths {
            cursors.push(Cursor::new(header.take(length)?));
        }
        if !header.at_end() {
            return Err(Error::TrailingBytes);
        }
        // A row occupying at least one column writes at least one byte, so
        // the column bytes bound how many rows the header can legitimately
        // claim. Without this a tampered count drives an unbounded
        // allocation in any consumer that trusts it.
        checked_count("row count", self.layout.columns, row_count, &cursors)?;
        Ok((row_count, cursors))
    }
}

pub(crate) fn columnar_schema_mismatch() -> Error {
    Error::SchemaMismatch {
        expected: "the type's own static schema (columnar fast path)".to_owned(),
        found: "a schema that differs from it; use the serde path for evolution".to_owned(),
    }
}

/// Iterator over the rows of one blob.
///
/// After an error, iteration fuses (yields `None`); a decoding error mid-blob
/// leaves the column cursors in an unspecified position.
pub struct Rows<'a, 'de, T: ?Sized> {
    de: &'a Deserializer<'a, T>,
    cursors: Vec<Cursor<'de>>,
    remaining: u64,
}

impl<T: ?Sized> fmt::Debug for Rows<'_, '_, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Rows")
            .field("remaining", &self.remaining)
            .field("columns", &self.cursors.len())
            .finish()
    }
}

impl<T: ?Sized> Rows<'_, '_, T> {
    /// Rows not yet read.
    ///
    /// The blob header is validated against the bytes present when the blob
    /// is opened, so this count cannot exceed what the input could hold.
    #[must_use]
    pub fn remaining(&self) -> u64 {
        self.remaining
    }
}

impl<'de, T> Iterator for Rows<'_, 'de, T>
where
    T: Deserialize<'de>,
{
    type Item = Result<T>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.remaining == 0 {
            return None;
        }
        self.remaining -= 1;
        let _shared_scope = crate::shared::RowScope::begin();
        let result = T::deserialize(ValueDeserializer {
            node: &self.de.layout.root,
            cursors: &mut self.cursors,
            fast: self.de.fast,
        })
        .and_then(|value| {
            // The last row must consume every column exactly.
            if self.remaining == 0 && !self.cursors.iter().all(Cursor::at_end) {
                return Err(Error::TrailingBytes);
            }
            Ok(value)
        });
        if result.is_err() {
            self.remaining = 0;
        }
        Some(result)
    }

    /// The lower bound is always `0`: the header's row count says how many
    /// rows the blob *claims*, but any of them may fail to decode, and a
    /// consumer must never size an allocation from a number the input has
    /// not yet earned.
    fn size_hint(&self) -> (usize, Option<usize>) {
        (0, usize::try_from(self.remaining).ok())
    }
}

impl<'de, T> FusedIterator for Rows<'_, 'de, T> where T: Deserialize<'de> {}

/// Iterator over the rows of one blob, decoded through the monomorphized
/// columnar fast path. See [`Deserializer::rows_columns`].
///
/// After an error, iteration fuses (yields `None`).
pub struct RowsColumns<'de, T: ?Sized> {
    cursors: Vec<Cursor<'de>>,
    remaining: u64,
    _marker: PhantomData<fn() -> T>,
}

impl<T: ?Sized> fmt::Debug for RowsColumns<'_, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RowsColumns")
            .field("remaining", &self.remaining)
            .field("columns", &self.cursors.len())
            .finish()
    }
}

impl<T: ?Sized> RowsColumns<'_, T> {
    /// Rows not yet read.
    #[must_use]
    pub fn remaining(&self) -> u64 {
        self.remaining
    }
}

impl<'de, T> Iterator for RowsColumns<'de, T>
where
    T: DeserializeColumns<'de>,
{
    type Item = Result<T>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.remaining == 0 {
            return None;
        }
        self.remaining -= 1;
        let _shared_scope = crate::shared::RowScope::begin();
        let result = T::deserialize_columns(&mut self.cursors).and_then(|value| {
            // The last row must consume every column exactly.
            if self.remaining == 0 && !self.cursors.iter().all(Cursor::at_end) {
                return Err(Error::TrailingBytes);
            }
            Ok(value)
        });
        if result.is_err() {
            self.remaining = 0;
        }
        Some(result)
    }

    /// As with [`Rows`], the lower bound is `0` until a row actually decodes.
    fn size_hint(&self) -> (usize, Option<usize>) {
        (0, usize::try_from(self.remaining).ok())
    }
}

impl<'de, T> FusedIterator for RowsColumns<'de, T> where T: DeserializeColumns<'de> {}

fn key_deserializer(key: &str) -> StrDeserializer<'_, Error> {
    StrDeserializer::new(key)
}

/// Whether the reading type tags any of its fields with one of the writer's
/// positions `"0"`..`"count-1"`, which is how it opts into index matching for a
/// payload the writer recorded positionally.
///
/// serde puts `#[serde(alias = "...")]` values in the field list it hands the
/// format, alongside the real names, so a reader that tags its fields with the
/// tuple positions they came from is visible here:
///
/// ```text
/// enum Action {
///     Apply {
///         #[serde(alias = "1")] y: u32,
///         #[serde(alias = "0")] x: u32,
///     },
/// }
/// ```
///
/// One tag is enough to opt in. carbonite then drives the reader over every
/// position the writer stored and lets serde do the matching, so a position
/// the reader left untagged matches nothing: it is reported as a missing field
/// rather than filled from whatever happened to line up. That is also what
/// lets a reader *drop* a position, by no longer naming it.
///
/// The list interleaves aliases with real names and cannot be split back up
/// per field, which is why this only looks for the tags rather than pairing
/// them with fields itself.
fn positional_names(fields: &[&str], count: usize) -> bool {
    /// `s == n`, rendered as decimal, without allocating.
    fn is_index(s: &str, n: usize) -> bool {
        let mut buf = [0u8; 20];
        let mut len = 0;
        let mut value = n;
        loop {
            len += 1;
            buf[buf.len() - len] = b'0' + (value % 10) as u8;
            value /= 10;
            if value == 0 {
                break;
            }
        }
        s.as_bytes() == &buf[buf.len() - len..]
    }

    (0..count).any(|i| fields.iter().any(|field| is_index(field, i)))
}

/// A positional payload reaching named fields that have not said which
/// position each one replaces.
///
/// Reading it in declaration order would be an evolution step that does not
/// compose: reordering named fields is already a no-op, so `A(f32, f32)` ->
/// `B { x, y }` followed by `B { x, y }` -> `C { y, x }` would make a blob
/// written as `A` decode into `C` with its values swapped, silently. Requiring
/// the positions to be declared makes the correspondence survive any later
/// reordering.
fn needs_positional_names(count: usize) -> Error {
    let aliases = (0..count)
        .map(|i| format!("#[serde(alias = \"{i}\")]"))
        .collect::<Vec<_>>()
        .join(", ");
    Error::SchemaMismatch {
        expected: format!(
            "named fields tagged with the positions they replace ({aliases}), since the writer \
             recorded positions and no names"
        ),
        found: "named fields without them; matching by declaration order would silently change \
                meaning if those fields are ever reordered"
            .to_owned(),
    }
}

/// The mirror image: a named payload reaching a tuple, which fails to compose
/// for the same reason and cannot be rescued, because a tuple has nowhere to
/// put the declaration.
fn named_into_positional() -> Error {
    Error::SchemaMismatch {
        expected: "a reader with named fields, matched by name".to_owned(),
        found: "a tuple, which has nowhere to declare which of the writer's named fields each \
                position holds"
            .to_owned(),
    }
}

/// Reads a positional payload as a map keyed by field index. The counterpart
/// of [`visit_product`] for readers that declared their positions via
/// [`positional_names`];
/// unlike that one it needs no drain, because the visitor is driven over every
/// one of the writer's fields and ignores the ones it does not know.
fn visit_numbered<'s, 'de, V: Visitor<'de>>(
    fields: FieldList<'s>,
    cursors: &mut [Cursor<'de>],
    fast: bool,
    visitor: V,
) -> Result<V::Value> {
    visitor.visit_map(NumberedMap {
        fields,
        cursors,
        index: 0,
        fast,
    })
}

/// Reads a product of fields positionally, then advances past any the visitor
/// did not ask for.
///
/// A reader whose type has fewer fields than the writer's stops pulling early
/// — a shortened tuple, or a struct reading a wider positional payload. The
/// leftover fields still own columns in this row, so they are skipped here;
/// otherwise the row is left partly unread and the blob reports trailing
/// bytes.
fn visit_product<'s, 'de, V: Visitor<'de>>(
    fields: FieldList<'s>,
    cursors: &mut [Cursor<'de>],
    fast: bool,
    visitor: V,
) -> Result<V::Value> {
    // The schemas are identical, so the visitor asks for every field and there
    // is nothing to drain. Hand the sequence over directly and keep the hot
    // path free of the bookkeeping the reconciling one needs.
    if fast {
        return visitor.visit_seq(FieldsSeq {
            fields,
            cursors,
            index: 0,
            fast,
        });
    }

    // Reconciling: keep the sequence afterwards (serde's blanket
    // `SeqAccess for &mut A` lends it out) to see how far the visitor got. A
    // reader with fewer fields than the writer stops early, and the fields it
    // never asked for still have to be skipped.
    let mut seq = FieldsSeq {
        fields,
        cursors: &mut *cursors,
        index: 0,
        fast,
    };
    let value = visitor.visit_seq(&mut seq)?;
    let mut index = seq.index;
    while let Some(node) = fields.get(index) {
        ValueDeserializer::skip(node, cursors)?;
        index += 1;
    }
    Ok(value)
}

/// Reads one value out of `cursors` (exactly the columns of `node`, in layout
/// order) through the schema-driven serde path.
///
/// The cursor slice is addressed relative to its own start, so this can be
/// pointed at a subrange of a larger row — which is how a
/// `#[carbonite(serde)]` field is read from inside the columnar fast path (see
/// [`crate::fallback`]). `fast` selects positional struct decoding, which is
/// correct exactly when `node` came from the reading type itself.
pub(crate) fn deserialize_value<'de, T: Deserialize<'de>>(
    node: &Node,
    cursors: &mut [Cursor<'de>],
    fast: bool,
) -> Result<T> {
    T::deserialize(ValueDeserializer {
        node,
        cursors,
        fast,
    })
}

// ---------------------------------------------------------------------------
// The serde::Deserializer implementation.
// ---------------------------------------------------------------------------

/// Deserializes one value position, driven entirely by the file schema.
struct ValueDeserializer<'s, 'c, 'de> {
    node: &'s Node,
    cursors: &'c mut [Cursor<'de>],
    fast: bool,
}

impl<'s, 'de> ValueDeserializer<'s, '_, 'de> {
    /// Reads whatever the file schema says is next and hands it to the
    /// visitor. Because the schema is in hand, this format is effectively
    /// self-describing at read time.
    fn dispatch<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value> {
        match self.node {
            Node::Primitive(p, col) => {
                let cursor = &mut self.cursors[*col];
                match p {
                    Primitive::Bool => match cursor.byte()? {
                        0 => visitor.visit_bool(false),
                        1 => visitor.visit_bool(true),
                        other => Err(Error::InvalidTag {
                            what: "bool",
                            value: u64::from(other),
                        }),
                    },
                    Primitive::I8 => visitor.visit_i8(i8::from_le_bytes(cursor.fixed()?)),
                    Primitive::I16 => visitor.visit_i16(i16::from_le_bytes(cursor.fixed()?)),
                    Primitive::I32 => visitor.visit_i32(i32::from_le_bytes(cursor.fixed()?)),
                    Primitive::I64 => visitor.visit_i64(i64::from_le_bytes(cursor.fixed()?)),
                    Primitive::I128 => visitor.visit_i128(i128::from_le_bytes(cursor.fixed()?)),
                    Primitive::U8 => visitor.visit_u8(cursor.byte()?),
                    Primitive::U16 => visitor.visit_u16(u16::from_le_bytes(cursor.fixed()?)),
                    Primitive::U32 => visitor.visit_u32(u32::from_le_bytes(cursor.fixed()?)),
                    Primitive::U64 => visitor.visit_u64(u64::from_le_bytes(cursor.fixed()?)),
                    Primitive::U128 => visitor.visit_u128(u128::from_le_bytes(cursor.fixed()?)),
                    Primitive::F32 => visitor.visit_f32(f32::from_le_bytes(cursor.fixed()?)),
                    Primitive::F64 => visitor.visit_f64(f64::from_le_bytes(cursor.fixed()?)),
                    Primitive::Char => {
                        let scalar = u32::from_le_bytes(cursor.fixed()?);
                        let c = char::from_u32(scalar).ok_or(Error::InvalidTag {
                            what: "char",
                            value: u64::from(scalar),
                        })?;
                        visitor.visit_char(c)
                    }
                }
            }
            Node::Str {
                bytes: false,
                len,
                data,
            } => {
                let n = self.cursors[*len].varint_len()?;
                let bytes = self.cursors[*data].take(n)?;
                let s = std::str::from_utf8(bytes).map_err(|_| Error::InvalidUtf8)?;
                visitor.visit_borrowed_str(s)
            }
            Node::Str {
                bytes: true,
                len,
                data,
            } => {
                let n = self.cursors[*len].varint_len()?;
                visitor.visit_borrowed_bytes(self.cursors[*data].take(n)?)
            }
            Node::Unit { .. } => visitor.visit_unit(),
            // A newtype struct occupies exactly its inner value's columns, so
            // the wrapper is invisible on the wire and a reader that is not
            // asking for one simply sees through it. A reader that *is* asking
            // never arrives here — `deserialize_newtype_struct` hands it the
            // wrapper itself.
            Node::Newtype { inner, .. } => ValueDeserializer {
                node: inner,
                cursors: self.cursors,
                fast: self.fast,
            }
            .dispatch(visitor),
            Node::Option { tag, inner } => match self.cursors[*tag].byte()? {
                0 => visitor.visit_none(),
                1 => visitor.visit_some(ValueDeserializer {
                    node: inner,
                    cursors: self.cursors,
                    fast: self.fast,
                }),
                other => Err(Error::InvalidTag {
                    what: "presence",
                    value: u64::from(other),
                }),
            },
            Node::Seq {
                len,
                elem,
                elem_columns,
            } => {
                let claimed = self.cursors[*len].varint()?;
                let remaining =
                    checked_count("sequence length", *elem_columns, claimed, self.cursors)? as u64;
                visitor.visit_seq(ColSeq {
                    node: elem,
                    cursors: self.cursors,
                    remaining,
                    fast: self.fast,
                })
            }
            Node::Tuple { fields, .. } => {
                visit_product(FieldList::Plain(fields), self.cursors, self.fast, visitor)
            }
            Node::Map {
                len,
                key,
                value,
                entry_columns,
            } => {
                let claimed = self.cursors[*len].varint()?;
                let remaining =
                    checked_count("map length", *entry_columns, claimed, self.cursors)? as u64;
                visitor.visit_map(ColMap {
                    key,
                    value,
                    cursors: self.cursors,
                    remaining,
                    awaiting_value: false,
                    fast: self.fast,
                })
            }
            Node::Struct { fields, .. } => {
                if self.fast {
                    visit_product(FieldList::Named(fields), self.cursors, self.fast, visitor)
                } else {
                    visitor.visit_map(StructMap {
                        fields,
                        cursors: self.cursors,
                        index: 0,
                        fast: self.fast,
                    })
                }
            }
            Node::Enum { tag, variants, .. } => {
                let raw = self.cursors[*tag].varint()?;
                let index = usize::try_from(raw)
                    .ok()
                    .filter(|i| *i < variants.len())
                    .ok_or(Error::InvalidTag {
                        what: "enum variant",
                        value: raw,
                    })?;
                let variant = &variants[index];
                visitor.visit_enum(ColEnum {
                    name: &variant.name,
                    kind: &variant.kind,
                    cursors: self.cursors,
                    fast: self.fast,
                })
            }
            // Shared data reached by a non-Shared reader (e.g. a field that
            // used to be plain, or IgnoredAny): first occurrences
            // materialize transparently; repeats cannot (there is no stored
            // value to clone) and error via read_slot.
            Node::Shared { key, inner } => {
                let position = std::ptr::from_ref(&self.cursors[*key]) as usize;
                let k = self.cursors[*key].varint()?;
                match crate::shared::read_slot(position, k)? {
                    crate::shared::ReadSlot::New(_reserved) => {
                        // The reservation intentionally stays unfulfilled:
                        // a plain reader produces no shareable pointer.
                        ValueDeserializer {
                            node: inner,
                            cursors: self.cursors,
                            fast: self.fast,
                        }
                        .dispatch(visitor)
                    }
                    crate::shared::ReadSlot::Repeat(_) => Err(crate::shared::repeat_unavailable()),
                }
            }
        }
    }

    /// This value's fields, if the writer's shape is a product (a unit, newtype
    /// struct, tuple, tuple struct, or struct). Every product shape is a list
    /// of fields in declaration order, which is what lets one replace another
    /// between versions — a unit being the empty list, so a type that stored
    /// nothing can grow fields as long as they have defaults.
    fn product_fields(&self) -> Option<FieldList<'s>> {
        match self.node {
            Node::Unit { .. } => Some(FieldList::Plain(&[])),
            Node::Newtype { inner, .. } => Some(FieldList::Plain(std::slice::from_ref(&**inner))),
            Node::Tuple { fields, .. } => Some(FieldList::Plain(fields)),
            Node::Struct { fields, .. } => Some(FieldList::Named(fields)),
            _ => None,
        }
    }

    /// Reads any product shape positionally, for a reader asking for a tuple,
    /// tuple struct, or newtype struct. A writer that recorded names is
    /// refused; see [`named_into_positional`].
    fn deserialize_positional<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value> {
        match self.product_fields() {
            Some(FieldList::Named(_)) if !self.fast => Err(named_into_positional()),
            Some(fields) => visit_product(fields, self.cursors, self.fast, visitor),
            None => self.dispatch(visitor),
        }
    }

    /// Advances every cursor past one value of `node` without materializing
    /// it. Used for fields the reading type does not know.
    fn skip(node: &Node, cursors: &mut [Cursor<'de>]) -> Result<()> {
        match node {
            Node::Primitive(p, col) => {
                cursors[*col].take(p.width())?;
            }
            Node::Str { len, data, .. } => {
                let n = cursors[*len].varint_len()?;
                cursors[*data].take(n)?;
            }
            Node::Unit { .. } => {}
            Node::Newtype { inner, .. } => {
                Self::skip(inner, cursors)?;
            }
            Node::Option { tag, inner } => match cursors[*tag].byte()? {
                0 => {}
                1 => Self::skip(inner, cursors)?,
                other => {
                    return Err(Error::InvalidTag {
                        what: "presence",
                        value: u64::from(other),
                    });
                }
            },
            Node::Seq {
                len,
                elem,
                elem_columns,
            } => {
                let claimed = cursors[*len].varint()?;
                let n = checked_count("sequence length", *elem_columns, claimed, cursors)?;
                for _ in 0..n {
                    Self::skip(elem, cursors)?;
                }
            }
            Node::Map {
                len,
                key,
                value,
                entry_columns,
            } => {
                let claimed = cursors[*len].varint()?;
                let n = checked_count("map length", *entry_columns, claimed, cursors)?;
                for _ in 0..n {
                    Self::skip(key, cursors)?;
                    Self::skip(value, cursors)?;
                }
            }
            Node::Tuple { fields, .. } => {
                for field in fields {
                    Self::skip(field, cursors)?;
                }
            }
            Node::Struct { fields, .. } => {
                for (_, field) in fields {
                    Self::skip(field, cursors)?;
                }
            }
            Node::Enum { tag, variants, .. } => {
                let raw = cursors[*tag].varint()?;
                let index = usize::try_from(raw)
                    .ok()
                    .filter(|i| *i < variants.len())
                    .ok_or(Error::InvalidTag {
                        what: "enum variant",
                        value: raw,
                    })?;
                Self::skip_variant(&variants[index].kind, cursors)?;
            }
            Node::Shared { key, inner } => {
                let position = std::ptr::from_ref(&cursors[*key]) as usize;
                let k = cursors[*key].varint()?;
                // Only first occurrences carry a payload to skip.
                if crate::shared::read_skip(position, k)? {
                    Self::skip(inner, cursors)?;
                }
            }
        }
        Ok(())
    }

    fn skip_variant(kind: &VariantKind, cursors: &mut [Cursor<'de>]) -> Result<()> {
        match kind {
            VariantKind::Unit => Ok(()),
            VariantKind::Newtype(inner) => Self::skip(inner, cursors),
            VariantKind::Tuple(fields) => {
                for field in fields {
                    Self::skip(field, cursors)?;
                }
                Ok(())
            }
            VariantKind::Struct(fields) => {
                for (_, field) in fields {
                    Self::skip(field, cursors)?;
                }
                Ok(())
            }
        }
    }

    /// Runs the shared-value protocol for a `carbonite::Shared`/`SharedArc`
    /// wrapper (recognized by its sentinel newtype name).
    fn deserialize_shared_wrapper<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value> {
        match self.node {
            Node::Shared { key, inner } => {
                let position = std::ptr::from_ref(&self.cursors[*key]) as usize;
                let k = self.cursors[*key].varint()?;
                match crate::shared::read_slot(position, k)? {
                    crate::shared::ReadSlot::Repeat(erased) => {
                        // Hand the existing pointer to the wrapper via the
                        // delivery slot; the guard deserializer ensures a
                        // non-cooperating visitor fails instead of
                        // desynchronizing the cursors.
                        crate::shared::deliver(erased);
                        let result = visitor.visit_newtype_struct(RepeatGuard);
                        crate::shared::clear_delivered();
                        result
                    }
                    crate::shared::ReadSlot::New(reserved) => {
                        let value = visitor.visit_newtype_struct(ValueDeserializer {
                            node: inner,
                            cursors: self.cursors,
                            fast: self.fast,
                        })?;
                        let erased = crate::shared::take_published().ok_or_else(|| {
                            Error::Message(
                                "carbonite::Shared protocol violation: no shared value was \
                                 published"
                                    .to_owned(),
                            )
                        })?;
                        crate::shared::fulfill(position, reserved, erased);
                        Ok(value)
                    }
                }
            }
            // A shared wrapper reading pre-Shared (plain) data: every
            // occurrence materializes a fresh value.
            _ => visitor.visit_newtype_struct(self),
        }
    }
}

/// Stands in for the payload of a repeated shared value: the data was
/// consumed at the first occurrence, so any attempt to actually read is an
/// error (it means the visitor was not a `carbonite::Shared` wrapper).
struct RepeatGuard;

impl<'de> de::Deserializer<'de> for RepeatGuard {
    type Error = Error;

    fn deserialize_any<V: Visitor<'de>>(self, _visitor: V) -> Result<V::Value> {
        Err(crate::shared::repeat_unavailable())
    }

    serde::forward_to_deserialize_any! {
        bool i8 i16 i32 i64 i128 u8 u16 u32 u64 u128 f32 f64 char str string
        bytes byte_buf option unit unit_struct newtype_struct seq tuple
        tuple_struct map struct enum identifier ignored_any
    }
}

impl<'de> de::Deserializer<'de> for ValueDeserializer<'_, '_, 'de> {
    type Error = Error;

    fn deserialize_any<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value> {
        self.dispatch(visitor)
    }

    fn deserialize_option<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value> {
        if matches!(self.node, Node::Option { .. }) {
            self.dispatch(visitor)
        } else {
            // The file has a bare value where the type now expects an Option:
            // treat it as present, mirroring JSON's non-null semantics. This
            // makes wrapping a field in Option a compatible change.
            visitor.visit_some(self)
        }
    }

    fn deserialize_ignored_any<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value> {
        Self::skip(self.node, self.cursors)?;
        visitor.visit_unit()
    }

    fn deserialize_newtype_struct<V: Visitor<'de>>(
        self,
        name: &'static str,
        visitor: V,
    ) -> Result<V::Value> {
        if name == crate::shared::SHARED_TOKEN {
            return self.deserialize_shared_wrapper(visitor);
        }
        match self.node {
            // Same shape: unwrap one layer and hand the inside to the reader's
            // own newtype visitor.
            Node::Newtype { inner, .. } => visitor.visit_newtype_struct(ValueDeserializer {
                node: inner,
                cursors: self.cursors,
                fast: self.fast,
            }),
            // A product of fields is read positionally, so a one-field writer
            // lands in the wrapper and any other arity is reported by serde.
            Node::Struct { .. } | Node::Tuple { .. } | Node::Unit { .. } => {
                self.deserialize_positional(visitor)
            }
            // Anything else is a plain value the reader has since wrapped: the
            // wrapper occupies no columns of its own, so the value goes
            // straight into it. This is what makes `u32` -> `Layer(u32)` a
            // compatible change, and — composed with the positional rule —
            // what lets any field become a named struct.
            _ => visitor.visit_newtype_struct(self),
        }
    }

    /// The reader wants named fields. A writer that had them too goes through
    /// `dispatch` (name matching); a positional writer is reconciled by
    /// [`positional_names`] — by index if the reader declared the positions,
    /// otherwise in order.
    fn deserialize_struct<V: Visitor<'de>>(
        self,
        _name: &'static str,
        fields: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value> {
        // The writer recorded names too — far and away the common case, so it
        // is handled here rather than round-tripping through `dispatch`.
        if let Node::Struct {
            fields: written, ..
        } = self.node
        {
            return if self.fast {
                visit_product(FieldList::Named(written), self.cursors, self.fast, visitor)
            } else {
                visitor.visit_map(StructMap {
                    fields: written,
                    cursors: self.cursors,
                    index: 0,
                    fast: self.fast,
                })
            };
        }
        match self.product_fields() {
            // Not a product at all: a plain value the reader has since
            // promoted to a struct of its own. It is a one-field product whose
            // single position is the value itself, so `u32` -> `Layer(u32)` ->
            // `Layer { id }` composes into `u32` -> `Layer { id }` directly.
            // Only offered when the reader claims position 0; otherwise this is
            // an ordinary type mismatch and serde should say so.
            None if positional_names(fields, 1) => visit_numbered(
                FieldList::Plain(std::slice::from_ref(self.node)),
                self.cursors,
                self.fast,
                visitor,
            ),
            None => self.dispatch(visitor),
            // No positions to declare: the writer stored nothing, so every
            // field the reader has now is missing and falls to its default.
            Some(positional) if positional.len() == 0 => {
                visit_numbered(positional, self.cursors, self.fast, visitor)
            }
            Some(positional) => {
                if positional_names(fields, positional.len()) {
                    visit_numbered(positional, self.cursors, self.fast, visitor)
                } else {
                    Err(needs_positional_names(positional.len()))
                }
            }
        }
    }

    /// The reader stores nothing here any more. Whatever the writer put in
    /// this position is skipped, the same as a field the reader has dropped.
    fn deserialize_unit<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value> {
        Self::skip(self.node, self.cursors)?;
        visitor.visit_unit()
    }

    fn deserialize_unit_struct<V: Visitor<'de>>(
        self,
        _name: &'static str,
        visitor: V,
    ) -> Result<V::Value> {
        self.deserialize_unit(visitor)
    }

    /// The reader wants positional fields. A writer that recorded names still
    /// has them in declaration order, so hand them over positionally.
    fn deserialize_tuple<V: Visitor<'de>>(self, _len: usize, visitor: V) -> Result<V::Value> {
        self.deserialize_positional(visitor)
    }

    fn deserialize_tuple_struct<V: Visitor<'de>>(
        self,
        _name: &'static str,
        _len: usize,
        visitor: V,
    ) -> Result<V::Value> {
        self.deserialize_positional(visitor)
    }

    serde::forward_to_deserialize_any! {
        bool i8 i16 i32 i64 i128 u8 u16 u32 u64 u128 f32 f64 char str string
        bytes byte_buf seq map enum identifier
    }

    fn is_human_readable(&self) -> bool {
        false
    }
}

// ---------------------------------------------------------------------------
// Access objects.
// ---------------------------------------------------------------------------

/// Sequence elements, length-driven.
struct ColSeq<'s, 'c, 'de> {
    node: &'s Node,
    cursors: &'c mut [Cursor<'de>],
    remaining: u64,
    fast: bool,
}

impl<'de> SeqAccess<'de> for ColSeq<'_, '_, 'de> {
    type Error = Error;

    fn next_element_seed<S: DeserializeSeed<'de>>(&mut self, seed: S) -> Result<Option<S::Value>> {
        if self.remaining == 0 {
            return Ok(None);
        }
        self.remaining -= 1;
        seed.deserialize(ValueDeserializer {
            node: self.node,
            cursors: self.cursors,
            fast: self.fast,
        })
        .map(Some)
    }

    fn size_hint(&self) -> Option<usize> {
        usize::try_from(self.remaining).ok()
    }
}

/// One product's fields, with or without the names the writer recorded.
#[derive(Clone, Copy)]
enum FieldList<'s> {
    Plain(&'s [Node]),
    Named(&'s [(String, Node)]),
}

impl<'s> FieldList<'s> {
    fn get(&self, index: usize) -> Option<&'s Node> {
        match self {
            FieldList::Plain(fields) => fields.get(index),
            FieldList::Named(fields) => fields.get(index).map(|(_, field)| field),
        }
    }

    fn len(&self) -> usize {
        match self {
            FieldList::Plain(fields) => fields.len(),
            FieldList::Named(fields) => fields.len(),
        }
    }
}

/// Positional fields: tuples, tuple structs/variants, and fast-path structs.
struct FieldsSeq<'s, 'c, 'de> {
    fields: FieldList<'s>,
    cursors: &'c mut [Cursor<'de>],
    index: usize,
    fast: bool,
}

impl<'de> SeqAccess<'de> for FieldsSeq<'_, '_, 'de> {
    type Error = Error;

    fn next_element_seed<S: DeserializeSeed<'de>>(&mut self, seed: S) -> Result<Option<S::Value>> {
        let Some(node) = self.fields.get(self.index) else {
            return Ok(None);
        };
        self.index += 1;
        seed.deserialize(ValueDeserializer {
            node,
            cursors: self.cursors,
            fast: self.fast,
        })
        .map(Some)
    }

    fn size_hint(&self) -> Option<usize> {
        Some(self.fields.len() - self.index)
    }
}

/// Name-matched struct fields (the evolution path): keys come from the file
/// schema; the visitor matches them against the type's fields.
struct StructMap<'s, 'c, 'de> {
    fields: &'s [(String, Node)],
    cursors: &'c mut [Cursor<'de>],
    index: usize,
    fast: bool,
}

impl<'de> MapAccess<'de> for StructMap<'_, '_, 'de> {
    type Error = Error;

    fn next_key_seed<S: DeserializeSeed<'de>>(&mut self, seed: S) -> Result<Option<S::Value>> {
        let Some((name, _)) = self.fields.get(self.index) else {
            return Ok(None);
        };
        seed.deserialize(key_deserializer(name)).map(Some)
    }

    fn next_value_seed<S: DeserializeSeed<'de>>(&mut self, seed: S) -> Result<S::Value> {
        let (_, node) = self
            .fields
            .get(self.index)
            .expect("next_value_seed called after next_key_seed returned a key");
        self.index += 1;
        seed.deserialize(ValueDeserializer {
            node,
            cursors: self.cursors,
            fast: self.fast,
        })
    }

    fn size_hint(&self) -> Option<usize> {
        Some(self.fields.len() - self.index)
    }
}

/// A positional payload presented as a map keyed by field index. See
/// [`positional_names`].
struct NumberedMap<'s, 'c, 'de> {
    fields: FieldList<'s>,
    cursors: &'c mut [Cursor<'de>],
    index: usize,
    fast: bool,
}

impl<'de> MapAccess<'de> for NumberedMap<'_, '_, 'de> {
    type Error = Error;

    fn next_key_seed<S: DeserializeSeed<'de>>(&mut self, seed: S) -> Result<Option<S::Value>> {
        if self.index >= self.fields.len() {
            return Ok(None);
        }
        let key = self.index.to_string();
        seed.deserialize(key_deserializer(&key)).map(Some)
    }

    fn next_value_seed<S: DeserializeSeed<'de>>(&mut self, seed: S) -> Result<S::Value> {
        let node = self
            .fields
            .get(self.index)
            .expect("next_value_seed called after next_key_seed returned a key");
        self.index += 1;
        seed.deserialize(ValueDeserializer {
            node,
            cursors: self.cursors,
            fast: self.fast,
        })
    }

    fn size_hint(&self) -> Option<usize> {
        Some(self.fields.len() - self.index)
    }
}

/// Map entries, length-driven.
struct ColMap<'s, 'c, 'de> {
    key: &'s Node,
    value: &'s Node,
    cursors: &'c mut [Cursor<'de>],
    remaining: u64,
    awaiting_value: bool,
    fast: bool,
}

impl<'de> MapAccess<'de> for ColMap<'_, '_, 'de> {
    type Error = Error;

    fn next_key_seed<S: DeserializeSeed<'de>>(&mut self, seed: S) -> Result<Option<S::Value>> {
        if self.remaining == 0 {
            return Ok(None);
        }
        self.remaining -= 1;
        self.awaiting_value = true;
        seed.deserialize(ValueDeserializer {
            node: self.key,
            cursors: self.cursors,
            fast: self.fast,
        })
        .map(Some)
    }

    fn next_value_seed<S: DeserializeSeed<'de>>(&mut self, seed: S) -> Result<S::Value> {
        // A misbehaving visitor must get an error, not silently desynced
        // cursors — the write side reports the same misuse the same way.
        if !self.awaiting_value {
            return Err(Error::Message(
                "map value requested without a key".to_owned(),
            ));
        }
        self.awaiting_value = false;
        seed.deserialize(ValueDeserializer {
            node: self.value,
            cursors: self.cursors,
            fast: self.fast,
        })
    }

    fn size_hint(&self) -> Option<usize> {
        usize::try_from(self.remaining).ok()
    }
}

/// One enum value: hands the visitor the file's variant name, then decodes
/// the payload per the file's shape for that variant.
struct ColEnum<'s, 'c, 'de> {
    name: &'s str,
    kind: &'s VariantKind,
    cursors: &'c mut [Cursor<'de>],
    fast: bool,
}

impl<'de> EnumAccess<'de> for ColEnum<'_, '_, 'de> {
    type Error = Error;
    type Variant = Self;

    fn variant_seed<S: DeserializeSeed<'de>>(self, seed: S) -> Result<(S::Value, Self::Variant)> {
        let value = seed.deserialize(key_deserializer(self.name))?;
        Ok((value, self))
    }
}

impl<'de> VariantAccess<'de> for ColEnum<'_, '_, 'de> {
    type Error = Error;

    /// The reader's variant carries nothing any more, so whatever payload the
    /// writer stored is skipped — the same as a field the reader has dropped.
    fn unit_variant(self) -> Result<()> {
        ValueDeserializer::skip_variant(self.kind, self.cursors)
    }

    fn newtype_variant_seed<S: DeserializeSeed<'de>>(self, seed: S) -> Result<S::Value> {
        // A newtype variant is a one-field product, so any positional writer
        // shape holding exactly one field reads into it.
        match self.payload_fields() {
            FieldList::Named(_) if !self.fast => Err(named_into_positional()),
            fields if fields.len() == 1 => {
                let node = fields.get(0).expect("len checked");
                seed.deserialize(ValueDeserializer {
                    node,
                    fast: self.fast,
                    cursors: self.cursors,
                })
            }
            _ => Err(Error::SchemaMismatch {
                expected: variant_shape_name(self.kind).to_owned(),
                found: format!("newtype variant `{}`", self.name),
            }),
        }
    }

    fn tuple_variant<V: Visitor<'de>>(self, _len: usize, visitor: V) -> Result<V::Value> {
        match self.payload_fields() {
            FieldList::Named(_) if !self.fast => Err(named_into_positional()),
            fields => visit_product(fields, self.cursors, self.fast, visitor),
        }
    }

    fn struct_variant<V: Visitor<'de>>(
        self,
        fields: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value> {
        match self.kind {
            // The writer had named fields too: match by name, which is what
            // makes reordering and `#[serde(alias)]` work.
            VariantKind::Struct(wfields) if !self.fast => visitor.visit_map(StructMap {
                fields: wfields,
                cursors: self.cursors,
                index: 0,
                fast: self.fast,
            }),
            // Otherwise the writer's payload was positional (or this is the
            // fast path, where positions already agree); see
            // [`positional_names`].
            _ => match self.payload_fields() {
                // On the fast path the shapes are identical, so positions
                // already agree and no declaration is needed. An empty payload
                // has no positions to declare either.
                payload if self.fast => visit_product(payload, self.cursors, self.fast, visitor),
                payload if payload.len() == 0 => {
                    visit_numbered(payload, self.cursors, self.fast, visitor)
                }
                payload => {
                    if positional_names(fields, payload.len()) {
                        visit_numbered(payload, self.cursors, self.fast, visitor)
                    } else {
                        Err(needs_positional_names(payload.len()))
                    }
                }
            },
        }
    }
}

impl<'s> ColEnum<'s, '_, '_> {
    /// The variant's payload as a positional field list, whatever shape the
    /// writer used. Every non-unit variant shape is a product of fields, so
    /// this is what lets a variant change shape between versions: a newtype,
    /// tuple, or struct payload all present the same way to a reader that
    /// asks for a different one. A unit variant is the empty product, so it
    /// can grow fields on the same terms a unit struct can: they are all
    /// missing, and each falls to its `#[serde(default)]`.
    fn payload_fields(&self) -> FieldList<'s> {
        match self.kind {
            VariantKind::Unit => FieldList::Plain(&[]),
            VariantKind::Newtype(inner) => FieldList::Plain(std::slice::from_ref(&**inner)),
            VariantKind::Tuple(fields) => FieldList::Plain(fields),
            VariantKind::Struct(fields) => FieldList::Named(fields),
        }
    }
}

fn variant_shape_name(kind: &VariantKind) -> &'static str {
    match kind {
        VariantKind::Unit => "unit variant",
        VariantKind::Newtype(_) => "newtype variant",
        VariantKind::Tuple(_) => "tuple variant",
        VariantKind::Struct(_) => "struct variant",
    }
}

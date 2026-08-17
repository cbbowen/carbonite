//! Shared-structure preservation: [`Shared`] and [`SharedArc`].
//!
//! serde's data model is a tree with no notion of object identity, so stock
//! `Rc`/`Arc` fields serialize their pointee once *per handle*. These
//! wrappers restore identity: within one row, each unique object is written
//! once — a varint **key column** (one entry per occurrence) plus the
//! payload columns as a dense **dictionary** (one entry per unique object,
//! in first-occurrence order). Keys are dense indices assigned at write
//! time; addresses are only ephemeral write-side identity and never reach
//! the file. Deserialization reconstructs the sharing with `Rc`/`Arc`
//! clones, so `Shared::ptr_eq` holds again after a round trip.
//!
//! The serde-path integration uses a sentinel newtype-struct name (the same
//! technique as `serde_json::RawValue`): carbonite's serializer,
//! deserializer, and tracer recognize the token and run the dictionary
//! protocol; **every other format sees a transparent newtype** and
//! serializes the value inline — exactly serde's stock duplicating
//! behavior, with no addresses or keys leaking into foreign output.
//!
//! Scope and limits:
//! - Sharing is deduplicated **per row** (per [`Batch`](crate::Batch) push).
//! - Cyclic values cannot be reconstructed generically and produce a clean
//!   error on read (and recursive schemas are rejected at trace time
//!   anyway); `Weak` backlinks are not serializable, which conveniently
//!   makes most real strong topologies DAGs.
//! - Reconstructing sharing requires `T: 'static` (plus `Send + Sync` for
//!   [`SharedArc`]) because the read registry is type-erased.

use std::any::Any;
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::fmt;
use std::hash::{Hash, Hasher};
use std::marker::PhantomData;
use std::ops::Deref;
use std::rc::Rc;
use std::sync::Arc;

use serde::de::Visitor;
use serde::{Deserialize, Serialize};

use crate::columnar::{ColumnCursor, DeserializeColumns, SerializeColumns, write_varint};
use crate::error::{Error, Result};
use crate::schema::SchemaNode;
use crate::static_schema::StaticSchema;

/// The sentinel newtype-struct name that lets carbonite's format recognize
/// shared wrappers through serde's data model.
pub(crate) const SHARED_TOKEN: &str = "$carbonite::Shared";

// ---------------------------------------------------------------------------
// The wrappers.
// ---------------------------------------------------------------------------

macro_rules! shared_wrapper {
    ($(#[$doc:meta])* $name:ident, $ptr:ident, $ptr_path:path) => {
        $(#[$doc])*
        pub struct $name<T>($ptr<T>);

        impl<T> $name<T> {
            /// Wraps a fresh value.
            #[must_use]
            pub fn new(value: T) -> Self {
                $name($ptr::new(value))
            }

            /// Wraps an existing pointer, preserving its identity.
            #[must_use]
            pub fn from_ptr(ptr: $ptr<T>) -> Self {
                $name(ptr)
            }

            /// Unwraps into the underlying pointer.
            #[must_use]
            pub fn into_ptr(self) -> $ptr<T> {
                self.0
            }

            /// Whether two handles point at the same object.
            #[must_use]
            pub fn ptr_eq(a: &Self, b: &Self) -> bool {
                $ptr::ptr_eq(&a.0, &b.0)
            }
        }

        impl<T> Deref for $name<T> {
            type Target = T;

            fn deref(&self) -> &T {
                &self.0
            }
        }

        impl<T> AsRef<T> for $name<T> {
            fn as_ref(&self) -> &T {
                &self.0
            }
        }

        impl<T> Clone for $name<T> {
            fn clone(&self) -> Self {
                $name($ptr::clone(&self.0))
            }
        }

        impl<T: fmt::Debug> fmt::Debug for $name<T> {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                fmt::Debug::fmt(&**self, f)
            }
        }

        impl<T: PartialEq> PartialEq for $name<T> {
            /// Compares by value, like `Rc`/`Arc`.
            fn eq(&self, other: &Self) -> bool {
                **self == **other
            }
        }

        impl<T: Eq> Eq for $name<T> {}

        impl<T: Hash> Hash for $name<T> {
            fn hash<H: Hasher>(&self, state: &mut H) {
                (**self).hash(state);
            }
        }

        impl<T: Default> Default for $name<T> {
            fn default() -> Self {
                Self::new(T::default())
            }
        }

        impl<T> From<T> for $name<T> {
            fn from(value: T) -> Self {
                Self::new(value)
            }
        }

        impl<T> From<$ptr<T>> for $name<T> {
            fn from(ptr: $ptr<T>) -> Self {
                Self::from_ptr(ptr)
            }
        }

        impl<T: Serialize> Serialize for $name<T> {
            fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                // Identity crosses the serde boundary via a thread-local
                // stash, consumed immediately by carbonite's serializer when
                // it sees the token; foreign formats never look.
                stash_addr($ptr::as_ptr(&self.0).cast::<()>() as usize);
                serializer.serialize_newtype_struct(SHARED_TOKEN, &*self.0)
            }
        }

        impl<T: StaticSchema> StaticSchema for $name<T> {
            fn schema_node() -> SchemaNode {
                SchemaNode::Shared(Box::new(T::schema_node()))
            }
            #[inline]
            fn columns() -> usize {
                1 + T::columns()
            }
        }
    };
}

shared_wrapper! {
    /// A shared, per-row-deduplicated value backed by [`Rc`].
    ///
    /// Serializes each unique object once per row; deserialization
    /// reconstructs the sharing ([`Shared::ptr_eq`] holds after a round
    /// trip). In non-carbonite serde formats the wrapper is invisible and
    /// duplicates inline. See the [crate-level notes](crate#shared-values).
    Shared, Rc, std::rc::Rc
}

shared_wrapper! {
    /// The [`Arc`]-backed sibling of [`Shared`]. The two share a wire
    /// representation, so a blob written with one can be read with the
    /// other.
    SharedArc, Arc, std::sync::Arc
}

impl<'de, T: Deserialize<'de> + 'static> Deserialize<'de> for Shared<T> {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct SharedVisitor<T>(PhantomData<T>);

        impl<'de, T: Deserialize<'de> + 'static> Visitor<'de> for SharedVisitor<T> {
            type Value = Shared<T>;

            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("a shared value")
            }

            fn visit_newtype_struct<D: serde::Deserializer<'de>>(
                self,
                deserializer: D,
            ) -> Result<Shared<T>, D::Error> {
                if let Some(erased) = take_delivered() {
                    return erased
                        .try_rc::<T>()
                        .map(Shared)
                        .map_err(serde::de::Error::custom);
                }
                let value = Rc::new(T::deserialize(deserializer)?);
                publish(Erased::Rc(value.clone()));
                Ok(Shared(value))
            }
        }

        deserializer.deserialize_newtype_struct(SHARED_TOKEN, SharedVisitor(PhantomData))
    }
}

impl<'de, T: Deserialize<'de> + Send + Sync + 'static> Deserialize<'de> for SharedArc<T> {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct SharedArcVisitor<T>(PhantomData<T>);

        impl<'de, T: Deserialize<'de> + Send + Sync + 'static> Visitor<'de> for SharedArcVisitor<T> {
            type Value = SharedArc<T>;

            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("a shared value")
            }

            fn visit_newtype_struct<D: serde::Deserializer<'de>>(
                self,
                deserializer: D,
            ) -> Result<SharedArc<T>, D::Error> {
                if let Some(erased) = take_delivered() {
                    return erased
                        .try_arc::<T>()
                        .map(SharedArc)
                        .map_err(serde::de::Error::custom);
                }
                let value = Arc::new(T::deserialize(deserializer)?);
                publish(Erased::Arc(value.clone()));
                Ok(SharedArc(value))
            }
        }

        deserializer.deserialize_newtype_struct(SHARED_TOKEN, SharedArcVisitor(PhantomData))
    }
}

// ---------------------------------------------------------------------------
// Columnar fast path.
// ---------------------------------------------------------------------------

fn require_active() -> Result<()> {
    if active() {
        Ok(())
    } else {
        Err(Error::Message(
            "carbonite shared values must be (de)serialized through carbonite entry points"
                .to_owned(),
        ))
    }
}

impl<T: SerializeColumns> SerializeColumns for Shared<T> {
    fn serialize_columns(&self, columns: &mut [Vec<u8>]) -> Result<()> {
        require_active()?;
        // The key column's Vec address identifies this schema position for
        // the duration of one row (the column buffers never move mid-push).
        let position = columns.as_ptr() as usize;
        let addr = Rc::as_ptr(&self.0).cast::<()>() as usize;
        match write_key(position, addr) {
            WriteKey::Existing(key) => {
                write_varint(&mut columns[0], key);
                Ok(())
            }
            WriteKey::New(key) => {
                write_varint(&mut columns[0], key);
                self.0.serialize_columns(&mut columns[1..])
            }
        }
    }
}

impl<'de, T: DeserializeColumns<'de> + 'static> DeserializeColumns<'de> for Shared<T> {
    fn deserialize_columns(cursors: &mut [ColumnCursor<'de>]) -> Result<Self> {
        require_active()?;
        let position = cursors.as_ptr() as usize;
        let key = cursors[0].varint()?;
        match read_slot(position, key)? {
            ReadSlot::Repeat(erased) => erased
                .try_rc::<T>()
                .map(Shared)
                .map_err(|msg| Error::Message(msg.to_owned())),
            ReadSlot::New(index) => {
                let value = Rc::new(T::deserialize_columns(&mut cursors[1..])?);
                fulfill(position, index, Erased::Rc(value.clone()));
                Ok(Shared(value))
            }
        }
    }
}

impl<T: SerializeColumns> SerializeColumns for SharedArc<T> {
    fn serialize_columns(&self, columns: &mut [Vec<u8>]) -> Result<()> {
        require_active()?;
        let position = columns.as_ptr() as usize;
        let addr = Arc::as_ptr(&self.0).cast::<()>() as usize;
        match write_key(position, addr) {
            WriteKey::Existing(key) => {
                write_varint(&mut columns[0], key);
                Ok(())
            }
            WriteKey::New(key) => {
                write_varint(&mut columns[0], key);
                self.0.serialize_columns(&mut columns[1..])
            }
        }
    }
}

impl<'de, T: DeserializeColumns<'de> + Send + Sync + 'static> DeserializeColumns<'de>
    for SharedArc<T>
{
    fn deserialize_columns(cursors: &mut [ColumnCursor<'de>]) -> Result<Self> {
        require_active()?;
        let position = cursors.as_ptr() as usize;
        let key = cursors[0].varint()?;
        match read_slot(position, key)? {
            ReadSlot::Repeat(erased) => erased
                .try_arc::<T>()
                .map(SharedArc)
                .map_err(|msg| Error::Message(msg.to_owned())),
            ReadSlot::New(index) => {
                let value = Arc::new(T::deserialize_columns(&mut cursors[1..])?);
                fulfill(position, index, Erased::Arc(value.clone()));
                Ok(SharedArc(value))
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Thread-local registries and handoff slots.
//
// The registries hold per-position dictionaries for the row being processed;
// the slots pass identity/values across the serde trait boundary between the
// wrappers and carbonite's format. `RowScope` (entered at every carbonite
// row boundary) saves, clears, and restores everything, so rows are
// independent and reentrant use (a nested carbonite call inside a
// Serialize/Deserialize impl) is safe.
// ---------------------------------------------------------------------------

/// A type-erased shared pointer held by the read registry.
#[derive(Clone)]
pub(crate) enum Erased {
    Rc(Rc<dyn Any>),
    Arc(Arc<dyn Any + Send + Sync>),
}

impl Erased {
    fn try_rc<T: 'static>(self) -> Result<Rc<T>, &'static str> {
        match self {
            Erased::Rc(rc) => rc
                .downcast::<T>()
                .map_err(|_| "carbonite: shared value type mismatch"),
            // Rc and Arc wrappers must not be mixed at one schema position:
            // converting would allocate a new pointer and break identity.
            Erased::Arc(_) => {
                Err("carbonite: shared value was registered as SharedArc but requested as Shared")
            }
        }
    }

    fn try_arc<T: Send + Sync + 'static>(self) -> Result<Arc<T>, &'static str> {
        match self {
            Erased::Arc(arc) => arc
                .downcast::<T>()
                .map_err(|_| "carbonite: shared value type mismatch"),
            Erased::Rc(_) => {
                Err("carbonite: shared value was registered as Shared but requested as SharedArc")
            }
        }
    }
}

#[derive(Default)]
struct WriteState {
    /// (position, object address) → assigned key.
    keys: HashMap<(usize, usize), u64>,
    /// position → next key.
    counts: HashMap<usize, u64>,
}

#[derive(Default)]
struct ReadState {
    /// position → dictionary entries in first-occurrence order. `None`
    /// marks an entry that is still being read (a cycle) or that was
    /// materialized by a non-`Shared` reader and so cannot be repeated.
    registries: HashMap<usize, Vec<Option<Erased>>>,
}

thread_local! {
    static ACTIVE: Cell<u32> = const { Cell::new(0) };
    static WRITE: RefCell<WriteState> = RefCell::new(WriteState::default());
    static READ: RefCell<ReadState> = RefCell::new(ReadState::default());
    static STASHED_ADDR: Cell<Option<usize>> = const { Cell::new(None) };
    static PUBLISHED: RefCell<Option<Erased>> = const { RefCell::new(None) };
    static DELIVERED: RefCell<Option<Erased>> = const { RefCell::new(None) };
}

fn active() -> bool {
    ACTIVE.with(|depth| depth.get() > 0)
}

fn stash_addr(addr: usize) {
    if active() {
        STASHED_ADDR.set(Some(addr));
    }
}

pub(crate) fn take_stashed_addr() -> Option<usize> {
    STASHED_ADDR.take()
}

fn publish(erased: Erased) {
    if active() {
        PUBLISHED.set(Some(erased));
    }
}

pub(crate) fn take_published() -> Option<Erased> {
    PUBLISHED.take()
}

pub(crate) fn deliver(erased: Erased) {
    DELIVERED.set(Some(erased));
}

fn take_delivered() -> Option<Erased> {
    DELIVERED.take()
}

pub(crate) fn clear_delivered() {
    DELIVERED.take();
}

pub(crate) enum WriteKey {
    New(u64),
    Existing(u64),
}

pub(crate) fn write_key(position: usize, addr: usize) -> WriteKey {
    WRITE.with_borrow_mut(|state| {
        if let Some(&key) = state.keys.get(&(position, addr)) {
            WriteKey::Existing(key)
        } else {
            let next = state.counts.entry(position).or_default();
            let key = *next;
            *next += 1;
            state.keys.insert((position, addr), key);
            WriteKey::New(key)
        }
    })
}

pub(crate) enum ReadSlot {
    /// An already-materialized dictionary entry.
    Repeat(Erased),
    /// A new entry; the payload follows in the dictionary columns. The
    /// reserved index must be [`fulfill`]ed once the value exists.
    New(usize),
}

pub(crate) fn read_slot(position: usize, key: u64) -> Result<ReadSlot> {
    READ.with_borrow_mut(|state| {
        let registry = state.registries.entry(position).or_default();
        let len = registry.len() as u64;
        if key < len {
            match &registry[usize::try_from(key).expect("key < len fits usize")] {
                Some(erased) => Ok(ReadSlot::Repeat(erased.clone())),
                None => Err(repeat_unavailable()),
            }
        } else if key == len {
            registry.push(None);
            Ok(ReadSlot::New(registry.len() - 1))
        } else {
            Err(Error::InvalidTag {
                what: "shared key",
                value: key,
            })
        }
    })
}

/// Like [`read_slot`] but for skipping: repeats are fine (nothing to
/// materialize). Returns whether the entry is new (payload must be skipped).
pub(crate) fn read_skip(position: usize, key: u64) -> Result<bool> {
    READ.with_borrow_mut(|state| {
        let registry = state.registries.entry(position).or_default();
        let len = registry.len() as u64;
        if key < len {
            Ok(false)
        } else if key == len {
            registry.push(None);
            Ok(true)
        } else {
            Err(Error::InvalidTag {
                what: "shared key",
                value: key,
            })
        }
    })
}

pub(crate) fn fulfill(position: usize, index: usize, erased: Erased) {
    READ.with_borrow_mut(|state| {
        if let Some(slot) = state
            .registries
            .get_mut(&position)
            .and_then(|registry| registry.get_mut(index))
        {
            *slot = Some(erased);
        }
    });
}

pub(crate) fn repeat_unavailable() -> Error {
    Error::Message(
        "a repeated shared value cannot be materialized here; the reading field must use \
         carbonite::Shared/SharedArc (or the value is cyclic)"
            .to_owned(),
    )
}

/// RAII scope entered at every carbonite row boundary: activates the shared
/// machinery, gives the row fresh registries, and restores the previous
/// state on drop (making nested carbonite calls safe).
pub(crate) struct RowScope {
    saved_write: WriteState,
    saved_read: ReadState,
    saved_addr: Option<usize>,
    saved_published: Option<Erased>,
    saved_delivered: Option<Erased>,
}

impl RowScope {
    pub(crate) fn begin() -> Self {
        ACTIVE.with(|depth| depth.set(depth.get() + 1));
        RowScope {
            saved_write: WRITE.take(),
            saved_read: READ.take(),
            saved_addr: STASHED_ADDR.take(),
            saved_published: PUBLISHED.take(),
            saved_delivered: DELIVERED.take(),
        }
    }
}

impl Drop for RowScope {
    fn drop(&mut self) {
        WRITE.set(std::mem::take(&mut self.saved_write));
        READ.set(std::mem::take(&mut self.saved_read));
        STASHED_ADDR.set(self.saved_addr.take());
        PUBLISHED.set(self.saved_published.take());
        DELIVERED.set(self.saved_delivered.take());
        ACTIVE.with(|depth| depth.set(depth.get() - 1));
    }
}

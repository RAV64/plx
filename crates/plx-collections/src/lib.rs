//! plx's collections.
//!
//! - [`Interner`] stores each distinct value once and hands out dense `u32`
//!   ids below its `LIMIT`, a type parameter, so each table's capacity is part
//!   of its type and checked before storing. Removed values free their id for
//!   reuse, so a table holds only what is live.
//! - [`SliceInterner`] does the same for variable-length slices, packed into
//!   one arena (e.g. multi-codepoint grapheme clusters). The arena compacts
//!   itself once removed slices take more room than live ones; ids survive.
//!
//! - [`IdMap`] maps monotonic ids (panes, clients) to values in a sorted
//!   `Vec`: new ids append, lookups are binary searches, iteration is in id
//!   order (deterministic), and memory is minimal for plx's small sets.
//!
//! The interners have an ahash index into their storage, so values are never duplicated
//! as keys, and the index is never iterated: only ids are observable, which
//! keeps output deterministic. Growth allocates (amortised); lookups of known
//! values never do.

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

use alloc::vec::Vec;
use core::hash::Hash;

use ahash::RandomState;
use hashbrown::HashTable;
use plx_bug::OrBug as _;

/// Fixed seeds: hashing is reproducible within a build (simulation replays
/// see the same table layout), and nothing ever iterates the index anyway.
const HASHER: RandomState = RandomState::with_seeds(
    0x243f_6a88_85a3_08d3,
    0x1319_8a2e_0370_7344,
    0xa409_3822_299f_31d0,
    0x082e_fa98_ec4e_6c89,
);

/// The interner is full: it holds `LIMIT` values. Nothing was stored.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Full;

/// Whether interning found the value or (re)defined an id for it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Interned {
    /// The value was already stored under the id.
    Existing,
    /// The id now holds the value: a fresh id, or a removed one reused.
    New,
}

/// Distinct values with dense ids `0..LIMIT`; removed ids are reused.
#[derive(Debug)]
pub struct Interner<T, const LIMIT: u32> {
    /// By id; `None` for a removed id awaiting reuse.
    values: Vec<Option<T>>,
    /// Removed ids, reused before fresh ones.
    free: Vec<u32>,
    /// `(hash, id)`: keeping the hash makes rehashing on growth exact and free.
    index: HashTable<(u64, u32)>,
}

impl<T: Hash + Eq, const LIMIT: u32> Interner<T, LIMIT> {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            values: Vec::new(),
            free: Vec::new(),
            index: HashTable::new(),
        }
    }

    /// The id of `value`, storing it first if it is new.
    ///
    /// # Errors
    ///
    /// When `value` is new and `LIMIT` values are live.
    pub fn intern(&mut self, value: T) -> Result<(u32, Interned), Full> {
        let hash = HASHER.hash_one(&value);
        let values = &self.values;
        if let Some((_, id)) = self.index.find(hash, |(h, id)| {
            *h == hash && values.get(widen(*id)).and_then(Option::as_ref) == Some(&value)
        }) {
            return Ok((*id, Interned::Existing));
        }
        let id = if let Some(id) = self.free.pop() {
            *self
                .values
                .get_mut(widen(id))
                .or_bug("a free id is within the storage") = Some(value);
            id
        } else {
            let id = next_id::<LIMIT>(self.values.len())?;
            self.values.push(Some(value));
            id
        };
        self.index.insert_unique(hash, (hash, id), |(h, _)| *h);
        Ok((id, Interned::New))
    }

    /// Frees `id` for reuse. It must be live.
    #[track_caller]
    pub fn remove(&mut self, id: u32) {
        let value = self
            .values
            .get_mut(widen(id))
            .and_then(Option::take)
            .or_bug("only live ids are removed");
        let hash = HASHER.hash_one(&value);
        self.index
            .find_entry(hash, |(_, found)| *found == id)
            .or_bug("a live id is indexed")
            .remove();
        self.free.push(id);
    }

    /// The live value with id `id`; `None` for a removed or unknown id.
    #[must_use]
    pub fn get_or_none(&self, id: u32) -> Option<&T> {
        self.values.get(widen(id)).and_then(Option::as_ref)
    }

    /// How many values are live.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.values.len() - self.free.len()
    }

    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl<T: Hash + Eq, const LIMIT: u32> Default for Interner<T, LIMIT> {
    fn default() -> Self {
        Self::new()
    }
}

/// Distinct slices packed into one arena, with dense ids `0..LIMIT`; removed
/// ids are reused.
#[derive(Debug)]
pub struct SliceInterner<T, const LIMIT: u32> {
    arena: Vec<T>,
    /// `(start, len)` of each live slice in `arena`, by id; `None` for a
    /// removed id awaiting reuse.
    spans: Vec<Option<(u32, u32)>>,
    /// Removed ids, reused before fresh ones.
    free: Vec<u32>,
    /// Arena elements that belong to removed slices.
    dead: usize,
    /// `(hash, id)`, as in [`Interner`].
    index: HashTable<(u64, u32)>,
}

impl<T: Hash + Eq + Copy, const LIMIT: u32> SliceInterner<T, LIMIT> {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            arena: Vec::new(),
            spans: Vec::new(),
            free: Vec::new(),
            dead: 0,
            index: HashTable::new(),
        }
    }

    /// The id of `slice`, copying it into the arena first if it is new.
    ///
    /// # Errors
    ///
    /// When `slice` is new and `LIMIT` slices are live.
    pub fn intern(&mut self, slice: &[T]) -> Result<(u32, Interned), Full> {
        let hash = HASHER.hash_one(slice);
        let (arena, spans) = (&self.arena, &self.spans);
        if let Some((_, id)) = self.index.find(hash, |(h, id)| {
            *h == hash && span_of(arena, spans, *id) == Some(slice)
        }) {
            return Ok((*id, Interned::Existing));
        }
        let reused = self.free.pop();
        let id = match reused {
            Some(id) => id,
            None => next_id::<LIMIT>(self.spans.len())?,
        };
        let start = u32::try_from(self.arena.len()).or_bug("an arena of live slices fits u32");
        let len = u32::try_from(slice.len()).or_bug("a slice fits u32");
        self.arena.extend_from_slice(slice);
        let span = Some((start, len));
        match reused {
            Some(_) => {
                *self
                    .spans
                    .get_mut(widen(id))
                    .or_bug("a free id is within the spans") = span;
            }
            None => self.spans.push(span),
        }
        self.index.insert_unique(hash, (hash, id), |(h, _)| *h);
        Ok((id, Interned::New))
    }

    /// Frees `id` for reuse, compacting the arena once removed slices take
    /// more room than live ones. It must be live.
    #[track_caller]
    pub fn remove(&mut self, id: u32) {
        let (start, len) = self
            .spans
            .get_mut(widen(id))
            .and_then(Option::take)
            .or_bug("only live ids are removed");
        let slice = self
            .arena
            .get(widen(start)..widen(start) + widen(len))
            .or_bug("a live span lies within the arena");
        let hash = HASHER.hash_one(slice);
        self.index
            .find_entry(hash, |(_, found)| *found == id)
            .or_bug("a live id is indexed")
            .remove();
        self.free.push(id);
        self.dead += widen(len);
        if self.dead > self.arena.len() - self.dead {
            self.compact();
        }
    }

    /// Copies the live slices into a fresh arena, in id order. Ids and hashes
    /// are unchanged. (In place would need arena order: reused ids sit at the
    /// end.) Rare: only after removals free more than is live.
    fn compact(&mut self) {
        let mut arena = Vec::with_capacity(self.arena.len() - self.dead);
        for span in self.spans.iter_mut().flatten() {
            let (start, len) = (widen(span.0), widen(span.1));
            let slice = self
                .arena
                .get(start..start + len)
                .or_bug("a live span lies within the arena");
            span.0 = u32::try_from(arena.len()).or_bug("a compacted offset is below its old one");
            arena.extend_from_slice(slice);
        }
        self.arena = arena;
        self.dead = 0;
    }

    /// The live slice with id `id`; `None` for a removed or unknown id.
    #[must_use]
    pub fn get_or_none(&self, id: u32) -> Option<&[T]> {
        span_of(&self.arena, &self.spans, id)
    }

    /// How many slices are live.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.spans.len() - self.free.len()
    }

    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Elements in the arena, live and dead: what the slices cost.
    #[must_use]
    pub const fn arena_len(&self) -> usize {
        self.arena.len()
    }
}

impl<T: Hash + Eq + Copy, const LIMIT: u32> Default for SliceInterner<T, LIMIT> {
    fn default() -> Self {
        Self::new()
    }
}

/// Values keyed by ids that only grow, in a sorted `Vec`. Not `Clone`.
#[derive(Debug, PartialEq, Eq)]
pub struct IdMap<K, V> {
    entries: Vec<(K, V)>,
}

impl<K: Ord + Copy, V> IdMap<K, V> {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            entries: Vec::new(),
        }
    }

    fn position(&self, key: K) -> Result<usize, usize> {
        self.entries.binary_search_by(|(k, _)| k.cmp(&key))
    }

    /// Adds `value` under `key`, which is new (ids are never reused).
    #[track_caller]
    pub fn insert(&mut self, key: K, value: V) {
        match self.position(key) {
            Ok(_) => plx_bug::bug!("an id is inserted once"),
            Err(at) => self.entries.insert(at, (key, value)),
        }
    }

    /// Sets the value under `key`, adding the key if it is new.
    pub fn upsert(&mut self, key: K, value: V) {
        match self.position(key) {
            Ok(at) => {
                if let Some(entry) = self.entries.get_mut(at) {
                    entry.1 = value;
                }
            }
            Err(at) => self.entries.insert(at, (key, value)),
        }
    }

    /// Removes and returns the value under `key`; `None` when there is none.
    pub fn remove_or_none(&mut self, key: K) -> Option<V> {
        match self.position(key) {
            Ok(at) => Some(self.entries.remove(at).1),
            Err(_) => None,
        }
    }

    /// Keeps only the entries for which `keep` is true.
    pub fn retain(&mut self, mut keep: impl FnMut(K, &V) -> bool) {
        self.entries.retain(|(k, v)| keep(*k, v));
    }

    /// Removes and returns the value under `key`, which is present.
    #[track_caller]
    pub fn remove(&mut self, key: K) -> V {
        let at = self.position(key).or_bug("a removed id is present");
        self.entries.remove(at).1
    }

    /// The value under `key`; `None` when there is none.
    #[must_use]
    pub fn get_or_none(&self, key: K) -> Option<&V> {
        self.position(key)
            .map_or(None, |at| self.entries.get(at).map(|(_, v)| v))
    }

    /// The value under `key`; `None` when there is none.
    #[must_use]
    pub fn get_mut_or_none(&mut self, key: K) -> Option<&mut V> {
        match self.position(key) {
            Ok(at) => self.entries.get_mut(at).map(|(_, v)| v),
            Err(_) => None,
        }
    }

    /// The value under `key`, which is present.
    #[must_use]
    #[track_caller]
    pub fn get_mut(&mut self, key: K) -> &mut V {
        self.get_mut_or_none(key)
            .or_bug("a looked-up id is present")
    }

    /// The value under `key`, which is present.
    #[must_use]
    #[track_caller]
    pub fn get(&self, key: K) -> &V {
        self.get_or_none(key).or_bug("a looked-up id is present")
    }

    #[must_use]
    pub fn contains(&self, key: K) -> bool {
        self.position(key).is_ok()
    }

    /// Entries in id order.
    pub fn iter(&self) -> impl Iterator<Item = (K, &V)> {
        self.entries.iter().map(|(k, v)| (*k, v))
    }

    /// Entries in id order, values mutable.
    pub fn iter_mut(&mut self) -> impl Iterator<Item = (K, &mut V)> {
        self.entries.iter_mut().map(|(k, v)| (*k, v))
    }

    /// Ids in order.
    pub fn keys(&self) -> impl Iterator<Item = K> + '_ {
        self.entries.iter().map(|(k, _)| *k)
    }

    #[must_use]
    pub const fn len(&self) -> usize {
        self.entries.len()
    }

    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

impl<K: Ord + Copy, V> Default for IdMap<K, V> {
    fn default() -> Self {
        Self::new()
    }
}

/// The id for a new entry when `len` are stored, if it is below `LIMIT`.
fn next_id<const LIMIT: u32>(len: usize) -> Result<u32, Full> {
    match u32::try_from(len) {
        Ok(id) if id < LIMIT => Ok(id),
        // Past u32::MAX is past any LIMIT.
        Ok(_) | Err(_) => Err(Full),
    }
}

fn span_of<'a, T>(arena: &'a [T], spans: &[Option<(u32, u32)>], id: u32) -> Option<&'a [T]> {
    let (start, len) = (*spans.get(widen(id))?)?;
    arena.get(widen(start)..widen(start) + widen(len))
}

#[cfg(not(target_pointer_width = "64"))]
compile_error!("plx targets 64-bit Linux");

/// `u32` to `usize`.
#[expect(
    clippy::as_conversions,
    reason = "usize is 64 bits (checked above), so this is lossless"
)]
const fn widen(value: u32) -> usize {
    value as usize
}

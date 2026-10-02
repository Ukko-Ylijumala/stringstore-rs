// Copyright (c) 2024-2026 Mikko Tanner. All rights reserved.

#[cfg(not(feature = "xxh128"))]
use custom_xxh3::hash_bytes;
#[cfg(feature = "xxh128")]
use xxhash_rust::xxh3::xxh3_128;
use dashmap::{mapref::entry::Entry, DashMap};
use miniutils::normalize_path;
use parking_lot::Mutex;
use std::{
    alloc::{alloc_zeroed, dealloc, handle_alloc_error, Layout},
    cmp::Ordering,
    error::Error,
    fmt::{self, Debug, Display, Formatter},
    hash::{BuildHasher, Hash, Hasher},
    hint,
    net::IpAddr, //Ipv4Addr, Ipv6Addr},
    ops::Deref,
    path::{Path, PathBuf},
    ptr,
    str::{self, FromStr, Split},
    sync::{
        atomic::{AtomicPtr, AtomicU32, Ordering as AtomicOrdering},
        Arc,
    },
};
use timesince::SecondsSinceEpoch;
//use uuid::Uuid;

#[cfg(feature = "size_of")]
use size_of::{Context, SizeOf};

const EMPTY_STR: &str = "";
const PATH_SEP: &str = "/";
const LATIN1_NUM: u32 = 256;
/// Maximum number of user-inserted strings; see `insert_unchecked`.
const MAX_USER_STRINGS: usize = (u32::MAX - LATIN1_NUM) as usize;
/// Default size in bytes of one [StrArena] chunk; `new_with_capacity` can
/// override it per store. 128 KiB is well above any typical string, so
/// the wasted tail per chunk is negligible, and small enough that a store
/// with a handful of strings does not reserve much.
pub const ARENA_CHUNK_SIZE: usize = 128 * 1024;
/**
Upper bound of the arena chunk size; `new_with_capacity` clamps larger
requests to it. A bigger chunk saves nothing, and one over `isize::MAX`
bytes cannot be allocated at all.
*/
const MAX_ARENA_CHUNK_SIZE: usize = 1 << 30;
/// log2 of the slot count of the first [SlotTable] segment (32 slots)
const SLOT_SEG0_BITS: u32 = 5;
/**
Number of [SlotTable] segments. Segment `k` holds `32 << k` slots, so 27
of them (`2^32 - 32` slots) cover all `MAX_USER_STRINGS` internal indices.
*/
const SLOT_SEGMENTS: usize = 32 - SLOT_SEG0_BITS as usize;
/// Busy-wait rounds before a reader waiting on an in-flight copy blocks on the mutex.
const SLOT_WAIT_SPINS: u32 = 64;

/**
The index key: an xxh3 digest of the string bytes.

64 bits by default, which is the memory-efficient choice: a hash hit is
verified against the stored contents (one lock-free slot read + one
compare on the duplicate-insert path) so a collision panics instead of
corrupting indices. The `xxh128` feature widens the key to 128 bits, which
makes a collision negligible enough (~n²/2¹²⁹) that the verification is
skipped and duplicate inserts never read the stored string — at the cost
of doubling the per-entry footprint of the index map. See
`doc/design/storage-architecture.md`.
*/
#[cfg(not(feature = "xxh128"))]
type HashKey = u64;
#[cfg(feature = "xxh128")]
type HashKey = u128;

/**
A memory-efficient storage for unique string slices with stable indexing.

[UniqueStrStore] implements a string interning system, which stores only one
copy of each distinct string. This should significantly reduce memory usage
in scenarios where many duplicate strings are used.

## Key Features
- Efficient: each unique string is stored only once.
- Fast lookups: O(1) average complexity for both index and content-based lookups.
- Stable indexing: once a string is stored, its index remains constant.
- Allocations: string bytes live in a chunked bump arena (one allocation
  per chunk, not per string); see `StrArena`.
- the empty string ("") always occupies the first index (0).
- ISO-8859-1 codepoints: contained explicitly, at indices 1-255 (minus '\0').

## Design Considerations
- Uses a lock-free segmented table of fat pointers into arena chunks for
  string storage, which is efficient for random access from any thread.
- Uses a [DashMap] with Xxh3 string hashes as keys for fast lookups: `u64`
  from `custom_xxh3::hash_bytes` (one-shot, custom secret) by default,
  `u128` with the `xxh128` feature. The map uses the keys as they are,
  without hashing them again.
- Thread-safe.
- trait [SizeOf]: provides a way to measure the size of the structure in memory.
- ISO-8859-1: separate non-locking [Vec] for indices 0-255 to avoid locking
  and hashing overhead for common characters.
- First inserted string is always at index 256.

## Performance Characteristics
- Insertion: O(1) average
- Lookup by content: O(1) average
- Lookup by index: O(1)
- Memory overhead: small fixed cost per unique string

## Usage
This structure should work nicely for scenarios where you need to store many
duplicate strings and require fast lookups by both content and stable indices.

## Safety
While most operations are safe, the `get_unchecked` method provides an unsafe,
non-bounds-checking lookup (meant mostly for internal use with known indices).

## Limitations
- Does not support string removal to maintain index stability.
- Does not support string modification after insertion.
- No partial deduplication of strings (e.g. substrings).
- The maximum number of unique strings is limited by the [u32] index.

## Example
```
use stringstore::UniqueStrStore;

let hello: &'static str = "Hello, world!";
let store = UniqueStrStore::new();
assert_eq!(store.len(), 256); // incl. ISO-8859-1 codepoints

let hello_id = store.insert(hello);
assert_eq!(store.len(), 256 + 1);
assert!(store.contains(hello));
assert_eq!(hello_id, 256, "hello string should be stored at index 256");

// try to insert the same string again
let hello_id2 = store.insert(hello);
assert_eq!(hello_id, hello_id2);
assert_eq!(store.get(hello_id).unwrap(), hello);

let foo_id = store.insert("foo");
assert_eq!(store.len(), 256 + 2);
assert_eq!(foo_id, 257, "foo string should be stored at index 257");

// panics if the index is out of bounds
assert_eq!(unsafe { store.borrow_str(foo_id) }, "foo");

// check internal consistency
store.validate_contents().expect("Store validation failed");
*/
#[derive(Clone)]
pub struct UniqueStrStore(Arc<StoreInner>);

/// Prints the public length, the arena's shape and every user string as
/// `public index: "string"`; the ISO-8859-1 table and the raw hash index
/// are left out, since neither is legible or informative.
impl Debug for UniqueStrStore {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        // the mutex freezes `len` while the strings are listed
        let arena = self.0.store.lock();
        f.debug_struct("UniqueStrStore")
            .field("len", &self.len())
            .field("arena", &*arena)
            .field("strings", &StoreStrings(self))
            .finish()
    }
}

/// `Debug` helper: maps every user string from its *public* index
/// without collecting anything. Only used with the store mutex held.
struct StoreStrings<'a>(&'a UniqueStrStore);

impl Debug for StoreStrings<'_> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        let store: &UniqueStrStore = self.0;
        let entries = (LATIN1_NUM..store.len() as u32).map(|idx| (idx, store.slot(idx - LATIN1_NUM)));
        f.debug_map().entries(entries).finish()
    }
}

/**
The shared state behind a [UniqueStrStore]. All fields live behind one
`Arc`: a `Clone` is a single refcount increment and every access goes
through one pointer instead of several separately allocated ones.

Reads never lock. `store` is taken only by inserts and by whole-store
walks (`Debug`, `validate_contents`, `SizeOf`); readers go through
`slots`, gated by `len`. One index, `len` itself, can be published a
moment before its string is copied; a reader that meets it waits for the
copy (see `await_copy`).
*/
struct StoreInner {
    /// writer state: the arena chunks
    store: Mutex<StrArena>,
    /// one pointer per user string, readable without the lock
    slots: SlotTable,
    index: DashMap<HashKey, u32, PreHashed>,
    ascii: Vec<Box<str>>,
    /// public length: strings whose slot is written, incl. the ISO-8859-1 range
    len: AtomicU32,
}

/**
[BuildHasher] for the index map, whose keys are already xxh3 digests of
the string bytes ([HashKey]). Re-hashing them through a full Xxh3 state
(`CustomXxh3Hasher`, 832 bytes, rebuilt on every lookup) cost ~140 ns
per map operation; passing the key straight through costs ~30 ns.
`custom_xxh3::QuickXxh3Builder` would re-hash a `u64` in ~1 ns, but that
is still 1 ns more than passing it through, for no better distribution.

This is sound because xxh3 output is uniformly distributed across all of
its bits, which is what [DashMap]'s shard selection (high bits) and
hashbrown's control bytes / bucket index (top 7 bits, low bits) rely on.
A 128-bit key is xor-folded to 64 bits so every key bit still takes part.
`write` exists only to satisfy the trait; the map never hashes anything
but a [HashKey].
*/
#[derive(Clone, Copy, Default)]
struct PreHashed(u64);

impl Hasher for PreHashed {
    #[inline]
    fn write_u64(&mut self, key: u64) {
        self.0 = key;
    }

    #[inline]
    fn write_u128(&mut self, key: u128) {
        self.0 = (key as u64) ^ ((key >> 64) as u64);
    }

    fn write(&mut self, bytes: &[u8]) {
        for chunk in bytes.chunks(8) {
            let mut b: [u8; 8] = [0; 8];
            b[..chunk.len()].copy_from_slice(chunk);
            self.0 = self.0.rotate_left(5) ^ u64::from_le_bytes(b);
        }
    }

    #[inline]
    fn finish(&self) -> u64 {
        self.0
    }
}

impl BuildHasher for PreHashed {
    type Hasher = PreHashed;

    #[inline]
    fn build_hasher(&self) -> PreHashed {
        PreHashed(0)
    }
}

#[cfg(feature = "size_of")]
impl SizeOf for PreHashed {
    fn size_of_children(&self, _context: &mut Context) {}
}

/**
Append-only bump arena holding the bytes of every user-inserted string.

Strings are copied back to back into fixed-size heap chunks that are
never resized, moved or freed until the arena is dropped; a string longer
than a chunk gets a dedicated chunk of exactly its own size. `push`
returns the fat pointer to the copy; the store keeps it in its
[SlotTable].

Compared to one `Box<str>` per string this removes the allocator's
per-allocation overhead (glibc's smallest block is 32 bytes, so a 9-byte
string used to cost 32 bytes of heap plus its 16-byte slot; it now costs
9 plus 16), takes the `malloc` call out of the write-lock critical
section, and makes drop O(chunks) instead of O(strings). Measurements are
in `doc/design/storage-architecture.md`.

The pointer-stability argument in `doc/design/unsafe-pointers.md` keeps
its shape: `chunks` may reallocate as it grows, which moves the chunk
pointers but never the bytes they point at.

A chunk is owned through the raw pointer `Box::into_raw` returns (freed
in `Drop`), never as a `Box<[u8]>`. A `Box`, and any `&mut [u8]` taken
through it, asserts exclusive access to the whole chunk: moving the `Box`
or borrowing the chunk mutably to append a string would invalidate every
`&str` already handed out into it (Miri's Stacked Borrows flags exactly
that). Appends therefore write through a raw pointer, to the unfilled
tail only.
*/
struct StrArena {
    /// bump chunks of `chunk_size` bytes, each from `Box::into_raw` and
    /// freed in `Drop`; the last one is being filled
    chunks: Vec<*mut [u8]>,
    /// exact-size chunks for strings longer than `chunk_size` (same ownership)
    oversized: Vec<*mut [u8]>,
    /// bytes used in the last element of `chunks`
    cursor: usize,
    chunk_size: usize,
}

/*
SAFETY: every chunk pointer is a unique allocation owned by this struct.
String bytes are written once, under `&mut self`, before the pointer to
them is handed out, and are freed only when the whole arena drops.
Sharing a `StrArena` across threads (behind the store's mutex) is
therefore no different from sharing a `Vec<Box<str>>`.
*/
unsafe impl Send for StrArena {}
unsafe impl Sync for StrArena {}

impl StrArena {
    fn new(chunk_size: usize) -> Self {
        Self {
            chunks: Vec::new(),
            oversized: Vec::new(),
            cursor: 0,
            chunk_size: chunk_size.clamp(1, MAX_ARENA_CHUNK_SIZE),
        }
    }

    /// Whether an `n`-byte string (not an oversized one) needs a fresh chunk.
    #[inline]
    fn needs_chunk(&self, n: usize) -> bool {
        self.chunks.is_empty() || self.cursor + n > self.chunk_size
    }

    /// Start a fresh bump chunk; the unused tail of the previous one is abandoned.
    fn new_chunk(&mut self) {
        let chunk: Box<[u8]> = vec![0u8; self.chunk_size].into_boxed_slice();
        self.chunks.push(Box::into_raw(chunk));
        self.cursor = 0;
    }

    /**
    Make room for one more string of `n` bytes. Every step of an append
    that can fail (`Vec` growth, a new chunk) runs here, so that the `push`
    after it cannot unwind; see `push`. Reserving without pushing is
    harmless: room is only used up by `push`.
    */
    fn reserve(&mut self, n: usize) {
        if n > self.chunk_size {
            self.oversized.reserve(1);
        } else if self.needs_chunk(n) {
            self.new_chunk();
        }
    }

    /**
    Copy `s` into the arena and return the pointer to the copy.

    After `reserve(s.len())` this neither grows a `Vec` nor allocates a
    chunk. The one allocation left is the exact-size chunk of an oversized
    string, which cannot unwind: a `&str` is at most `isize::MAX` bytes,
    so its layout is valid, and a failed allocation aborts. Without the
    reserve it still works, it just may allocate (and so fail) itself.
    */
    fn push(&mut self, s: &str) -> *const str {
        let n: usize = s.len();

        if n > self.chunk_size {
            // does not fit any chunk: give it an allocation of its own,
            // whose bytes are a verbatim copy of a `&str` (valid UTF-8)
            let chunk: *mut [u8] = Box::into_raw(Box::<[u8]>::from(s.as_bytes()));
            self.oversized.push(chunk);
            return chunk.cast_const() as *const str;
        }

        if self.needs_chunk(n) {
            // `reserve` has normally done this already
            self.new_chunk();
        }
        let start: usize = self.cursor;
        let base: *mut u8 = self.chunks.last().expect("a chunk was just ensured").cast::<u8>();
        /*
        SAFETY: `start + n <= chunk_size`, so the destination lies inside
        the chunk, past every string handed out. Only the raw pointer is
        used, so no reference to the chunk exists that could invalidate
        those strings. The copied bytes come from a `&str`: valid UTF-8.
        */
        let ptr: *const str = unsafe {
            let dst: *mut u8 = base.add(start);
            ptr::copy_nonoverlapping(s.as_ptr(), dst, n);
            ptr::slice_from_raw_parts(dst.cast_const(), n) as *const str
        };
        self.cursor = start + n;
        ptr
    }
}

impl Drop for StrArena {
    fn drop(&mut self) {
        for &chunk in self.chunks.iter().chain(&self.oversized) {
            // SAFETY: each chunk came from `Box::into_raw` and is freed only here
            drop(unsafe { Box::from_raw(chunk) });
        }
    }
}

impl Debug for StrArena {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.debug_struct("StrArena")
            .field("chunks", &self.chunks.len())
            .field("oversized", &self.oversized.len())
            .field("chunk_size", &self.chunk_size)
            .field("cursor", &self.cursor)
            .finish()
    }
}

#[cfg(feature = "size_of")]
impl SizeOf for StrArena {
    fn size_of_children(&self, context: &mut Context) {
        // every chunk is one allocation; only the tail of the chunk
        // currently being filled is slack
        for chunk in self.chunks.iter().chain(self.oversized.iter()) {
            context.add(chunk.len()).add_distinct_allocation();
        }
        if let Some(last) = self.chunks.last() {
            context.add_excess(last.len() - self.cursor);
        }
        for (len, cap) in [
            (self.chunks.len(), self.chunks.capacity()),
            (self.oversized.len(), self.oversized.capacity()),
        ] {
            if cap > 0 {
                context
                    .add_vectorlike(len, cap, size_of::<*mut [u8]>())
                    .add_distinct_allocation();
            }
        }
    }
}

/**
One `*const str` per user string, in internal index order, readable
without any lock. Unlike a `Vec` it never moves a slot: it is a fixed
array of segments of doubling size (`32 << k` slots in segment `k`),
each allocated on first use and freed only on drop, so a reader can load
a slot while a writer is adding more.

Synchronization is by publication, not by the slots themselves: the
single writer (holding the store mutex) writes each slot exactly once
and only then Release-stores `len` past it; a reader loads a slot only
after an Acquire load of `len` showed it, or after waiting out the
insert that published it (see `await_copy`).
*/
struct SlotTable {
    segments: [AtomicPtr<*const str>; SLOT_SEGMENTS],
}

impl SlotTable {
    /// A table whose segments already cover the first `capacity` slots.
    fn with_capacity(capacity: usize) -> Self {
        let table: SlotTable = Self {
            segments: [const { AtomicPtr::new(ptr::null_mut()) }; SLOT_SEGMENTS],
        };
        if capacity > 0 {
            let last: u32 = capacity.min(MAX_USER_STRINGS) as u32 - 1;
            for k in 0..=Self::locate(last).0 {
                table.alloc_segment(k);
            }
        }
        table
    }

    /// Segment number and offset within it of slot `i`.
    #[inline]
    fn locate(i: u32) -> (usize, usize) {
        let j: usize = i as usize + (1 << SLOT_SEG0_BITS);
        let top: u32 = usize::BITS - 1 - j.leading_zeros();
        ((top - SLOT_SEG0_BITS) as usize, j - (1 << top))
    }

    /// Slot count of segment `k`.
    #[inline]
    fn segment_len(k: usize) -> usize {
        1 << (k as u32 + SLOT_SEG0_BITS)
    }

    /// Memory layout of segment `k`.
    fn segment_layout(k: usize) -> Layout {
        Layout::array::<*const str>(Self::segment_len(k)).expect("slot segment size overflows isize")
    }

    /// Zeroed, so the pages of a large segment stay untouched until used
    /// (like `Vec::with_capacity`); all-zero is a valid (null) raw pointer.
    fn alloc_segment(&self, k: usize) {
        let layout: Layout = Self::segment_layout(k);
        // SAFETY: the layout is non-zero-sized; zeroed memory is valid `*const str`s
        let seg: *mut *const str = unsafe { alloc_zeroed(layout) }.cast::<*const str>();
        if seg.is_null() {
            handle_alloc_error(layout);
        }
        self.segments[k].store(seg, AtomicOrdering::Release);
    }

    /**
    Make sure slot `i` has a segment. May allocate, so it can fail: call
    it before publishing anything about `i`.

    # Safety
    Only the holder of the store mutex may call `ensure` or `write`.
    */
    unsafe fn ensure(&self, i: u32) {
        let k: usize = Self::locate(i).0;
        if self.segments[k].load(AtomicOrdering::Relaxed).is_null() {
            self.alloc_segment(k);
        }
    }

    /**
    Write slot `i`.

    # Safety
    The store mutex is held, `ensure(i)` ran, and slot `i` has never been
    written (no reader can be looking at it yet).
    */
    #[inline]
    unsafe fn write(&self, i: u32, s: *const str) {
        let (k, off) = Self::locate(i);
        self.segments[k].load(AtomicOrdering::Relaxed).add(off).write(s);
    }

    /**
    Read slot `i`.

    # Safety
    Slot `i` was written, and this thread observed that through an
    Acquire load of `len`, or of the index entry published after it.
    */
    #[inline]
    unsafe fn read(&self, i: u32) -> *const str {
        let (k, off) = Self::locate(i);
        *self.segments[k].load(AtomicOrdering::Acquire).add(off)
    }

    #[cfg(feature = "size_of")]
    fn size_of_children(&self, used: usize, context: &mut Context) {
        let mut allocated: usize = 0;
        for (k, seg) in self.segments.iter().enumerate() {
            if !seg.load(AtomicOrdering::Relaxed).is_null() {
                allocated += Self::segment_len(k);
                context
                    .add(Self::segment_len(k) * size_of::<*const str>())
                    .add_distinct_allocation();
            }
        }
        context.add_excess(allocated.saturating_sub(used) * size_of::<*const str>());
    }
}

impl Drop for SlotTable {
    fn drop(&mut self) {
        for (k, seg) in self.segments.iter_mut().enumerate() {
            let p: *mut *const str = *seg.get_mut();
            if !p.is_null() {
                // SAFETY: allocated by `alloc_segment` with this layout, freed only here
                unsafe { dealloc(p.cast::<u8>(), Self::segment_layout(k)) };
            }
        }
    }
}

/// Outcome of the lock-free first half of an insert (`UniqueStrStore::find_or_hash`).
enum Resolved {
    /// Needs no insert: the public index of the empty string, of a single
    /// ISO-8859-1 character, or of an already interned string.
    Found(u32),
    /// Not interned (yet): the index key to insert the string under.
    Missing(HashKey),
}

// The derived `Default` would bypass `new()` and produce a store with an
// empty `ascii` table and `len == 0`, violating every invariant.
impl Default for UniqueStrStore {
    fn default() -> Self {
        Self::new()
    }
}

impl UniqueStrStore {
    /// Create a new [UniqueStrStore] with a default capacity of 128 and
    /// the default arena chunk size ([ARENA_CHUNK_SIZE]).
    pub fn new() -> Self {
        Self::new_with_capacity(128, ARENA_CHUNK_SIZE)
    }

    /**
    Create a new [UniqueStrStore] sized for `capacity` strings, with string
    bytes stored in arena chunks of `chunk_size` bytes (see [StrArena];
    [ARENA_CHUNK_SIZE] is the default). A string longer than `chunk_size`
    gets an allocation of its own, so any size works; a chunk size below
    the typical string length just degrades to one allocation per string.
    `chunk_size` is clamped to `1..=1 GiB`.
    */
    pub fn new_with_capacity(capacity: usize, chunk_size: usize) -> Self {
        // Make the ISO-8859-1 codepoint Vec. Its first element
        // is always the empty string.
        let mut latin1: Vec<Box<str>> = (0..LATIN1_NUM)
            .map(|i: u32| {
                // this is safe because we stay in a safe range
                unsafe { char::from_u32_unchecked(i) }.to_string().into()
            })
            .collect();
        // replace the null string ('\0') with an empty string
        latin1[0] = EMPTY_STR.into();

        UniqueStrStore(Arc::new(StoreInner {
            store: Mutex::new(StrArena::new(chunk_size)),
            slots: SlotTable::with_capacity(capacity),
            index: DashMap::with_capacity_and_hasher(capacity, PreHashed::default()),
            ascii: latin1,
            len: AtomicU32::new(LATIN1_NUM),
        }))
    }

    /// Put this [UniqueStrStore] into an [Arc].
    pub fn shared(self) -> Arc<Self> {
        self.into()
    }

    /**
    The number of unique string slices. Includes the ISO-8859-1 codepoints.

    Acquire pairs with the Release increment in `insert_unchecked`:
    observing the new length implies the corresponding push is visible.
    */
    #[inline]
    pub fn len(&self) -> usize {
        self.0.len.load(AtomicOrdering::Acquire) as usize
    }

    /// Always false: indices 0-255 (the ISO-8859-1 codepoints) are populated
    /// at construction and nothing is ever removed.
    #[inline]
    pub fn is_empty(&self) -> bool {
        false
    }

    /**
    Whether we already have this string slice stored.

    NOTE: this is a pure hash lookup (no content comparison, no locking),
    so a never-inserted string whose xxh3 hash collides with a stored one
    yields a false positive. `insert` does verify contents (with 64-bit
    keys; see [HashKey]) — `doc/design/storage-architecture.md` has the
    collision policy.
    */
    #[inline]
    pub fn contains(&self, s: &str) -> bool {
        if s.is_empty() {
            return true; // empty string is always contained
        }

        // ISO-8859-1 codepoints (minus NUL) are implicitly contained. NOTE:
        // codepoints 128-255 are 2 bytes in UTF-8, hence the `<= 2` gate.
        if s.len() <= 2 && return_iso8859_1_cp(s).is_some() {
            return true;
        }

        self.0.index.contains_key(&hash_key(s.as_bytes()))
    }

    /**
    Get the index of a stored string slice by its content, if it exists.

    NOTE: like `contains`, this is a pure hash lookup — an xxh3 collision
    with a stored string returns that string's index instead of `None`.
    `insert` is the verified path (with 64-bit keys; see [HashKey]).
    */
    pub fn idx(&self, s: &str) -> Option<u32> {
        if s.is_empty() {
            return Some(0);
        }

        if s.len() <= 2 {
            if let Some(c) = return_iso8859_1_cp(s) {
                return Some(c);
            }
        }

        self.0.index
            .get(&hash_key(s.as_bytes()))
            .map(|r| r.value() + LATIN1_NUM)
    }

    /// Get a reference to a stored string slice by its index, if it exists.
    #[inline]
    pub fn get(&self, idx: u32) -> StringStoreResult<&str> {
        match self.resolve(idx) {
            Some(s) => Ok(s),
            // a tail call: keeps the hot path free of register saves
            None => self.get_slow(idx),
        }
    }

    #[cold]
    #[inline(never)]
    fn get_slow(&self, idx: u32) -> StringStoreResult<&str> {
        match self.resolve_past_len(idx) {
            Some(s) => Ok(s),
            None => Err(StringStoreError::oob(idx, self.len() - 1)),
        }
    }

    /**
    Resolve a public index to a pointer at the stored [str], or `None` if
    it is out of bounds. Takes no lock; every bounds-checked read path
    (`get`, `borrow_str`, `reconstruct`) is built on this.
    */
    #[inline]
    fn lookup(&self, idx: u32) -> Option<*const str> {
        self.resolve(idx).or_else(|| self.resolve_past_len(idx)).map(|s| s as *const str)
    }

    /// The fast half of `lookup`: every index below `len`. `None` for the
    /// rest, which `resolve_past_len` settles.
    #[inline]
    fn resolve(&self, idx: u32) -> Option<&str> {
        if idx < LATIN1_NUM {
            return Some(&self.0.ascii[idx as usize]);
        }
        if idx < self.0.len.load(AtomicOrdering::Acquire) {
            // SAFETY: the Acquire load of `len` shows the slot written
            return Some(unsafe { &*self.0.slots.read(idx - LATIN1_NUM) });
        }
        None
    }

    /**
    The slow half of `lookup`, for `idx >= len` as last seen. `idx == len`
    may be published and still being copied: only the mutex holder
    inserts, one string at a time, so no other index can be. Spin only
    while an insert is running; either way the mutex settles it, since any
    published index is copied before the unlock. (A free mutex seen by a
    plain load proves nothing: the insert may have finished after our
    `len` load, without us seeing its store.)
    */
    #[cold]
    #[inline(never)]
    fn resolve_past_len(&self, idx: u32) -> Option<&str> {
        if idx >= LATIN1_NUM && idx == self.0.len.load(AtomicOrdering::Acquire) {
            if self.0.store.is_locked() {
                self.await_copy(idx - LATIN1_NUM);
            } else {
                drop(self.0.store.lock());
            }
        }
        // re-check: `len` may have passed `idx` since the caller looked
        self.resolve(idx)
    }

    /**
    The string at internal index `i`, which must be one the store handed
    out (an index entry) or that a bounds check admitted. Takes no lock,
    and may briefly wait for an in-flight copy (see `await_copy`).

    The wait is a cold tail call, so the inlined fast path (every
    `StoredStr` read goes through here) needs no register saves.
    */
    #[inline]
    fn slot(&self, i: u32) -> &str {
        if i + LATIN1_NUM < self.0.len.load(AtomicOrdering::Acquire) {
            // SAFETY: the Acquire load of `len` shows the slot written
            return unsafe { &*self.0.slots.read(i) };
        }
        self.slot_in_flight(i)
    }

    /// `slot` for an index at or past `len` as last seen: published, so
    /// its copy is in flight and ends before the insert unlocks.
    #[cold]
    #[inline(never)]
    fn slot_in_flight(&self, i: u32) -> &str {
        self.await_copy(i);
        // SAFETY: `await_copy` returned, so the slot is written and observed
        unsafe { &*self.0.slots.read(i) }
    }

    /**
    Wait out an insert that may be copying internal index `i`.
    `insert_unchecked` publishes an index entry a moment before it copies
    the string, all under the store mutex, so a reader that got the index
    early spins briefly on `len`, then waits for the mutex. Returns once
    `len` passes `i` or the insert that held the mutex has finished; the
    caller re-checks `len` when the index may not have been published.
    */
    #[cold]
    #[inline(never)]
    fn await_copy(&self, i: u32) {
        for _ in 0..SLOT_WAIT_SPINS {
            if i + LATIN1_NUM < self.0.len.load(AtomicOrdering::Acquire) {
                return;
            }
            hint::spin_loop();
        }
        // the copy ends before the mutex is released
        drop(self.0.store.lock());
    }

    /**
    Get a raw pointer by index to a string slice in the store.
    Does no bounds checking apart from deciding whether to get the pointer
    from the ISO-8859-1 range Vec, or from the slot table.

    This is safe because we're returning a pointer to string bytes inside an
    arena chunk, which never move and are freed only with the store.
    */
    #[inline]
    unsafe fn get_str_ptr(&self, idx: u32) -> *const str {
        // ISO-8859-1 range
        if idx < LATIN1_NUM {
            self.0.ascii[idx as usize].as_ref() as *const str
        } else {
            self.slot(idx - LATIN1_NUM) as *const str
        }
    }

    /**
    Borrow a raw reference to a stored [str]. For a safe alternative, use `get`.

    # Safety
    Calling this method with an out-of-bounds index will panic. Callers
    must uphold the append-only contract (see
    `doc/design/unsafe-pointers.md`).
    */
    #[inline]
    pub unsafe fn borrow_str(&self, idx: u32) -> &str {
        match self.lookup(idx) {
            Some(ptr) => &*ptr,
            None => panic!("Store index {idx} out of bounds (max: {})", self.len() - 1),
        }
    }

    /**
    Get a pointer to a stored string slice.

    WARNING: THIS IS AN UNSAFE FN AND SHOULD BE USED WITH CAUTION.
    NO BOUNDS CHECKING IS PERFORMED

    # Safety
    `idx` must be in bounds (`idx < self.len()`) — no bounds checking is
    performed. The pointer is only valid as long as the store is alive, but
    this is not enforced; the lifetime is the responsibility of the user.
    */
    pub unsafe fn get_ptr(&self, idx: u32) -> StoredStrPtr {
        StoredStrPtr(self.get_str_ptr(idx))
    }

    /**
    Insert a new string foregoing the first index check before taking the
    store mutex. `key` must be the xxh3 hash of `s` (computed by the caller,
    so the fast path and this slow path hash only once between them).

    We still must check again after acquiring the mutex, as another
    thread might have gone behind our back in the meantime.
    */
    fn insert_unchecked(&self, s: &str, key: HashKey) -> StringStoreResult<u32> {
        let mut arena = self.0.store.lock();
        self.insert_locked(&mut arena, s, key)
    }

    /**
    The body of `insert_unchecked`, run by a caller that already holds the
    store mutex (`arena` is its guarded contents) - so that a batch can
    insert all of its strings under one lock hold. Each call completes a
    whole insert, `len` store included, before it returns: a later string
    of the same batch may be a duplicate of this one, and its recheck then
    reads this slot, which must already be below `len` (a slot at or past
    `len` must never be read under the mutex).
    */
    fn insert_locked(&self, arena: &mut StrArena, s: &str, key: HashKey) -> StringStoreResult<u32> {
        // only the mutex holder moves `len`, so this is stable until we unlock
        let len: usize = (self.0.len.load(AtomicOrdering::Relaxed) - LATIN1_NUM) as usize;

        /*
        Refuse inserts that would overflow the u32 public index space.
        Public index = internal index + LATIN1_NUM, and the public length
        counter `len` is itself a u32, so the store can hold at most
        MAX_USER_STRINGS user-inserted strings: the last one lands at
        public index u32::MAX - 1 and pushes `len` to exactly u32::MAX.
        (Allowing one more would wrap `len` to 0.)
        */
        if len >= MAX_USER_STRINGS {
            return Err(StringStoreError::StoreFull);
        }

        // next free index
        let idx: u32 = len as u32;

        /*
        Do everything that can fail (a new chunk, a new slot segment)
        before the entry is published below. From then on nothing may
        unwind until the slot is written.
        */
        arena.reserve(s.len());
        // SAFETY: we hold the store mutex
        unsafe { self.0.slots.ensure(idx) };

        // atomic recheck: another thread may have interned this hash since
        // the caller's lookup
        match self.0.index.entry(key) {
            Entry::Vacant(vacant) => {
                /*
                Publish first, then copy. A reader that finds the entry
                before `write_slot` releases `len` past it waits in
                `await_copy`, at most until we unlock. (Copying first and
                publishing last needs no waiting, but measured ~25 % slower
                on fresh inserts; see `doc/design/concurrency.md`.)
                */
                vacant.insert(idx);
                self.write_slot(arena, idx, s);
                Ok(idx + LATIN1_NUM)
            }
            Entry::Occupied(hit) => {
                // someone else interned this hash first: make sure it is
                // actually the same string and not a hash collision
                let indexed: u32 = *hit.get();
                drop(hit);
                #[cfg(not(feature = "xxh128"))]
                verify_hit(self.slot(indexed), indexed, s);
                Ok(indexed + LATIN1_NUM)
            }
        }
    }

    /// Copy `s` into the arena, write slot `idx`, and release `len` past it.
    #[inline]
    fn write_slot(&self, arena: &mut StrArena, idx: u32, s: &str) {
        let ptr: *const str = arena.push(s);
        // SAFETY: mutex held by our caller, `ensure(idx)` ran, slot is fresh
        unsafe { self.0.slots.write(idx, ptr) };
        self.0.len.store(idx + LATIN1_NUM + 1, AtomicOrdering::Release);
    }

    /// Core insert logic shared by `insert` and `try_insert`. Borrows only —
    /// the already-interned case (the hot path) allocates nothing.
    fn insert_internal(&self, s: &str) -> StringStoreResult<u32> {
        match self.find_or_hash(s) {
            Resolved::Found(idx) => Ok(idx),
            Resolved::Missing(key) => self.insert_unchecked(s, key),
        }
    }

    /**
    The lock-free first half of every insert: resolve `s` if it needs no
    insert, or else hash it for the locked second half (which rechecks the
    index, see `insert_locked`), so each string is hashed only once.
    */
    #[inline]
    fn find_or_hash(&self, s: &str) -> Resolved {
        if s.is_empty() {
            return Resolved::Found(0);
        }

        if s.len() <= 2 {
            if let Some(c) = return_iso8859_1_cp(s) {
                return Resolved::Found(c); // ISO-8859-1 code point
            }
        }

        /*
        For non-ASCII or multi-character strings. NOTE: copy the index
        out of the DashMap guard before reading the slot, so no shard
        guard is held while `slot` might wait for an in-flight copy.
        */
        let key: HashKey = hash_key(s.as_bytes());
        if let Some(internal) = self.0.index.get(&key).map(|r| *r.value()) {
            /*
            64-bit keys: verify the hit against the stored string, read
            without any lock. 128-bit keys (`xxh128`): the hash is trusted
            and the slot is not read at all.
            */
            #[cfg(not(feature = "xxh128"))]
            verify_hit(self.slot(internal), internal, s);
            return Resolved::Found(internal + LATIN1_NUM);
        }
        Resolved::Missing(key)
    }

    /**
    Insert a new string (slice), if it doesn't already exist.
    Returns the index in either case.

    On a hash hit the existing string's contents are compared against `s`;
    a mismatch means a genuine xxh3 collision, which the hash-keyed index
    cannot represent, and results in a panic. With the `xxh128` feature the
    key is 128 bits wide and the comparison is skipped; see [HashKey] and
    `doc/design/storage-architecture.md` for the collision policy.

    Panics when the store is full (the u32 index space is exhausted) —
    use `try_insert` for a `Result`-returning alternative.
    */
    pub fn insert<T>(&self, s: T) -> u32
    where
        T: AsRef<str>,
    {
        match self.insert_internal(s.as_ref()) {
            Ok(idx) => idx,
            Err(e) => panic!("UniqueStrStore::insert failed: {e}"),
        }
    }

    /**
    Like `insert`, but returns [StringStoreError::StoreFull] instead of
    panicking when the u32 index space is exhausted.

    NOTE: a genuine hash collision (64-bit keys) still panics — it signals
    that the store cannot represent the string at all, which no caller can
    meaningfully recover from. See `doc/design/storage-architecture.md`.
    */
    pub fn try_insert<T>(&self, s: T) -> StringStoreResult<u32>
    where
        T: AsRef<str>,
    {
        self.insert_internal(s.as_ref())
    }

    /**
    Insert a batch of strings; returns their indices in input order.

    Same result as calling `insert` on each string in turn (duplicates
    within the batch get one index, the first occurrence inserting it),
    but the store mutex is taken **at most once** for the whole batch
    instead of once per new string. Strings that need no insert (already
    interned, empty, single ISO-8859-1 characters) are resolved lock-free
    first, as in `insert`; a batch of those only never takes the lock.

    Meant for bulk producers with many new strings, e.g. a parallel
    directory walker interning file names: with every worker taking the
    mutex per string, the handoffs dominate once the per-string work is
    small. The cost is a longer hold: a reader that has to wait out an
    in-flight index (see `doc/design/concurrency.md`) may wait for the
    rest of the batch, and so may other writers. Keep batches moderate
    (tens to a few hundred strings).

    Panics when the store is full, and on a genuine hash collision (64-bit
    keys), like `insert`. See `try_insert_many` for a non-panicking
    alternative to the former.
    */
    pub fn insert_many<T>(&self, strs: &[T]) -> Vec<u32>
    where
        T: AsRef<str>,
    {
        match self.try_insert_many(strs) {
            Ok(indices) => indices,
            Err(e) => panic!("UniqueStrStore::insert_many failed: {e}"),
        }
    }

    /**
    Like `insert_many`, but returns [StringStoreError::StoreFull] instead of
    panicking when the u32 index space runs out. The strings of the batch
    inserted before the one that did not fit stay inserted.
    */
    pub fn try_insert_many<T>(&self, strs: &[T]) -> StringStoreResult<Vec<u32>>
    where
        T: AsRef<str>,
    {
        let mut indices: Vec<u32> = Vec::with_capacity(strs.len());
        // (position in `strs`, index key) of the strings that need an insert
        let mut missing: Vec<(usize, HashKey)> = Vec::new();
        for (pos, s) in strs.iter().enumerate() {
            match self.find_or_hash(s.as_ref()) {
                Resolved::Found(idx) => indices.push(idx),
                Resolved::Missing(key) => {
                    indices.push(0); // placeholder, filled in below
                    missing.push((pos, key));
                }
            }
        }
        if !missing.is_empty() {
            let mut arena = self.0.store.lock();
            for (pos, key) in missing {
                indices[pos] = self.insert_locked(&mut arena, strs[pos].as_ref(), key)?;
            }
        }
        Ok(indices)
    }

    /// The [StoredStr] reference of a stored string slice, if it exists.
    #[inline]
    pub fn get_ref(&'_ self, s: &str) -> Option<StoredStr<'_>> {
        self.idx(s).map(|idx: u32| StoredStr(idx, self))
    }

    /**
    Insert a new string (slice) and return its [StoredStr] reference.

    If the string (slice) already exists, return its reference instead.
    */
    pub fn insert_or_get<T>(&'_ self, s: T) -> StoredStr<'_>
    where
        T: AsRef<str>,
    {
        /*
        Delegating to `insert` gets us the collision check for free and
        avoids the previous contains() -> insert_unchecked() -> get_ref()
        dance (which cloned the string and skipped verification when the
        hash was already present).
        */
        StoredStr(self.insert(s), self)
    }

    /// Store the parts and return their indices.
    fn store_parts(&self, s: &str, delim: &str) -> Vec<u32> {
        let mut result: Vec<u32> = Vec::new();
        let parts: Split<&str> = s.split(delim);
        for part in parts {
            if part.is_empty() {
                // empty string here means one of the following:
                // - 2 contiguous delimiters
                // - delimiter at the start or end of the string
                result.push(0);
            } else {
                result.push(self.insert(part));
            }
        }
        result
    }

    /**
    Splits a string by a delimiter, stores each part and the delimiter,
    and returns a [Vec] of part indices in the same order, plus the delimiter
    index separately.

    The index 0 (empty string) in the returned Vec means:
    - at start/end: delimiter found at start/end of the string
    - elsewhere: 2 contiguous delimiters (or more with subsequent zero indices)
    */
    pub fn split_and_store(&self, s: &str, delim: &str) -> (Vec<u32>, u32) {
        if s.is_empty() {
            // special case: empty string
            return (vec![0], self.insert(delim));
        }

        // Store the delimiter first
        let delim_idx: u32 = match delim.is_empty() {
            true => 0,
            false => self.insert(delim),
        };

        (self.store_parts(s, delim), delim_idx)
    }

    /**
    Splits a given string into multiple parts based on multiple delimiters
    and stores each part, returning their indices in the storage, along with
    the storage indices of the provided delimiters.

    First, the delimiters provided in `delims` are stored, then the string
    `s` is split based on these delimiters by the byte-class scanner (see
    `doc/design/tokenization.md`) and each part is stored and its index
    returned. The parts are interned straight from slices of `s`, and a
    delimiter token reuses the index its delimiter got above; no
    intermediate [Token]s are allocated.

    ## Arguments
    * `s` - a string slice to be atomized
    * `delims` - string slices, based on which `s` shall be split
    * `force_regex` - **ignored.** It used to select between a linear and a
      regex-based tokenizer; there is one scanner now and it outperforms
      both at every input size measured. The parameter is retained so the
      signature stays stable.

    ## Returns
    A tuple of two [Vec]s:
    - first one contains the indices of the parts of `s` (including delims!)
    - second one contains the indices of the delimiters themselves

    ## Special Cases
    - If `s` is empty, index `0` is returned, along with the delimiter indices.
    - If `delims` is empty, the function returns a Vec of the index of `s`
      itself (assuming `s` is not empty), and an empty Vec for delimiters.

    Matching is leftmost-first with the earliest delimiter in `delims`
    winning at a given position, exactly like `tokenize`.
    */
    pub fn split_and_store_multi(
        &self,
        s: &str,
        delims: &[&str],
        force_regex: Option<bool>,
    ) -> (Vec<u32>, Vec<u32>) {
        let _ = force_regex; // see the doc comment: no-op since unification
        let mut result: Vec<u32> = vec![];
        let mut delim_indices: Vec<u32> = vec![];

        if !delims.is_empty() {
            // store the delimiters first
            for delim in delims {
                if delim.is_empty() {
                    delim_indices.push(0);
                } else {
                    delim_indices.push(self.insert(*delim));
                }
            }
        }

        if s.is_empty() {
            // special case: empty string
            return (vec![0], delim_indices);
        } else if delims.is_empty() {
            // special case: no delimiters
            return (vec![self.insert(s)], vec![]);
        }

        // a delimiter token is exactly `delims[d]`, already interned above
        scan_tokens(s, delims, |token: &str, delim: Option<usize>| {
            result.push(match delim {
                Some(d) => delim_indices[d],
                None => self.insert(token),
            })
        });

        (result, delim_indices)
    }

    /**
    Insert a new string (which can be coerced into a [Path]) and return
    a [Vec] of parts' indices. The delimiter is assumed to be a '/'.

    NOTE: the path will be normalized before storing, hence the result may
    not be the same as the input if it contains relative paths, escape
    sequences or control characters.

    NOTE: non-unicode sequences will be replaced with the replacement
    character [`U+FFFD REPLACEMENT CHARACTER`][U+FFFD].

    NOTE: if `index[0] == 0` && `index.len() > 1`, it means that the path is
    absolute and starts with "delimiter", in this case the forward slash.
    Especially, for the root path ('/'), the resultant Vec is `[0, 0]`.

    [U+FFFD]: core::char::REPLACEMENT_CHARACTER
    */
    pub fn store_path<P>(&self, s: P) -> Vec<u32>
    where
        P: AsRef<Path>,
    {
        let s: PathBuf = normalize_path(s, false);
        if s.as_os_str().is_empty() {
            return [0].into();
        }
        // we can unwrap safely, as the path is guaranteed to be valid
        self.store_parts(s.to_str().unwrap(), PATH_SEP)
    }

    /**
    Reconstruct a string from stored parts' and delimiter indices.
    Returns an error if any index is out of bounds.

    The same effect can be achieved by something like:
    ```ignore
    let built: String = indices
        .iter()
        .map(|&idx| store.get(idx).unwrap())
        .collect::<Vec<&str>>()
        .join(store.get(delim).unwrap());
    */
    pub fn reconstruct(&self, indices: &[u32], delim: u32) -> StringStoreResult<String> {
        let parts_num: usize = indices.len();
        // special case: empty string
        if parts_num == 0 || (parts_num == 1 && indices[0] == 0) {
            return Ok(EMPTY_STR.to_string());
        } else if parts_num > u32::MAX as usize {
            return Err(StringStoreError::ReconstructionTooLarge {
                requested: parts_num,
                max: u32::MAX as usize,
            });
        }

        // delimiter check
        let Some(delim_ptr) = self.lookup(delim) else {
            return Err(StringStoreError::IndexOutOfBounds {
                idx: delim,
                max: self.len() - 1,
            });
        };
        // SAFETY: `lookup` only returns pointers to stored strings
        let delim_str: &str = unsafe { &*delim_ptr };

        /*
        Validate every index and size the output in one pass, so the
        build pass below never reallocates. No lock: the store only grows,
        so an index valid here is still valid below. Index 0 is the empty
        string, so it needs no special casing here or below.
        */
        let mut total: usize = delim_str.len() * (parts_num - 1);
        for (i, &idx) in indices.iter().enumerate() {
            match self.lookup(idx) {
                Some(part) => total += unsafe { &*part }.len(),
                // `max` is the highest valid index, like `IndexOutOfBounds`
                None => return Err(StringStoreError::reconstruction(idx, i, self.len() as u32 - 1)),
            }
        }

        // construct the string
        let mut result: String = String::with_capacity(total);
        for (i, &idx) in indices.iter().enumerate() {
            let part: *const str = self.lookup(idx).expect("validated above");
            result.push_str(unsafe { &*part });
            if i < parts_num - 1 {
                // no delimiter after the last part
                result.push_str(delim_str);
            }
        }

        Ok(result)
    }

    /**
    Validate the contents of the store and index.

    ### Release mode
    Returns a list of errors if any are found.

    ### Debug mode
    Panics with the error list if any are found.
    */
    pub fn validate_contents(&self) -> Result<(), Vec<String>> {
        // the mutex keeps inserts out, so `len` and the index hold still
        let _arena = self.0.store.lock();
        let len: usize = self.len();
        let l_store: usize = len - LATIN1_NUM as usize;
        let mut errs: Vec<String> = Vec::new();

        let l_index: usize = self.0.index.len();
        if l_store != l_index {
            errs.push(format!("stored strings ({l_store}) != index.len() ({l_index})"));
        };

        // a slot by internal index, or None past the stored length
        // SAFETY: every slot below `len` is written; the mutex orders us after it
        let stored = |sid: usize| -> Option<&str> {
            (sid < l_store).then(|| unsafe { &*self.0.slots.read(sid as u32) })
        };

        // Check that each store entry has a corresponding index.
        for sid in 0..l_store {
            let s: &str = stored(sid).expect("below the stored length");
            let key: HashKey = hash_key(s.as_bytes());
            match self.0.index.get(&key).map(|r| *r.value()) {
                None => errs.push(format!("missing hash: 0x{key:x} (str_id: {sid}, str: '{s}')")),
                Some(found) if found != sid as u32 => {
                    // a corrupt index value may also be out of bounds; this
                    // must be reported, not panic the validation itself
                    let other: &str = stored(found as usize).unwrap_or("<out of bounds>");
                    errs.push(format!(
                        "index mismatch for str_id {sid} ('{s}'): hash 0x{key:x} -> {found} ('{other}')"
                    ));
                }
                Some(_) => {}
            }
        }

        // Check that each index is valid wrt. the store.
        for itm in self.0.index.iter() {
            let (key, sid) = itm.pair();
            let s: &str = match stored(*sid as usize) {
                Some(s) => s,
                None => {
                    errs.push(format!("index out of bounds: {sid} >= {l_store} (hash: 0x{key:x})"));
                    continue;
                }
            };
            let csum: HashKey = hash_key(s.as_bytes());
            if csum != *key {
                errs.push(format!(
                    "hash mismatch for '{s}' (stored: 0x{key:x}, calculated: 0x{csum:x})"
                ));
            }
        }

        #[cfg(debug_assertions)]
        if !errs.is_empty() {
            panic!("UniqueStrStore validation failed:\n{}", errs.join("\n"));
        }

        if !errs.is_empty() {
            return Err(errs);
        }
        Ok(())
    }
}

// We have to implement our own since `size_of::SizeOf` does not support
// `Mutex` nor `DashMap`.
#[cfg(feature = "size_of")]
impl SizeOf for UniqueStrStore {
    fn size_of_children(&self, context: &mut Context) {
        self.0.store.lock().size_of_children(context);
        self.0.slots.size_of_children(self.len() - LATIN1_NUM as usize, context);
        self.0.ascii.size_of_children(context);

        if self.0.index.capacity() > 0 {
            // key + value + RwLock
            let entry: usize = size_of::<HashKey>() + size_of::<u32>() + 8;
            let used: usize = entry * self.0.index.len();
            let total: usize = entry * self.0.index.capacity();
            context
                .add(used)
                .add_excess(total - used)
                .add_distinct_allocation();

            self.0.index.iter().for_each(|itm| {
                itm.key().size_of_children(context);
                itm.value().size_of_children(context);
            });
        };

        self.0.index.hasher().size_of_children(context);
    }
}

/* ######################################################################### */

/**
Pointer to a string slice living in a [UniqueStrStore]. This is the return
type of [StoredStr::as_ptr].

NOTE: this pointer is only valid as long as the store is alive. The lifetime
is not enforced, as the store is expected to outlive any references to its
contents. This is the responsibility of the user of this struct to enforce.
*/
#[derive(Clone, Eq)]
pub struct StoredStrPtr(*const str);

impl StoredStrPtr {
    #[inline]
    pub fn as_str(&self) -> &str {
        unsafe { &*self.0 }
    }
}

/* --------------------------------- */

impl AsRef<str> for StoredStrPtr {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl Deref for StoredStrPtr {
    type Target = *const str;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

/* --------------------------------- */

/**
Equality is by *content*, consistent with `Ord` and `Hash` (the previously
derived impl compared pointer address + length, which disagreed with
`cmp() == Equal` for equal strings living in two different stores).
Within one store interning guarantees equal content <=> equal pointer,
so the pointer comparison is a cheap fast path, not a semantic.
*/
impl PartialEq for StoredStrPtr {
    fn eq(&self, other: &Self) -> bool {
        ptr::eq(self.0, other.0) || self.as_str() == other.as_str()
    }
}

impl PartialOrd for StoredStrPtr {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for StoredStrPtr {
    fn cmp(&self, other: &Self) -> Ordering {
        self.as_str().cmp(other.as_str())
    }
}

impl Hash for StoredStrPtr {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.as_str().hash(state)
    }
}

/* --------------------------------- */

impl Debug for StoredStrPtr {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "StoredStrPtr({:?} -> {:?})", self.0, self.as_str())
    }
}

impl Display for StoredStrPtr {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

/* --------------------------------- */

impl PartialEq<StoredStrPtr> for &str {
    fn eq(&self, other: &StoredStrPtr) -> bool {
        *self == other.as_str()
    }
}

impl PartialEq<&str> for StoredStrPtr {
    fn eq(&self, other: &&str) -> bool {
        self.as_str() == *other
    }
}

/* --------------------------------- */

/**
Convert a [StoredStrPtr] into a plain string slice.

<b style="color:red">WARNING: the target lifetime `'a` is completely
unconstrained.</b> Safe code can use this impl to conjure a `&'static str`
(or any other lifetime) with no compile-time tie to the originating
[UniqueStrStore]. This is intentional: the store is meant to live for the
remainder of the program (effectively `'static`), so the pointer is assumed
to never dangle. If your store is **not** program-lifetime, do not use this
conversion — the resulting reference can outlive the storage, and using it
after the store is dropped is undefined behavior.
*/
impl<'a> From<StoredStrPtr> for &'a str {
    fn from(v: StoredStrPtr) -> &'a str {
        unsafe { &*v.0 }
    }
}

/* ######################################################################### */

/**
A reference (index) to a stored string slice in a [UniqueStrStore].

This is a self-contained version which has a reference back to the store,
which allows it to be used in place of a "normal" string slice.
*/
#[derive(Clone)]
pub struct StoredStr<'a>(u32, &'a UniqueStrStore);

impl<'a> StoredStr<'a> {
    /**
    Safe to call without a bounds check: a [StoredStr] can only be built
    from an index the store handed out (`get_ref` / `insert_or_get`), and
    the store is append-only, so the slot exists for as long as `'a`.
    Skipping the check saves a redundant length compare on every deref.
    */
    #[inline]
    fn reference(&self) -> &str {
        unsafe { &*self.1.get_str_ptr(self.0) }
    }

    #[inline]
    pub fn as_ptr(&self) -> StoredStrPtr {
        StoredStrPtr(unsafe { (*self.1).get_str_ptr(self.0) })
    }

    /// Get the index of the stored string slice.
    #[inline]
    pub fn idx(&self) -> u32 {
        self.0
    }

    /// Get the reference to the [UniqueStrStore] that contains this string.
    pub fn store(&self) -> &UniqueStrStore {
        self.1
    }

    pub fn cloned(&self) -> String {
        self.reference().to_string()
    }
}

/* --------------------------------- */

impl<'a> AsRef<str> for StoredStr<'a> {
    #[inline]
    fn as_ref(&self) -> &str {
        self.reference()
    }
}

impl<'a> Deref for StoredStr<'a> {
    type Target = str;

    #[inline]
    fn deref(&self) -> &Self::Target {
        self.reference()
    }
}

/* --------------------------------- */

impl<'a> Eq for StoredStr<'a> {}

impl<'a> PartialEq for StoredStr<'a> {
    #[inline]
    fn eq(&self, other: &Self) -> bool {
        /*
        Same underlying store: interning guarantees equal content <=>
        equal index, so the indices decide without touching the lock.
        Two clones of one store share the `Arc`, hence `Arc::ptr_eq`
        rather than comparing the `&UniqueStrStore` addresses.
        */
        if Arc::ptr_eq(&self.1 .0, &other.1 .0) {
            return self.0 == other.0;
        }
        self.reference() == other.reference()
    }
}

impl<'a> PartialOrd for StoredStr<'a> {
    #[inline]
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl<'a> Ord for StoredStr<'a> {
    #[inline]
    fn cmp(&self, other: &Self) -> Ordering {
        self.reference().cmp(other.reference())
    }
}

impl<'a> Hash for StoredStr<'a> {
    #[inline]
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.reference().hash(state)
    }
}

/* --------------------------------- */

impl<'a> Debug for StoredStr<'a> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "StoredStr({}: {:?})", self.0, self.reference())
    }
}

impl<'a> Display for StoredStr<'a> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.reference())
    }
}

/* --------------------------------- */

impl<'a> PartialEq<StoredStr<'a>> for &str {
    #[inline]
    fn eq(&self, other: &StoredStr) -> bool {
        *self == other.reference()
    }
}

impl<'a> PartialEq<&str> for StoredStr<'a> {
    #[inline]
    fn eq(&self, other: &&str) -> bool {
        self.reference() == *other
    }
}

/* --------------------------------- */

// Implement `From` for converting `StoredStr` into `u32`.
impl<'a> From<StoredStr<'a>> for u32 {
    #[inline]
    fn from(v: StoredStr) -> u32 {
        v.0
    }
}

impl<'a> From<StoredStr<'a>> for &'a str {
    #[inline]
    fn from(v: StoredStr<'a>) -> &'a str {
        // see `reference` for why the unchecked lookup is sound
        unsafe { &*v.1.get_str_ptr(v.0) }
    }
}

/* ######################################################################### */

/*
NOTE: the dormant structured-text scaffolding below carries scoped
`#[expect(dead_code)]` attributes instead of a crate-wide allow. The
moment an item gets wired up, its attribute reports itself as an
"unfulfilled lint expectation" — remove it then. Items that the tests
already exercise (`StructuredLine` and the `with_store` formatting
helpers) use `#[cfg_attr(not(test), expect(dead_code))]` so the
expectation holds in both build configurations. The type definitions
themselves need no attribute: rustc treats the `expect`-annotated impls
and `TextElement` as live roots, which keeps the types they reference
transitively live.
*/

/**
A value from the structured-text scaffolding paired with the
[UniqueStrStore] its indices refer to, so that it can be formatted.

`Display` recreates the text; `Debug` prints the index *and* the text
(`CompactStr(256: "foo")`), which is what makes a dumped [StructuredLine]
legible. Obtained through the `with_store` method of [CompactStr],
[Character] and [TextElement]; [StructuredLine] carries its own store and
implements both traits directly.
*/
#[cfg_attr(not(test), expect(dead_code))]
struct WithStore<'a, T: ?Sized>(&'a T, &'a UniqueStrStore);

/**
A reference (index) to a stored string slice in a [UniqueStrStore].

This is a compact version which lacks a reference back to the containing store,
hence it is only usable as a part of a larger structure with a reference.
*/
#[derive(Debug, Clone, PartialEq, Eq)]
struct CompactStr(u32);

#[cfg_attr(not(test), expect(dead_code))]
impl CompactStr {
    /// Pair with `store` for `Display` / `Debug`; see [WithStore].
    fn with_store<'a>(&'a self, store: &'a UniqueStrStore) -> WithStore<'a, Self> {
        WithStore(self, store)
    }
}

impl Display for WithStore<'_, CompactStr> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.write_str(self.0.get(self.1))
    }
}

impl Debug for WithStore<'_, CompactStr> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "CompactStr({}: {:?})", self.0.idx(), self.0.get(self.1))
    }
}

#[expect(dead_code)]
impl CompactStr {
    #[inline]
    fn idx(&self) -> u32 {
        self.0
    }

    fn get<'a>(&self, store: &'a UniqueStrStore) -> &'a str {
        unsafe { store.borrow_str(self.0) }
    }

    fn to_string(&self, store: &UniqueStrStore) -> String {
        self.get(store).to_string()
    }
}

/// This is a single or repeated character stored in a [UniqueStrStore].
#[derive(Debug, Clone, PartialEq, Eq)]
struct Character(CompactStr, u8);

#[cfg_attr(not(test), expect(dead_code))]
impl Character {
    /// Pair with `store` for `Display` / `Debug`; see [WithStore].
    fn with_store<'a>(&'a self, store: &'a UniqueStrStore) -> WithStore<'a, Self> {
        WithStore(self, store)
    }
}

impl Display for WithStore<'_, Character> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        let s: &str = self.0.get(self.1);
        for _ in 0..self.0.num() {
            f.write_str(s)?;
        }
        Ok(())
    }
}

impl Debug for WithStore<'_, Character> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "Character({}: {:?} x{})", self.0.idx(), self.0.get(self.1), self.0.num())
    }
}

#[expect(dead_code)]
impl Character {
    #[inline]
    fn idx(&self) -> u32 {
        self.0 .0
    }

    #[inline]
    fn num(&self) -> u8 {
        self.1
    }

    fn get<'a>(&self, store: &'a UniqueStrStore) -> &'a str {
        unsafe { store.borrow_str(self.idx()) }
    }

    fn to_string(&self, store: &UniqueStrStore) -> String {
        self.get(store).repeat(self.num() as usize)
    }
}

/* --------------------------------- */

/// Possible text elements in a structured line.
#[expect(dead_code)]
#[derive(Debug, PartialEq)]
enum TextElement<I: Integer = i64> {
    /// An element which is explicitly a delimiter, f.ex. space (`" "`).
    Delimiter(Character),
    /// A single (or repeated) character, f.ex. `*` or `***`.
    Char(Character),
    Word(CompactStr),
    /// A key-value pair, f.ex. `foo="bar"`.
    KeyVal(CompactStr, CompactStr),
    /// Integer type. Define like this (i64 is default):
    /// ```ignore
    /// let elem = TextElement::<i32>::Integer(42);
    Integer(I),
    Float(f64),
    /// A range of integers with "range" marker, f.ex. `-15..10` or `0-100`.
    Range(I, I, Character),
    /// A date in the format `YYYY-MM-DD`.
    Day(i16, u8, u8),
    /// A time in the format `HH:MM:SS`.
    Time(u8, u8, u8),
    /// A timestamp as seconds since the Unix epoch.
    Timestamp(SecondsSinceEpoch),
    /// An IPv4 or IPv6 address.
    IPAddress(IpAddr),
    /// A host name, f.ex. `www.example.org`. Usually a FQDN.
    Hostname(CompactStr),
    /// An username, f.ex. `john@workstation`.
    Username(CompactStr, CompactStr),
    /// An email address, f.ex. `john.doe@example.org`.
    Email(CompactStr, CompactStr),
    /// A hexadecimal number, f.ex. `0xdeadbeef` or `feedf00d`.
    HexStr(Hex, HexFormat),
    /// A sentence as a single element, f.ex. `Mary had a little lamb.`.
    Sentence(Vec<CompactStr>),
    /// A URL, f.ex. `https://www.example.org:8080/path/to/file.html`.
    URLStr(Vec<CompactStr>),
    /// URL query params, f.ex. `?foo=bar&baz=qux`. Question mark is implicit.
    URLParams(Vec<CompactStr>),
    /// Enclosed [TextElement] with a start and end delimiter.
    EnclosedElem(Box<TextElement>, CompactStr, CompactStr),
    /// Unprocessed text.
    RawText(String),
    // Maybe for future...?
    //UuidStr(Uuid),
    //PhoneNumber,
    //GeoCoordinate,
    //Duration,
}

#[cfg_attr(not(test), expect(dead_code))]
impl<I: Integer> TextElement<I> {
    /// Pair with `store` for `Display` / `Debug`; see [WithStore].
    fn with_store<'a>(&'a self, store: &'a UniqueStrStore) -> WithStore<'a, Self> {
        WithStore(self, store)
    }
}

/**
Recreates the element's text. This is a *canonical* rendering: the sketch
does not record the original spelling of composite elements (the `=` of a
`KeyVal`, the whitespace inside a `Sentence`, number formatting), so the
result reads correctly but is not guaranteed byte-identical to the input.
*/
impl<I: Integer> Display for WithStore<'_, TextElement<I>> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        let store: &UniqueStrStore = self.1;
        // write a list of stored strings with `sep` between them
        let join = |f: &mut Formatter<'_>, parts: &[CompactStr], sep: &str| -> fmt::Result {
            for (i, part) in parts.iter().enumerate() {
                if i > 0 {
                    f.write_str(sep)?;
                }
                f.write_str(part.get(store))?;
            }
            Ok(())
        };

        match self.0 {
            TextElement::Delimiter(c) | TextElement::Char(c) => write!(f, "{}", c.with_store(store)),
            TextElement::Word(w) | TextElement::Hostname(w) => f.write_str(w.get(store)),
            TextElement::KeyVal(k, v) => write!(f, "{}={}", k.get(store), v.get(store)),
            TextElement::Integer(i) => write!(f, "{i}"),
            TextElement::Float(x) => write!(f, "{x}"),
            TextElement::Range(a, b, marker) => write!(f, "{a}{}{b}", marker.with_store(store)),
            TextElement::Day(y, m, d) => write!(f, "{y:04}-{m:02}-{d:02}"),
            TextElement::Time(h, m, sec) => write!(f, "{h:02}:{m:02}:{sec:02}"),
            TextElement::Timestamp(t) => write!(f, "{t}"),
            TextElement::IPAddress(ip) => write!(f, "{ip}"),
            TextElement::Username(u, host) => write!(f, "{}@{}", u.get(store), host.get(store)),
            TextElement::Email(u, domain) => write!(f, "{}@{}", u.get(store), domain.get(store)),
            TextElement::HexStr(hex, fmt) => f.write_str(&Hex::to_string(*hex, *fmt)),
            TextElement::Sentence(words) => join(f, words, " "),
            TextElement::URLStr(parts) => join(f, parts, ""),
            TextElement::URLParams(params) => {
                f.write_str("?")?;
                join(f, params, "&")
            }
            TextElement::EnclosedElem(inner, open, close) => {
                write!(f, "{}{}{}", open.get(store), inner.with_store(store), close.get(store))
            }
            TextElement::RawText(s) => f.write_str(s),
        }
    }
}

/// Like the derived `Debug`, but every stored-string field shows its
/// text next to its index (`Word(CompactStr(256: "foo"))`).
impl<I: Integer> Debug for WithStore<'_, TextElement<I>> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        let store: &UniqueStrStore = self.1;
        // a Vec<CompactStr> field, resolved
        let list = |parts: &[CompactStr]| -> Vec<String> {
            parts.iter().map(|p| format!("{:?}", p.with_store(store))).collect()
        };

        match self.0 {
            TextElement::Delimiter(c) => f.debug_tuple("Delimiter").field(&c.with_store(store)).finish(),
            TextElement::Char(c) => f.debug_tuple("Char").field(&c.with_store(store)).finish(),
            TextElement::Word(w) => f.debug_tuple("Word").field(&w.with_store(store)).finish(),
            TextElement::KeyVal(k, v) => f
                .debug_tuple("KeyVal")
                .field(&k.with_store(store))
                .field(&v.with_store(store))
                .finish(),
            TextElement::Integer(i) => f.debug_tuple("Integer").field(i).finish(),
            TextElement::Float(x) => f.debug_tuple("Float").field(x).finish(),
            TextElement::Range(a, b, marker) => f
                .debug_tuple("Range")
                .field(a)
                .field(b)
                .field(&marker.with_store(store))
                .finish(),
            TextElement::Day(y, m, d) => f.debug_tuple("Day").field(y).field(m).field(d).finish(),
            TextElement::Time(h, m, sec) => f.debug_tuple("Time").field(h).field(m).field(sec).finish(),
            TextElement::Timestamp(t) => f.debug_tuple("Timestamp").field(&format_args!("{t}")).finish(),
            TextElement::IPAddress(ip) => f.debug_tuple("IPAddress").field(ip).finish(),
            TextElement::Hostname(h) => f.debug_tuple("Hostname").field(&h.with_store(store)).finish(),
            TextElement::Username(u, host) => f
                .debug_tuple("Username")
                .field(&u.with_store(store))
                .field(&host.with_store(store))
                .finish(),
            TextElement::Email(u, domain) => f
                .debug_tuple("Email")
                .field(&u.with_store(store))
                .field(&domain.with_store(store))
                .finish(),
            TextElement::HexStr(hex, fmt) => f.debug_tuple("HexStr").field(hex).field(fmt).finish(),
            TextElement::Sentence(words) => f.debug_tuple("Sentence").field(&list(words)).finish(),
            TextElement::URLStr(parts) => f.debug_tuple("URLStr").field(&list(parts)).finish(),
            TextElement::URLParams(params) => f.debug_tuple("URLParams").field(&list(params)).finish(),
            TextElement::EnclosedElem(inner, open, close) => f
                .debug_tuple("EnclosedElem")
                .field(&inner.with_store(store))
                .field(&open.with_store(store))
                .field(&close.with_store(store))
                .finish(),
            TextElement::RawText(s) => f.debug_tuple("RawText").field(s).finish(),
        }
    }
}

/* --------------------------------- */

/// A unit of structured text, which can be a line or a block.
/// Contains a reference to the [UniqueStrStore] for string retrieval.
#[cfg_attr(not(test), expect(dead_code))]
struct StructuredLine {
    elems: Vec<TextElement>,
    store: Arc<UniqueStrStore>,
}

#[cfg_attr(not(test), expect(dead_code))]
impl StructuredLine {
    fn new(store: &Arc<UniqueStrStore>) -> Self {
        Self {
            elems: Vec::new(),
            store: store.clone(),
        }
    }

    fn len(&self) -> usize {
        self.elems.len()
    }

    fn push(&mut self, elem: TextElement) {
        self.elems.push(elem);
    }
}

impl PartialEq for StructuredLine {
    fn eq(&self, other: &Self) -> bool {
        self.elems == other.elems
    }
}

/// Recreates the line by concatenating its elements (canonical rendering,
/// see [TextElement]'s `Display`).
impl Display for StructuredLine {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        for elem in &self.elems {
            write!(f, "{}", elem.with_store(&self.store))?;
        }
        Ok(())
    }
}

/// Lists the elements with their strings resolved; the store itself is
/// not printed (it may hold millions of strings).
impl Debug for StructuredLine {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.write_str("StructuredLine ")?;
        f.debug_list()
            .entries(self.elems.iter().map(|e| e.with_store(&self.store)))
            .finish()
    }
}

/* --------------------------------- */

/// A representation of a hexadecimal number.
#[derive(Clone, Copy, PartialEq)]
struct Hex(u64);

#[expect(dead_code)]
impl Hex {
    fn get(&self) -> u64 {
        self.0
    }

    fn to_string(self, fmt: HexFormat) -> String {
        // Order matters: the "0x" prefix must be applied last so that it is
        // neither uppercased ("0X...") nor counted into the column grouping.
        let mut result: String = format!("{:x}", self.0);
        if fmt.is_upper() {
            result = result.to_uppercase();
        }
        if fmt.is_columns() {
            // group by 4 digits counting from the *right*, like a numeric
            // separator: 0xfeedf00d5 -> "f:eedf:00d5"
            let n: usize = result.len();
            result = result
                .chars()
                .enumerate()
                .map(|(i, c)| {
                    if i != 0 && (n - i).is_multiple_of(4) {
                        format!(":{c}")
                    } else {
                        c.to_string()
                    }
                })
                .collect();
        }
        if fmt.is_prefix() {
            result = format!("0x{}", result);
        }
        result
    }
}

impl Debug for Hex {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "Hex({:x})", self.0)
    }
}

impl Display for Hex {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "{:x}", self.0)
    }
}

/* --------------------------------- */

/// Bitmap of options for hex string display.
#[derive(Clone, Copy, PartialEq)]
pub struct HexFormat(u8);

impl HexFormat {
    pub const PLAIN: Self = Self(0b0);
    pub const UPPER: Self = Self(0b1);
    pub const PREFIX: Self = Self(0b10);
    pub const COLUMNS: Self = Self(0b100);

    /// Whether the hex string is uppercase.
    pub fn is_upper(&self) -> bool {
        self.0 & Self::UPPER.0 != 0
    }
    /// Whether the "0x" prefix should be shown.
    pub fn is_prefix(&self) -> bool {
        self.0 & Self::PREFIX.0 != 0
    }
    /// Whether the parts are divided by columns (":").
    pub fn is_columns(&self) -> bool {
        self.0 & Self::COLUMNS.0 != 0
    }

}

#[rustfmt::skip]
impl Display for HexFormat {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        if *self == Self::PLAIN {
            return write!(f, "Plain");
        }

        let mut parts: Vec<&str> = Vec::new();
        if self.is_upper() { parts.push("Upper"); }
        if self.is_prefix() { parts.push("Prefix"); }
        if self.is_columns() { parts.push("Columns"); }
        if parts.is_empty() {
            // non-zero but no known flag bits set
            return write!(f, "Unknown(0b{:b})", self.0);
        }
        write!(f, "{}", parts.join("|"))
    }
}

impl Debug for HexFormat {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "HexFmt({}: {})", self.0, self)
    }
}

/* --------------------------------- */

#[expect(dead_code)]
trait Integer: FromStr + Display + Debug + Copy + PartialOrd + Send + Sync + 'static {
    fn as_i64(&self) -> i64;
    fn as_u64(&self) -> u64;
}

macro_rules! impl_integer {
    ($($t:ty),*) => {
        $(
            impl Integer for $t {
                fn as_i64(&self) -> i64 {
                    *self as i64
                }
                fn as_u64(&self) -> u64 {
                    *self as u64
                }
            }
        )*
    }
}

impl_integer!(i8, i16, i32, i64, i128, isize, u8, u16, u32, u64, u128, usize);

/* ############################# TOKENIZATION ############################## */

#[derive(Default, Debug, PartialEq, Eq)]
/// A token (part) of a delimited string, which has been processed (tokenized).
/// It can be a delimiter, or a regular part.
pub struct Token {
    content: String,
    is_delim: bool,           // default: false
    delim_idx: Option<usize>, // default: None
}

impl Token {
    /// The text content of this token.
    #[inline]
    pub fn content(&self) -> &str {
        &self.content
    }

    /// Whether this token matched one of the delimiters.
    #[inline]
    pub fn is_delim(&self) -> bool {
        self.is_delim
    }

    /// The position of the matched delimiter in the original `delims` slice
    /// (None for non-delimiter tokens).
    #[inline]
    pub fn delim_idx(&self) -> Option<usize> {
        self.delim_idx
    }
}

/// The token's text, delimiter or not.
impl Display for Token {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.write_str(&self.content)
    }
}

/// Classification of a byte for the scanner's 256-entry lookup table.
#[derive(Clone, Copy, PartialEq, Eq)]
enum ByteClass {
    /// Nothing to check here; the scanner just advances.
    Plain,
    /// At least one non-empty delimiter starts with this byte.
    DelimStart,
}

/**
The scanner's per-byte lookup table. Every byte of the input is looked up
here first, and only a `DelimStart` byte triggers the delimiter comparison
loop — so for typical text the per-byte cost is one table load.

This is the extension point for richer tokenization later on (character
class delimiters, repeated-character runs, enclosures that suppress
splitting): each becomes another [ByteClass] variant and a branch in
`scan_tokens`, without changing the entry points.
*/
struct ScanTable([ByteClass; 256]);

impl ScanTable {
    /// Build the table for `delims`; `None` if no delimiter is usable
    /// (all empty or none given), in which case nothing can ever match.
    fn new(delims: &[&str]) -> Option<Self> {
        let mut table: [ByteClass; 256] = [ByteClass::Plain; 256];
        let mut usable: bool = false;
        for d in delims {
            if let Some(&b) = d.as_bytes().first() {
                table[b as usize] = ByteClass::DelimStart;
                usable = true;
            }
        }
        usable.then_some(Self(table))
    }

    #[inline]
    fn class(&self, b: u8) -> ByteClass {
        self.0[b as usize]
    }
}

/**
The scanner core behind every tokenizing entry point. Calls `f` once per
token in order, with the token's slice of `s` and, for a delimiter, the
position of the matched delimiter in `delims`. Non-delimiter tokens are
never empty.

Matching is leftmost-first: at the leftmost position where any delimiter
matches, the one that appears earliest in `delims` wins, and scanning
resumes after it. Empty delimiters never match.

## Why stepping byte-wise is safe on UTF-8

A delimiter is valid UTF-8, so its first byte is never a continuation
byte (`0x80..=0xBF`) and the table marks no continuation byte as
`DelimStart`. A match can therefore only start on a char boundary, and
since a delimiter consists of whole chars it also ends on one, so every
slice taken below is on char boundaries. Bytes that do not start a match
are simply skipped, whether or not they are inside a multibyte char.
*/
fn scan_tokens<'a, F>(s: &'a str, delims: &[&str], mut f: F)
where
    F: FnMut(&'a str, Option<usize>),
{
    let Some(table) = ScanTable::new(delims) else {
        // no usable delimiters: the whole input is one token (or nothing)
        if !s.is_empty() {
            f(s, None);
        }
        return;
    };

    let bytes: &[u8] = s.as_bytes();
    let mut i: usize = 0; // scan cursor
    let mut start: usize = 0; // start of the pending non-delimiter token
    while i < bytes.len() {
        if table.class(bytes[i]) == ByteClass::DelimStart {
            let hit = delims
                .iter()
                .enumerate()
                .find(|(_, d)| !d.is_empty() && bytes[i..].starts_with(d.as_bytes()));
            if let Some((d_idx, d)) = hit {
                if start < i {
                    f(&s[start..i], None);
                }
                let end: usize = i + d.len();
                f(&s[i..end], Some(d_idx));
                i = end;
                start = end;
                continue;
            }
        }
        i += 1;
    }

    if start < bytes.len() {
        f(&s[start..], None);
    }
}

/**
Tokenize a string by a set of delimiters and return a [Vec] of [Token]s.
The delimiters are included as separate tokens.

Matching is leftmost-first with the earliest delimiter in `delims`
winning at a given position; see `scan_tokens` and
`doc/design/tokenization.md`. Empty delimiters are skipped without
renumbering the others.
*/
pub fn tokenize(s: &str, delims: &[&str]) -> Vec<Token> {
    let mut tokens: Vec<Token> = Vec::new();
    scan_tokens(s, delims, |content: &str, delim_idx: Option<usize>| {
        tokens.push(Token {
            content: content.to_string(),
            is_delim: delim_idx.is_some(),
            delim_idx,
        })
    });
    tokens
}

/**
Identical to [tokenize]; retained for API compatibility.

This used to compile a regex alternation of the delimiters. The single
scanner now outperforms that at every input size measured (the regex
compile alone cost ~12 µs per call), so both names share one
implementation and always produce the same output.
*/
#[inline]
pub fn tokenize_regex(s: &str, delims: &[&str]) -> Vec<Token> {
    tokenize(s, delims)
}

/* ########################## UTILITY FUNCTIONS ############################ */

/// Hash string bytes into the index key; see [HashKey].
#[cfg(not(feature = "xxh128"))]
#[inline]
fn hash_key(bytes: &[u8]) -> HashKey {
    hash_bytes(bytes)
}

/// Hash string bytes into the index key; see [HashKey].
#[cfg(feature = "xxh128")]
#[inline]
fn hash_key(bytes: &[u8]) -> HashKey {
    xxh3_128(bytes)
}

/**
Confirm that the store slot a hash lookup returned really holds `s`.
With 64-bit keys a hit may be a genuine collision, which the hash-keyed
index cannot represent, so a mismatch panics. Not compiled with the
`xxh128` feature: there the hash is trusted, which is exactly what makes
the duplicate-insert path lock-free.
*/
#[cfg(not(feature = "xxh128"))]
#[inline]
fn verify_hit(stored: &str, internal: u32, s: &str) {
    if stored != s {
        collision_panic(stored, s, internal);
    }
}

/**
A genuine xxh3 collision between two distinct strings. The index is keyed
by hash alone, so the store cannot represent both — and silently returning
the other string's index would corrupt every downstream user.
*/
#[cfg(not(feature = "xxh128"))]
#[cold]
#[inline(never)]
fn collision_panic(stored: &str, new: &str, internal: u32) -> ! {
    panic!(
        "xxh3 hash collision: new string '{new}' hashes identically to stored \
         string '{stored}' (internal index {internal}); UniqueStrStore cannot \
         represent both"
    );
}

/**
Check whether a string consists of exactly one ISO-8859-1 codepoint,
and if so, return it. Otherwise (incl. empty string), return None.
Note: codepoints 128-255 are *two* bytes in UTF-8, so callers must not
pre-filter on byte length == 1.

NUL (`'\0'`, codepoint 0) is deliberately excluded: index 0 holds the
empty string, not NUL, so a NUL string must go through the regular
hash-indexed path like any other content.
*/
#[inline]
fn return_iso8859_1_cp(s: &str) -> Option<u32> {
    let mut chars = s.chars();
    let c: u32 = chars.next()? as u32;
    if c != 0 && c < LATIN1_NUM && chars.next().is_none() {
        return Some(c);
    }
    None
}

/* ############################# ERROR HANDLING ############################ */

/// Error type for string store operations.
#[derive(Debug, Clone, PartialEq)]
pub enum StringStoreError {
    /// Index out of bounds error. Contains the invalid index and max index.
    IndexOutOfBounds { idx: u32, max: usize },
    /// Error when the store has reached its maximum capacity (u32::MAX).
    StoreFull,
    /// Error when attempting to reconstruct a string with invalid parts.
    /// Contains the offending index, its position in the input slice,
    /// and the highest valid index.
    InvalidReconstruction { idx: u32, pos: usize, max: u32 },
    /// Error when string reconstruction would exceed the maximum allowed size.
    ReconstructionTooLarge { requested: usize, max: usize },
    /// Error when a string contains invalid UTF-8 sequences.
    InvalidUtf8(String),
    /// Error when path contains invalid characters or sequences.
    InvalidPath(String),
    /// Error when delimiter is invalid (e.g., empty when not allowed).
    InvalidDelimiter(String),
    /// Internal error, used when invariants are violated. Should never happen normally.
    InternalError(String),
}

impl Error for StringStoreError {}

impl Display for StringStoreError {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::IndexOutOfBounds { idx, max } => {
                write!(f, "index {idx} out of bounds (max: {max})")
            }
            Self::StoreFull => write!(f, "string store has reached maximum capacity"),
            Self::InvalidReconstruction { idx, pos, max } => {
                write!(f, "invalid index {idx} at position {pos} (max: {max})")
            }
            Self::ReconstructionTooLarge { requested, max } => {
                write!(f, "too many indexes to reconstruct (requested: {requested}, max: {max})")
            }
            Self::InvalidUtf8(info) => write!(f, "invalid UTF-8 sequence: {info}"),
            Self::InvalidPath(info) => write!(f, "invalid path: {info}"),
            Self::InvalidDelimiter(info) => write!(f, "invalid delimiter: {info}"),
            Self::InternalError(info) => write!(f, "internal error: {info}"),
        }
    }
}

/// Type alias for Result with StringStoreError.
pub type StringStoreResult<T> = Result<T, StringStoreError>;

// Helper methods for creating errors
impl StringStoreError {
    /// Create a new IndexOutOfBounds error.
    pub fn oob(idx: u32, max: usize) -> Self {
        Self::IndexOutOfBounds { idx, max }
    }

    /// Create a new InvalidReconstruction error.
    pub fn reconstruction(idx: u32, pos: usize, max: u32) -> Self {
        Self::InvalidReconstruction { idx, pos, max }
    }

    /// Create a new InvalidPath error with details.
    pub fn path<S: Into<String>>(info: S) -> Self {
        Self::InvalidPath(info.into())
    }

    /// Create a new InvalidDelimiter error with details.
    pub fn delimiter<S: Into<String>>(info: S) -> Self {
        Self::InvalidDelimiter(info.into())
    }

    /// Create a new InternalError with details.
    pub fn internal_error<S: Into<String>>(info: S) -> Self {
        Self::InternalError(info.into())
    }
}

/* ################################ TESTS ################################## */

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        hint,
        panic::{catch_unwind, AssertUnwindSafe},
        thread,
    };

    const HELLO: &str = "Hello, world!";
    const CONC_S_NUM: usize = 100_000;
    const CONC_T_NUM: usize = 10;
    const TOKEN_TEST: &str = "apple,banana,cherry,cake,,cake";
    const TOKEN_DELIMS: [&str; 8] = [",", "cherry", " ", ",", ".", "!", "?", "\n"];
    const TOKENS_LEN: usize = 10;
    const TOKENS_EXPECTED: [&str; TOKENS_LEN] = [
        "apple", ",", "banana", ",", "cherry", ",", "cake", ",", ",", "cake",
    ];

    #[rustfmt::skip]
    #[test]
    fn test_tokenize() {
        let tokens: Vec<Token> = tokenize(TOKEN_TEST, &TOKEN_DELIMS);

        // Check that the length and tokenization is correct
        assert_eq!(tokens.len(), TOKENS_LEN, "len failed, tokens:\n{tokens:#?}");
        for (i, token) in tokens.iter().enumerate() {
            let exp: &str = TOKENS_EXPECTED[i];
            assert_eq!(token.content, exp, "token {i}: {token:?} (tokens: {tokens:#?})");
        }
    }

    #[rustfmt::skip]
    #[test]
    fn test_tokenize_regex() {
        let tokens: Vec<Token> = tokenize_regex(TOKEN_TEST, &TOKEN_DELIMS);

        assert_eq!(tokens.len(), TOKENS_LEN, "len failed, tokens:\n{tokens:#?}");
        for (i, token) in tokens.iter().enumerate() {
            let exp: &str = TOKENS_EXPECTED[i];
            assert_eq!(token.content, exp, "token {i}: {token:?}, tokens:\n{tokens:#?})");
        }

        // detailed check for the tokenization
        let t: [Token; 10] = [
            Token { content: "apple".to_string(),  is_delim: false, delim_idx: None },
            Token { content: ",".to_string(),      is_delim: true,  delim_idx: Some(0) },
            Token { content: "banana".to_string(), is_delim: false, delim_idx: None },
            Token { content: ",".to_string(),      is_delim: true,  delim_idx: Some(0) },
            Token { content: "cherry".to_string(), is_delim: true,  delim_idx: Some(1) },
            Token { content: ",".to_string(),      is_delim: true,  delim_idx: Some(0) },
            Token { content: "cake".to_string(),   is_delim: false, delim_idx: None },
            Token { content: ",".to_string(),      is_delim: true,  delim_idx: Some(0) },
            Token { content: ",".to_string(),      is_delim: true,  delim_idx: Some(0) },
            Token { content: "cake".to_string(),   is_delim: false, delim_idx: None }
            ];

        assert!(tokens == t, "tokens don't match expected:\n{tokens:#?}");
    }

    #[rustfmt::skip]
    #[test]
    fn test_tokenize_long() {
        let test: &str = "Mary had a little lamb, its fleece was white as snow.\n";
        let tokens: Vec<Token> = tokenize(test, &TOKEN_DELIMS);

        assert_eq!(tokens.len(), 24, "len failed, tokens:\n{tokens:#?}");

        let t: [Token; 24] = [
            Token { content: "Mary".to_string(),   is_delim: false, delim_idx: None },
            Token { content: " ".to_string(),      is_delim: true,  delim_idx: Some(2) },
            Token { content: "had".to_string(),    is_delim: false, delim_idx: None },
            Token { content: " ".to_string(),      is_delim: true,  delim_idx: Some(2) },
            Token { content: "a".to_string(),      is_delim: false, delim_idx: None },
            Token { content: " ".to_string(),      is_delim: true,  delim_idx: Some(2) },
            Token { content: "little".to_string(), is_delim: false, delim_idx: None },
            Token { content: " ".to_string(),      is_delim: true,  delim_idx: Some(2) },
            Token { content: "lamb".to_string(),   is_delim: false, delim_idx: None },
            Token { content: ",".to_string(),      is_delim: true,  delim_idx: Some(0) },
            Token { content: " ".to_string(),      is_delim: true,  delim_idx: Some(2) },
            Token { content: "its".to_string(),    is_delim: false, delim_idx: None },
            Token { content: " ".to_string(),      is_delim: true,  delim_idx: Some(2) },
            Token { content: "fleece".to_string(), is_delim: false, delim_idx: None },
            Token { content: " ".to_string(),      is_delim: true,  delim_idx: Some(2) },
            Token { content: "was".to_string(),    is_delim: false, delim_idx: None },
            Token { content: " ".to_string(),      is_delim: true,  delim_idx: Some(2) },
            Token { content: "white".to_string(),  is_delim: false, delim_idx: None },
            Token { content: " ".to_string(),      is_delim: true,  delim_idx: Some(2) },
            Token { content: "as".to_string(),     is_delim: false, delim_idx: None },
            Token { content: " ".to_string(),      is_delim: true,  delim_idx: Some(2) },
            Token { content: "snow".to_string(),   is_delim: false, delim_idx: None },
            Token { content: ".".to_string(),      is_delim: true,  delim_idx: Some(4) },
            Token { content: "\n".to_string(),     is_delim: true,  delim_idx: Some(7) },
            ];

            assert!(tokens == t, "tokens don't match expected:\n{tokens:#?}");

            // regex part
            let tokens: Vec<Token> = tokenize_regex(test, &TOKEN_DELIMS);
            assert_eq!(tokens.len(), 24, "regex len failed, tokens:\n{tokens:#?}");
            assert!(tokens == t, "regex tokens don't match expected:\n{tokens:#?}");

    }

    #[test]
    fn test_tokenize_empty_delim() {
        // An empty delimiter must be skipped rather than hanging the loop.
        let delims: [&str; 3] = [",", "", " "];
        let tokens: Vec<Token> = tokenize("a, b", &delims);

        assert_eq!(tokens.len(), 4, "tokens: {tokens:#?}");
        assert_eq!(tokens[0].content, "a");
        assert_eq!(tokens[1].content, ",");
        assert_eq!(tokens[1].delim_idx, Some(0));
        assert_eq!(tokens[2].content, " ");
        assert_eq!(tokens[2].delim_idx, Some(2));
        assert_eq!(tokens[3].content, "b");
    }

    #[test]
    fn test_tokenize_regex_empty_delim() {
        let delims: [&str; 3] = [",", "", " "];
        let tokens: Vec<Token> = tokenize_regex("a, b", &delims);

        assert_eq!(tokens.len(), 4, "tokens: {tokens:#?}");
        assert_eq!(tokens[0].content, "a");
        assert_eq!(tokens[1].content, ",");
        assert_eq!(tokens[1].delim_idx, Some(0));
        assert_eq!(tokens[2].content, " ");
        assert_eq!(tokens[2].delim_idx, Some(2));
        assert_eq!(tokens[3].content, "b");
    }

    #[test]
    fn test_tokenize_all_empty_delims() {
        // When every delimiter is empty (or there are none), the input passes
        // through as a single non-delim token.
        let delims: [&str; 2] = ["", ""];
        let tokens: Vec<Token> = tokenize("hello", &delims);
        assert_eq!(tokens.len(), 1);
        assert_eq!(tokens[0].content, "hello");
        assert!(!tokens[0].is_delim);

        let tokens: Vec<Token> = tokenize_regex("hello", &delims);
        assert_eq!(tokens.len(), 1);
        assert_eq!(tokens[0].content, "hello");
        assert!(!tokens[0].is_delim);
    }

    #[test]
    fn test_tokenize_multibyte() {
        // The linear tokenizer used to advance by one byte after pushing a
        // full char, panicking on a mid-char slice for any non-ASCII input.
        let input: &str = "héllo wörld,日本語";
        let expected: [&str; 5] = ["héllo", " ", "wörld", ",", "日本語"];

        let tokens: Vec<Token> = tokenize(input, &[" ", ","]);
        assert_eq!(tokens.len(), expected.len(), "tokens: {tokens:#?}");
        for (token, exp) in tokens.iter().zip(expected) {
            assert_eq!(token.content, exp);
        }

        // Both tokenizers must agree on multibyte input.
        let tokens_re: Vec<Token> = tokenize_regex(input, &[" ", ","]);
        assert!(tokens == tokens_re, "tokenizers diverge:\n{tokens:#?}\nvs\n{tokens_re:#?}");

        // Multibyte delimiters must work too.
        let tokens: Vec<Token> = tokenize("a→b", &["→"]);
        assert_eq!(tokens.len(), 3, "tokens: {tokens:#?}");
        assert_eq!(tokens[0].content, "a");
        assert_eq!(tokens[1].content, "→");
        assert!(tokens[1].is_delim);
        assert_eq!(tokens[2].content, "b");
    }

    /**
    Reference oracle for `scan_tokens`: the original char-by-char linear
    tokenizer, kept verbatim. It is O(n * m) and allocates per char, but its
    semantics (leftmost-first, earliest delimiter wins, empty delimiters
    skipped, advance by whole chars) are exactly what the scanner must
    reproduce.
    */
    fn reference_tokenize(s: &str, delims: &[&str]) -> Vec<Token> {
        let mut tokens: Vec<Token> = Vec::new();
        let mut current_token: String = String::new();
        let mut i: usize = 0;

        while i < s.len() {
            if let Some((d_idx, delimiter)) = delims
                .iter()
                .enumerate()
                .find(|(_, &d)| !d.is_empty() && s[i..].starts_with(d))
            {
                if !current_token.is_empty() {
                    tokens.push(Token {
                        content: std::mem::take(&mut current_token),
                        ..Default::default()
                    });
                }
                tokens.push(Token {
                    content: delimiter.to_string(),
                    is_delim: true,
                    delim_idx: Some(d_idx),
                });
                i += delimiter.len();
            } else {
                let c: char = s[i..].chars().next().unwrap();
                current_token.push(c);
                i += c.len_utf8();
            }
        }

        if !current_token.is_empty() {
            tokens.push(Token {
                content: current_token,
                ..Default::default()
            });
        }
        tokens
    }

    #[test]
    fn test_scanner_matches_reference() {
        /*
        The byte-class scanner must agree with the reference tokenizer on
        every input, including overlapping delimiters ("ab" vs "abc",
        "a" vs "aa"), multibyte chars, and duplicated or empty delimiters.
        Deterministic LCG so a failure is reproducible from the seed.
        */
        const ALPHABET: [&str; 4] = ["a", "b", "c", "é"];
        const DELIM_POOL: [&str; 11] =
            ["a", "b", "c", "ab", "ba", "abc", "bc", "aa", "é", "aé", ""];
        let mut seed: u64 = 42;
        let mut next = move |modulus: u64| -> usize {
            seed = seed
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            ((seed >> 33) % modulus) as usize
        };

        for case in 0..5000 {
            let n_delims: usize = 1 + next(5);
            let delims: Vec<&str> = (0..n_delims).map(|_| DELIM_POOL[next(11)]).collect();
            let len: usize = next(12);
            let s: String = (0..len).map(|_| ALPHABET[next(4)]).collect();

            let expected: Vec<Token> = reference_tokenize(&s, &delims);
            let actual: Vec<Token> = tokenize(&s, &delims);
            assert_eq!(
                actual, expected,
                "case {case}: scanner diverges on {s:?} with delims {delims:?}"
            );
            assert_eq!(tokenize_regex(&s, &delims), expected, "tokenize_regex must alias");
        }
    }

    #[test]
    fn test_latin1_two_byte_chars() {
        /*
        Codepoints 128-255 are 2 bytes in UTF-8. They used to bypass the
        ISO-8859-1 fast path (gated on byte length == 1) and get interned
        a second time at an index >= LATIN1_NUM.
        */
        let store: UniqueStrStore = UniqueStrStore::new();

        for cp in 1..LATIN1_NUM {
            let s: String = char::from_u32(cp).unwrap().to_string();
            assert!(store.contains(&s), "contains('{s}') should be true (cp {cp})");
            assert_eq!(store.idx(&s), Some(cp), "idx('{s}') should be {cp}");
            assert_eq!(store.insert(&s), cp, "insert('{s}') should return {cp}");
        }
        assert_eq!(store.len(), LATIN1_NUM as usize, "no duplicates should be stored");

        // 2-char strings (same byte length as a 2-byte char) still intern normally.
        let idx: u32 = store.insert("ab");
        assert_eq!(idx, LATIN1_NUM);
        assert_eq!(store.get(idx).unwrap(), "ab");

        // Single chars beyond Latin-1 also intern normally.
        let idx: u32 = store.insert("€");
        assert_eq!(store.get(idx).unwrap(), "€");
    }

    #[test]
    fn test_arena_chunk_boundaries_and_oversized() {
        /*
        Tiny chunks so strings straddle many chunk boundaries, plus strings
        longer than a chunk, which get a dedicated allocation. Pointers
        handed out before the arena grew must stay valid and identical.
        Under Miri this also checks that appending to a chunk does not
        invalidate the strings already in it.
        */
        let store: UniqueStrStore = UniqueStrStore::new_with_capacity(8, 16);
        let strs: Vec<String> = (0..500).map(|i| format!("s{i}-{}", "x".repeat(i % 40))).collect();
        let ids: Vec<u32> = strs.iter().map(|s| store.insert(s)).collect();
        let early: StoredStrPtr = unsafe { store.get_ptr(ids[0]) };
        let early_ref: &str = store.get(ids[0]).unwrap();

        for (s, &id) in strs.iter().zip(&ids) {
            assert_eq!(store.get(id).unwrap(), s);
            assert_eq!(store.insert(s), id, "re-insert must dedup");
        }
        assert_eq!(store.len(), LATIN1_NUM as usize + strs.len());
        assert_eq!(early.as_str(), strs[0]);
        assert!(ptr::eq(early.as_str(), store.get(ids[0]).unwrap()), "pointer moved");
        assert!(ptr::eq(early_ref, store.get(ids[0]).unwrap()), "reference moved");

        // a string longer than any chunk, followed by more small ones
        let big: String = "y".repeat(10_000);
        let big_id: u32 = store.insert(&big);
        let after: u32 = store.insert("after-the-big-one");
        assert_eq!(store.get(big_id).unwrap(), big);
        assert_eq!(store.get(after).unwrap(), "after-the-big-one");
        assert_eq!(store.insert(&big), big_id);
        store.validate_contents().ok();
    }

    #[test]
    fn test_failed_append_publishes_nothing() {
        /*
        The index entry is published before the push, and the chunk used
        to be allocated inside the push. A panic there (here a chunk that
        cannot be sized) left an entry pointing at a missing slot, and a
        safe `get_ref` then read out of bounds. The chunk size is set past
        the constructor's clamp to force the panic.
        */
        let store: UniqueStrStore = UniqueStrStore::new();
        store.0.store.lock().chunk_size = usize::MAX;
        let res = catch_unwind(AssertUnwindSafe(|| store.insert(HELLO)));
        assert!(res.is_err(), "the chunk allocation should have panicked");

        assert_eq!(store.len(), LATIN1_NUM as usize);
        assert_eq!(store.idx(HELLO), None, "no entry may outlive a failed push");
        assert!(store.get_ref(HELLO).is_none());
        store.validate_contents().ok();

        // the store stays usable
        store.0.store.lock().chunk_size = ARENA_CHUNK_SIZE;
        assert_eq!(store.insert(HELLO), LATIN1_NUM);
        assert_eq!(store.get_ref(HELLO).unwrap(), HELLO);
        store.validate_contents().ok();
    }

    #[test]
    fn test_reserve_front_loads_allocation() {
        // after `reserve`, `push` must not grow a Vec or start a chunk:
        // it runs while the index entry is already published
        let mut arena: StrArena = StrArena::new(16);
        let strs: [&str; 5] = ["abc", "defghijklmno", "pqrs", "an oversized string", "t"];
        let mut ptrs: Vec<*const str> = Vec::new();
        for s in strs {
            arena.reserve(s.len());
            let before = (arena.chunks.len(), arena.oversized.capacity());
            ptrs.push(arena.push(s));
            let after = (arena.chunks.len(), arena.oversized.capacity());
            assert_eq!(before, after, "push of {s:?} allocated after reserve");
        }
        let got: Vec<&str> = ptrs.iter().map(|&p| unsafe { &*p }).collect();
        assert_eq!(got, strs);
        assert_eq!(arena.oversized.len(), 1);
    }

    #[test]
    fn test_slot_table_layout() {
        // segment k holds 32 << k slots, back to back in index order
        assert_eq!(SlotTable::locate(0), (0, 0));
        assert_eq!(SlotTable::locate(31), (0, 31));
        assert_eq!(SlotTable::locate(32), (1, 0));
        assert_eq!(SlotTable::locate(95), (1, 63));
        assert_eq!(SlotTable::locate(96), (2, 0));
        let (k, off) = SlotTable::locate(MAX_USER_STRINGS as u32 - 1);
        assert_eq!(k, SLOT_SEGMENTS - 1, "the last index must need exactly the last segment");
        assert!(off < SlotTable::segment_len(k));

        // round trip across segment boundaries
        let table: SlotTable = SlotTable::with_capacity(0);
        let strs: Vec<String> = (0..200).map(|i| format!("s{i}")).collect();
        for (i, s) in strs.iter().enumerate() {
            unsafe {
                table.ensure(i as u32);
                table.write(i as u32, s.as_str());
            }
        }
        for (i, s) in strs.iter().enumerate() {
            assert_eq!(unsafe { &*table.read(i as u32) }, s);
        }
    }

    #[test]
    fn test_reader_follows_writer() {
        readers_follow_writer(1);
    }

    #[test]
    fn test_reader_follows_batch_writer() {
        // a batch holds the mutex across many inserts; waiting readers must cope
        readers_follow_writer(50);
    }

    /**
    Readers chase a writer: each spins on `idx` until the next string
    appears, then reads it back through every read path. Inserts publish
    the entry before the copy lands; the reads must wait for it, not fail
    or read an unwritten slot. The writer inserts one string at a time
    (`batch == 1`) or `batch` strings per `insert_many`.
    */
    fn readers_follow_writer(batch: usize) {
        // Miri interprets every spin; a few hundred rounds still interleave
        const N: usize = if cfg!(miri) { 300 } else { 20_000 };
        let store: UniqueStrStore = UniqueStrStore::new();
        let writer = {
            let store = store.clone();
            thread::spawn(move || {
                let all: Vec<String> = (0..N).map(|i| format!("follow-{i}")).collect();
                match batch {
                    1 => all.iter().for_each(|s| _ = store.insert(s)),
                    _ => all.chunks(batch).for_each(|c| _ = store.insert_many(c)),
                }
            })
        };
        let readers: Vec<_> = (0..3)
            .map(|_| {
                let store = store.clone();
                thread::spawn(move || {
                    for i in 0..N {
                        let s: String = format!("follow-{i}");
                        let idx: u32 = loop {
                            match store.idx(&s) {
                                Some(idx) => break idx,
                                None => hint::spin_loop(),
                            }
                        };
                        assert_eq!(store.get(idx).unwrap(), s);
                        assert_eq!(store.get_ref(&s).unwrap(), s.as_str());
                        assert_eq!(store.insert(&s), idx);
                    }
                })
            })
            .collect();
        writer.join().unwrap();
        for r in readers {
            r.join().unwrap();
        }
        store.validate_contents().ok();
    }

    #[test]
    fn test_chunk_size_is_clamped() {
        // an unallocatable chunk size used to panic on the first insert
        let chunk = |size: usize| UniqueStrStore::new_with_capacity(1, size).0.store.lock().chunk_size;
        assert_eq!(chunk(0), 1);
        assert_eq!(chunk(ARENA_CHUNK_SIZE), ARENA_CHUNK_SIZE);
        assert_eq!(chunk(usize::MAX), MAX_ARENA_CHUNK_SIZE);
    }

    #[test]
    fn test_hash_key_width() {
        // the `xxh128` feature is the only thing that changes the key type
        let expected: usize = if cfg!(feature = "xxh128") { 16 } else { 8 };
        assert_eq!(size_of::<HashKey>(), expected);
    }

    #[test]
    fn test_nul_is_not_the_empty_string() {
        /*
        Index 0 holds the empty string, not '\0'. The Latin-1 fast path
        used to accept codepoint 0, so a NUL string mapped to index 0
        and read back as "".
        */
        let store: UniqueStrStore = UniqueStrStore::new();
        assert!(!store.contains("\0"), "NUL must not be implicitly contained");
        assert_eq!(store.idx("\0"), None);

        let idx: u32 = store.insert("\0");
        assert!(idx >= LATIN1_NUM, "NUL must be interned as regular content, got {idx}");
        assert_eq!(store.get(idx).unwrap(), "\0");
        assert_eq!(store.get(0).unwrap(), "");
        assert!(store.contains("\0"));
        assert_eq!(store.idx("\0"), Some(idx));
        assert_eq!(store.insert("\0"), idx, "second insert must dedup");
        store.validate_contents().ok();
    }

    #[test]
    fn test_default_store() {
        // The derived `Default` used to produce a store with an empty ascii
        // table and len == 0, panicking on basic operations.
        let store: UniqueStrStore = UniqueStrStore::default();
        assert_eq!(store.len(), LATIN1_NUM as usize);

        let idx: u32 = store.insert("a");
        assert_eq!(idx, 'a' as u32);
        assert_eq!(store.get(idx).unwrap(), "a");

        let idx: u32 = store.insert(HELLO);
        assert_eq!(idx, LATIN1_NUM);
        assert_eq!(store.get(idx).unwrap(), HELLO);
    }

    #[test]
    fn test_unique_store_basic() {
        let store: UniqueStrStore = UniqueStrStore::new_with_capacity(10, ARENA_CHUNK_SIZE);
        assert_eq!(store.len(), LATIN1_NUM as usize, "Store length should be {LATIN1_NUM}");

        let test = ["", " ", "a", "Z", "1", "2", "3", "/", ",", ")"];
        for s in test {
            assert!(store.contains(s), "store should contains('{s}')");
        }

        let i: u32 = store.insert(HELLO);
        let num: usize = LATIN1_NUM as usize + 1;

        assert_eq!(store.len(), num, "Store length should be {num}");
        assert!(store.contains(HELLO), "Store does not contain '{HELLO}': {store:?}");
        assert_eq!(store.get(i).unwrap(), HELLO, "get({i}) should == '{HELLO}'");
        assert_eq!(unsafe { store.borrow_str(i) }, HELLO, "get_unchecked({i}) should == '{HELLO}'");

        // Test the pointer structs.
        let stored: StoredStr = store.get_ref(HELLO).expect("StoredStr should be returned");
        assert_eq!(stored.idx(), i, "StoredStr index should be {i}");
        let ptr: StoredStrPtr = stored.as_ptr();
        assert!(!ptr.is_null(), "StoredStrPtr should not be null: {ptr:?}");
        assert_eq!(ptr.as_str(), HELLO, "Pointer string should be '{HELLO}': {ptr:?}");
    }

    #[test]
    #[should_panic(expected = "out of bounds")]
    fn test_borrow_str_oob_at_latin1_boundary() {
        /*
        idx == LATIN1_NUM (256) with an empty user-string store used to be
        undefined behavior because the bounds check used `>` instead of `>=`.
        Must now panic.
        */
        let store: UniqueStrStore = UniqueStrStore::new();
        unsafe {
            let _ = store.borrow_str(LATIN1_NUM);
        }
    }

    #[test]
    fn test_unique_store_shared() {
        let foo_s: &'static str = "foo";
        let store: Arc<UniqueStrStore> = UniqueStrStore::new_with_capacity(10, ARENA_CHUNK_SIZE).shared();
        let stored: StoredStr = store.insert_or_get(HELLO);
        let start: u32 = LATIN1_NUM;

        assert_eq!(store.len(), start as usize + 1, "Store length should be {}", start + 1);
        assert!(store.contains(HELLO), "Store does not contain '{HELLO}': {store:?}");

        let again: StoredStr = store.insert_or_get(HELLO);
        let foo: StoredStr = store.insert_or_get(foo_s);
        assert_eq!(store.len(), start as usize + 2, "Store length should be {}", start + 2);

        assert_eq!(stored.idx(), start, "'{HELLO}' idx should be {start}: {stored:?}");
        assert_eq!(again.idx(), start, "Second '{HELLO}!' idx should be again {start}: {again:?}");
        assert_eq!(foo.idx(), start + 1, "'{foo_s}' idx should be {}: {foo:?}", start + 1);

        assert_eq!(stored.as_ref(), HELLO, "as_ref() should == '{HELLO}': {stored:?}");
        assert_eq!(stored, again, "StoredStr instances should be equal: {stored:?} != {again:?}");
        assert_eq!(store.get(start).unwrap(), HELLO, "get({start}) should == '{HELLO}'");
        assert_eq!(
            unsafe { store.borrow_str(start + 1) },
            foo_s,
            "get_unchecked({}) should == '{foo_s}'",
            start + 1
        );
    }

    #[test]
    fn test_stored_str_ptr_eq_by_content() {
        // `==`, `cmp` and `Hash` must agree; equal strings in two stores are equal.
        let a: UniqueStrStore = UniqueStrStore::new();
        let b: UniqueStrStore = UniqueStrStore::new();
        let pa: StoredStrPtr = a.insert_or_get(HELLO).as_ptr();
        let pa2: StoredStrPtr = a.insert_or_get(HELLO).as_ptr();
        let pb: StoredStrPtr = b.insert_or_get(HELLO).as_ptr();
        let other: StoredStrPtr = b.insert_or_get("other").as_ptr();

        assert!(ptr::eq(pa.0, pa2.0), "same store must intern to the same pointer");
        assert!(!ptr::eq(pa.0, pb.0), "different stores allocate separately");
        assert_eq!(pa, pa2);
        assert_eq!(pa, pb);
        assert_eq!(pa.cmp(&pb), Ordering::Equal);
        assert_ne!(pa, other);
        assert_ne!(pa.cmp(&other), Ordering::Equal);
    }

    #[test]
    fn test_concurrent_inserts() {
        use std::thread;

        let exp_len: usize = CONC_S_NUM + LATIN1_NUM as usize;
        let store: UniqueStrStore = UniqueStrStore::new_with_capacity(exp_len, ARENA_CHUNK_SIZE);

        let threads: Vec<_> = (0..CONC_T_NUM)
            .map(|t: usize| {
                let store = store.clone();
                thread::spawn(move || {
                    (0..CONC_S_NUM / CONC_T_NUM).for_each(|i: usize| {
                        let s: String = format!("{HELLO} t: {t}, i: {i}");
                        let stored: StoredStr = store.insert_or_get(&s);
                        assert_eq!(stored.as_ref(), s, "Stored string should be '{s}': {stored:?}");
                    })
                })
            })
            .collect();

        for t in threads {
            t.join().unwrap();
        }

        store.validate_contents().ok(); // will panic on failure in debug mode
        assert_eq!(store.len(), exp_len, "Stored num should be {}", exp_len);
    }

    #[test]
    fn test_competing_inserts() {
        use std::thread;

        let per_thread: usize = CONC_S_NUM / CONC_T_NUM;
        let exp_len: usize = per_thread + LATIN1_NUM as usize;
        let store: UniqueStrStore = UniqueStrStore::new_with_capacity(exp_len, ARENA_CHUNK_SIZE);

        let threads: Vec<_> = (0..CONC_T_NUM)
            .map(|_t| {
                let store = store.clone();
                thread::spawn(move || {
                    (0..per_thread).for_each(|i: usize| {
                        let s: String = format!("{HELLO} i: {i}");
                        let stored: StoredStr = store.insert_or_get(&s);
                        assert_eq!(stored.as_ref(), s, "Stored string should be '{s}': {stored:?}");
                    })
                })
            })
            .collect();

        for t in threads {
            t.join().unwrap();
        }

        store.validate_contents().ok(); // will panic on failure in debug mode
        assert_eq!(store.len(), exp_len, "Stored num should be {}", exp_len);
    }

    #[rustfmt::skip]
    #[test]
    fn test_split_and_store() {
        let store: UniqueStrStore = UniqueStrStore::new();
        let input: &str = ",apple,banana,cherry,cake,,cake,,,";
        let exp_v: Vec<&str> = vec!["", "apple", "banana", "cherry", "cake", "", "cake", "", "", ""];
        let delim: &str = ",";
        // 4 uniq parts (delim + empty string already should exist)
        let mut exp_store_len: usize = (LATIN1_NUM + 4) as usize;

        let (indices, d) = store.split_and_store(input, delim);
        assert_eq!(indices.len(), exp_v.len(), "{indices:?}");
        assert_eq!(store.len(), exp_store_len);

        // Check that the delimiter is stored
        assert!(store.contains(delim), "Store should contain the delim: '{delim}'");
        assert_eq!(store.get(d).unwrap(), delim);

        // Check that the parts are stored correctly
        for (i, &idx) in indices.iter().enumerate() {
            let exp: &str = exp_v[i];
            assert_eq!(store.get(idx).unwrap(), exp, "index {i}: '{exp}'");
        }

        // Check that the original string can be reconstructed
        let built: String = indices
            .iter()
            .map(|&idx| unsafe { store.borrow_str(idx) })
            .collect::<Vec<&str>>()
            .join(delim);

        assert_eq!(input, built, "Reconstructed string should be '{input}'");
        assert_eq!(
            input,
            store.reconstruct(&indices, d).unwrap(),
            "input <-> reconstruct() mismatch"
        );

        /* --------------------------------- */

        // Check for incorrect delimiter handling
        let delim: &str = ";";
        let (indices, d) = store.split_and_store(input, delim);
        exp_store_len += 1; // +1 new part
        assert_eq!(indices.len(), 1, "{indices:?}");
        assert_eq!(store.len(), exp_store_len);
        assert_eq!(store.get(d).unwrap(), delim, "Store should have the next delim: '{delim}'");
        assert!(store.contains(input), "Store should contain '{input}' (not split)");

        assert_eq!(
            input,
            store.reconstruct(&indices, d).unwrap(),
            "input <-> reconstruct() mismatch (not split)"
        );

        store.validate_contents().ok();
    }

    #[rustfmt::skip]
    #[test]
    fn test_split_and_store_multi() {
        /*
        Delimiter tokens reuse the index their delimiter was interned at,
        for multi-character, single-character and duplicated delimiters;
        an empty delimiter maps to 0. The parts concatenate to the input.
        */
        let store: UniqueStrStore = UniqueStrStore::new();
        let input: &str = "key = value :: next=1 :: end";
        let delims: [&str; 5] = [" :: ", " = ", "", "=", " :: "];
        let (parts, delim_idx) = store.split_and_store_multi(input, &delims, None);

        let exp: [&str; 9] = ["key", " = ", "value", " :: ", "next", "=", "1", " :: ", "end"];
        let got: Vec<&str> = parts.iter().map(|&i| store.get(i).unwrap()).collect();
        assert_eq!(got, exp);
        assert_eq!(got.concat(), input);

        assert_eq!(delim_idx.len(), delims.len());
        for (d, &i) in delims.iter().zip(&delim_idx) {
            assert_eq!(store.get(i).unwrap(), *d);
        }
        assert_eq!(delim_idx[2], 0, "an empty delimiter maps to index 0");
        assert_eq!(delim_idx[0], delim_idx[4], "a duplicated delimiter interns once");
        assert_eq!([parts[1], parts[3], parts[5], parts[7]],
                   [delim_idx[1], delim_idx[0], delim_idx[3], delim_idx[0]]);
        store.validate_contents().ok();
    }

    #[test]
    fn test_reconstruct_oob_reports_max_index() {
        // Both error variants report the highest *valid* index as `max`.
        let store: UniqueStrStore = UniqueStrStore::new();
        let last: u32 = store.insert("last");
        let bad: u32 = last + 1;

        assert_eq!(
            store.reconstruct(&[last], bad),
            Err(StringStoreError::oob(bad, last as usize))
        );
        assert_eq!(
            store.reconstruct(&[last, bad], last),
            Err(StringStoreError::reconstruction(bad, 1, last))
        );
    }

    #[rustfmt::skip]
    #[test]
    fn test_store_path() {
        let store: UniqueStrStore = UniqueStrStore::new();
        let path1: &str = "/home/user/foo/bar/garbage.txt";
        let exp_1: Vec<&str> = vec!["", "home", "user", "foo", "bar", "garbage.txt"];
        let parts1: Vec<u32> = store.store_path(path1);

        // 5 uniq parts (delim + empty string already should exist)
        let mut exp_store_len: usize = (LATIN1_NUM + 5) as usize;

        // 6 returned indices expected, not 5, since it includes the
        // empty string at the 1st index, as this is an absolute path
        assert_eq!(parts1.len(), exp_1.len(), "{parts1:?}");
        assert_eq!(store.len(), exp_store_len);

        // Check that the delimiter is stored
        assert!(store.contains(PATH_SEP), "Store should contain the delim: '{PATH_SEP}'");

        // Check that the parts are stored correctly
        for (i, &idx) in parts1.iter().enumerate() {
            let exp: &str = exp_1[i];
            assert_eq!(store.get(idx).unwrap(), exp, "parts1 {i}: '{exp}'");
        }

        // Check that the original string can be reconstructed
        let built: String = parts1
            .iter()
            .map(|&idx| unsafe { store.borrow_str(idx) })
            .collect::<Vec<&str>>()
            .join(PATH_SEP);

        assert_eq!(path1, built, "Reconstructed path should be '{path1}'");
        assert_eq!(
            path1,
            store.reconstruct(&parts1, store.idx(PATH_SEP).unwrap()).unwrap(),
            "input <-> reconstruct() mismatch (path1)"
        );

        /* --------------------------------- */

        // Check for canonicalized path handling
        let path2: &str = "/home/user/./..../foo/../bar/garbage2.txt";
        let exp_2: Vec<&str> = vec!["", "home", "user", "bar", "garbage2.txt"];
        let parts2: Vec<u32> = store.store_path(path2);

        exp_store_len += 1; // 1 new part, as the "dots" should be normalized away
        assert_eq!(parts2.len(), exp_2.len(), "{parts2:?}");
        assert_eq!(store.len(), exp_store_len);

        for (i, &idx) in parts2.iter().enumerate() {
            let exp: &str = exp_2[i];
            assert_eq!(store.get(idx).unwrap(), exp, "parts2 {i}: '{exp}'");
        }

        assert_eq!(
            exp_2.join(PATH_SEP),
            store.reconstruct(&parts2, store.idx(PATH_SEP).unwrap()).unwrap(),
            "input <-> reconstruct() mismatch (path2)"
        );

        /* --------------------------------- */

        // Check for relative path handling
        let path3: &str = "veri/sekrit/.///hidn/../lokas\0juun/garbage.1";
        let exp_3: Vec<&str> = vec!["veri", "sekrit", "lokasjuun", "garbage.1"];
        let parts3: Vec<u32> = store.store_path(path3);

        exp_store_len += exp_3.len();
        assert_eq!(parts3.len(), exp_3.len(), "{parts3:?}");
        assert_eq!(store.len(), exp_store_len);

        for (i, &idx) in parts3.iter().enumerate() {
            let exp: &str = exp_3[i];
            assert_eq!(store.get(idx).unwrap(), exp, "parts3 {i}: '{exp}'");
        }

        assert_eq!(
            exp_3.join(PATH_SEP),
            store.reconstruct(&parts3, store.idx(PATH_SEP).unwrap()).unwrap(),
            "input <-> reconstruct() mismatch (path3)"
        );
    }

    #[test]
    fn test_try_insert() {
        let store: UniqueStrStore = UniqueStrStore::new();
        assert_eq!(store.try_insert(""), Ok(0));
        assert_eq!(store.try_insert("a"), Ok('a' as u32));

        let idx: u32 = store.try_insert(HELLO).unwrap();
        assert_eq!(idx, LATIN1_NUM);
        assert_eq!(store.try_insert(HELLO), Ok(idx), "duplicate should return same index");
        assert_eq!(store.insert(HELLO), idx, "insert/try_insert must agree");
        assert_eq!(store.len(), LATIN1_NUM as usize + 1);
    }

    #[test]
    fn test_insert_many_matches_insert() {
        let batched: UniqueStrStore = UniqueStrStore::new();
        let single: UniqueStrStore = UniqueStrStore::new();
        // one string interned before the batch, taking the lock-free hit path
        assert_eq!(batched.insert("pre"), single.insert("pre"));

        // new, empty, 1- and 2-byte ISO-8859-1, pre-interned, in-batch duplicates
        let input: [&str; 9] = ["alpha", "", "a", "é", "pre", "beta", "alpha", "€", "beta"];
        let indices: Vec<u32> = batched.insert_many(&input);
        let expected: Vec<u32> = input.iter().map(|s: &&str| single.insert(s)).collect();
        assert_eq!(indices, expected, "insert_many must agree with sequential inserts");
        assert_eq!(indices[0], indices[6], "in-batch duplicates share one index");
        assert_eq!(indices[5], indices[8], "in-batch duplicates share one index");
        assert_eq!(indices[1], 0, "the empty string is index 0");
        assert_eq!(indices[3], 'é' as u32, "a 2-byte ISO-8859-1 char is its codepoint");
        for (idx, s) in indices.iter().zip(input) {
            assert_eq!(batched.get(*idx).unwrap(), s);
        }
        assert_eq!(batched.len(), single.len());
        batched.validate_contents().ok();

        // all known: resolved without an insert, the store does not grow
        let len: usize = batched.len();
        assert_eq!(batched.insert_many(&input), indices);
        assert_eq!(batched.len(), len);
        // owned strings work too, and so does an empty batch
        let owned: Vec<String> = vec!["gamma".to_string(), "alpha".to_string()];
        assert_eq!(batched.insert_many(&owned), vec![batched.insert("gamma"), indices[0]]);
        assert!(batched.insert_many::<&str>(&[]).is_empty());
    }

    #[test]
    fn test_try_insert_many() {
        let store: UniqueStrStore = UniqueStrStore::new();
        let indices: Vec<u32> = store.try_insert_many(&[HELLO, "", HELLO]).unwrap();
        assert_eq!(indices, vec![LATIN1_NUM, 0, LATIN1_NUM]);
        assert_eq!(store.try_insert(HELLO), Ok(LATIN1_NUM), "try_insert/try_insert_many must agree");
        assert_eq!(store.len(), LATIN1_NUM as usize + 1);
    }

    #[test]
    fn test_competing_insert_many() {
        /*
        All threads intern the same strings, in batches that start at
        different offsets so the batches overlap only partly; every other
        thread uses plain inserts. Batched and single inserts must agree on
        every index, and each string must be stored exactly once.
        */
        const BATCH: usize = 64;
        // Miri: a few batches per thread still overlap and compete
        let per_thread: usize = if cfg!(miri) { 4 * BATCH } else { CONC_S_NUM / CONC_T_NUM };
        let exp_len: usize = per_thread + LATIN1_NUM as usize;
        let store: UniqueStrStore = UniqueStrStore::new();
        let all: Arc<Vec<String>> =
            Arc::new((0..per_thread).map(|i| format!("{HELLO} i: {i}")).collect());

        let threads: Vec<_> = (0..CONC_T_NUM)
            .map(|t: usize| {
                let store = store.clone();
                let all = all.clone();
                thread::spawn(move || {
                    let mut indices: Vec<u32> = Vec::with_capacity(all.len());
                    if t % 2 == 1 {
                        all.iter().for_each(|s| indices.push(store.insert(s)));
                        return indices;
                    }
                    // an odd head batch shifts this thread's batch boundaries
                    let (head, rest) = all.split_at((t * 7) % BATCH);
                    indices.extend(store.insert_many(head));
                    rest.chunks(BATCH).for_each(|c| indices.extend(store.insert_many(c)));
                    indices
                })
            })
            .collect();

        let results: Vec<Vec<u32>> = threads.into_iter().map(|t| t.join().unwrap()).collect();
        for r in &results[1..] {
            assert_eq!(r, &results[0], "threads disagree on indices");
        }
        for (idx, s) in results[0].iter().zip(all.iter()) {
            assert_eq!(store.get(*idx).unwrap(), s);
        }
        store.validate_contents().ok(); // will panic on failure in debug mode
        assert_eq!(store.len(), exp_len, "Stored num should be {}", exp_len);
    }

    #[test]
    fn test_token_accessors() {
        let tokens: Vec<Token> = tokenize("a,b", &[","]);
        assert_eq!(tokens.len(), 3);
        assert_eq!(tokens[0].content(), "a");
        assert!(!tokens[0].is_delim());
        assert_eq!(tokens[0].delim_idx(), None);
        assert_eq!(tokens[1].content(), ",");
        assert!(tokens[1].is_delim());
        assert_eq!(tokens[1].delim_idx(), Some(0));
    }

    #[test]
    fn test_store_debug_is_legible() {
        let store: UniqueStrStore = UniqueStrStore::new();
        let idx: u32 = store.insert("foo");
        let dbg: String = format!("{store:?}");
        assert!(dbg.contains(&format!("{idx}: \"foo\"")), "{dbg}");
        assert!(dbg.contains("len: 257"), "{dbg}");
        assert!(!dbg.contains("\"A\""), "ISO-8859-1 table must not be dumped: {dbg}");
    }

    #[test]
    fn test_token_display() {
        let tokens: Vec<Token> = tokenize("a,b", &[","]);
        let joined: String = tokens.iter().map(Token::to_string).collect();
        assert_eq!(joined, "a,b");
    }

    #[rustfmt::skip]
    #[test]
    fn test_structured_line_display_and_debug() {
        let store: Arc<UniqueStrStore> = UniqueStrStore::new().shared();
        let word = |s: &str| CompactStr(store.insert(s));
        let ch = |s: &str, n: u8| Character(CompactStr(store.insert(s)), n);
        let mut line: StructuredLine = StructuredLine::new(&store);

        line.push(TextElement::Word(word("Mary")));
        line.push(TextElement::Delimiter(ch(" ", 1)));
        line.push(TextElement::KeyVal(word("had"), word("lamb")));
        line.push(TextElement::Delimiter(ch(" ", 2)));
        line.push(TextElement::Integer(-42));
        line.push(TextElement::Char(ch("*", 3)));
        line.push(TextElement::Range(0, 100, ch("-", 1)));
        line.push(TextElement::Day(2026, 9, 29));
        line.push(TextElement::Time(7, 5, 0));
        line.push(TextElement::IPAddress("10.0.0.1".parse().unwrap()));
        line.push(TextElement::Email(word("john"), word("example.org")));
        line.push(TextElement::HexStr(Hex(0xfeedf00d), HexFormat::PREFIX));
        line.push(TextElement::Sentence(vec![word("a"), word("little")]));
        line.push(TextElement::URLParams(vec![word("x=1"), word("y=2")]));
        line.push(TextElement::EnclosedElem(
            Box::new(TextElement::Word(word("inner"))), word("("), word(")"),
        ));
        line.push(TextElement::RawText("raw".to_string()));

        assert_eq!(
            line.to_string(),
            "Mary had=lamb  -42***0-1002026-09-2907:05:0010.0.0.1john@example.org0xfeedf00da little?x=1&y=2(inner)raw"
        );
        assert_eq!(line.len(), 16);

        let dbg: String = format!("{line:?}");
        let mary: u32 = store.idx("Mary").unwrap();
        assert!(dbg.starts_with("StructuredLine ["), "{dbg}");
        assert!(dbg.contains(&format!("Word(CompactStr({mary}: \"Mary\"))")), "{dbg}");
        assert!(dbg.contains("Delimiter(Character(32: \" \" x2))"), "{dbg}");
        assert!(dbg.contains("Integer(-42)"), "{dbg}");
        assert!(dbg.contains("EnclosedElem(Word(CompactStr("), "{dbg}");
        assert!(!dbg.contains("UniqueStrStore"), "store must not be dumped: {dbg}");
    }

    #[test]
    fn test_hex_to_string() {
        let h: Hex = Hex(0xdeadbeef);
        assert_eq!(h.to_string(HexFormat::PLAIN), "deadbeef");
        assert_eq!(h.to_string(HexFormat::UPPER), "DEADBEEF");
        assert_eq!(h.to_string(HexFormat::PREFIX), "0xdeadbeef");
        assert_eq!(h.to_string(HexFormat::COLUMNS), "dead:beef");

        // groups count from the right when the digit count isn't a multiple of 4
        assert_eq!(Hex(0xfeedf00d5).to_string(HexFormat::COLUMNS), "f:eedf:00d5");
        assert_eq!(Hex(0x5).to_string(HexFormat::COLUMNS), "5");

        // the "0x" prefix is applied last: neither uppercased nor grouped
        let all: HexFormat =
            HexFormat(HexFormat::UPPER.0 | HexFormat::PREFIX.0 | HexFormat::COLUMNS.0);
        assert_eq!(h.to_string(all), "0xDEAD:BEEF");

        assert_eq!(all.to_string(), "Upper|Prefix|Columns");
        assert_eq!(HexFormat::PLAIN.to_string(), "Plain");
        assert_eq!(HexFormat(0b1000).to_string(), "Unknown(0b1000)");
    }

    #[test]
    fn test_error_display() {
        let err = StringStoreError::oob(42, 10);
        assert_eq!(err.to_string(), "index 42 out of bounds (max: 10)");

        let err = StringStoreError::reconstruction(5, 2, 4);
        assert_eq!(err.to_string(), "invalid index 5 at position 2 (max: 4)");

        let err = StringStoreError::InvalidPath("contains null byte".to_string());
        assert_eq!(err.to_string(), "invalid path: contains null byte");
    }

    #[test]
    fn test_error_debug() {
        let err = StringStoreError::StoreFull;
        assert_eq!(format!("{err:?}"), "StoreFull");

        let err = StringStoreError::delimiter("Empty delimiter");
        assert_eq!(format!("{err:?}"), r#"InvalidDelimiter("Empty delimiter")"#);
    }
}

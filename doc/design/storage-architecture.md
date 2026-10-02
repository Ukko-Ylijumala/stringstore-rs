# Storage architecture

`UniqueStrStore` is not one container — it is three, glued together by a single index space that hides the seam from callers. Understanding the seam is the prerequisite for touching almost anything else in `src/lib.rs`.

## The three backing containers

`UniqueStrStore` itself is a newtype around `Arc<StoreInner>`; all the fields below live in that one allocation, so `Clone` is a single refcount increment and every access dereferences one pointer.

```text
public index space:    0 ─────────────── 255 │ 256 ────────────── u32::MAX
                       └── ascii Vec ───┘    │ └── slots (offset by 256) ────────┘
                       (fixed, no lock)      │ (SlotTable: segments of *const str,
                                             │  read without a lock, pointing into
                                             │  the 128 KiB bump chunks of the
                                             │  StrArena behind Mutex `store`)

content lookup:        index: DashMap<HashKey xxh3 hash, u32 internal index, PreHashed>
                       (HashKey = u64, or u128 with the `xxh128` feature)
length:                len: AtomicU32  // public length, starts at 256; publishes slots
```

1. **`ascii: Vec<Box<str>>`** — a fixed 256-entry vector populated once at construction with every ISO-8859-1 codepoint as a one-character `Box<str>`. The empty string `""` replaces `'\0'` at index 0, so a NUL string is *not* covered by this table: `return_iso8859_1_cp` rejects codepoint 0 and `"\0"` is interned through the regular hash-indexed path like any other content. Read access skips both `len` and the hash map entirely.
2. **`slots: SlotTable` plus `store: Mutex<StrArena>`** — the actual interned strings. Internally indexed `0..N`, but every public-facing index is offset by `LATIN1_NUM` (256). `SlotTable` holds one fat pointer per string in fixed segments of doubling size (`32 << k` slots in segment `k`, 27 segments covering every index), which never move, so any thread reads a slot without a lock. The pointers lead into the chunked bump arena `StrArena`, which only inserts touch, under the mutex; see "String bytes: the arena" below and `concurrency.md`. A segment is allocated zeroed on first use (or up front for `new_with_capacity`), so its pages stay untouched until slots are written — like `Vec::with_capacity`, but rounded up to whole segments.
3. **`index: DashMap<HashKey, u32, PreHashed>`** — content-to-position lookup, keyed by the xxh3 hash of the bytes (`HashKey` is `u64` by default, `u128` with the `xxh128` feature; see below). The stored value is the *internal* `store` index (pre-offset). `PreHashed` is an identity `BuildHasher`: the keys are already uniformly distributed 64-bit digests, so the map passes them straight through instead of running them through a second (streaming, 832-byte-state) Xxh3 pass per lookup. That second pass used to cost ~140 ns per map operation versus ~30 ns now; it is the single largest cost that was removed from `contains`/`idx`/`insert`. (`custom_xxh3` ≥ 0.4.1 has `QuickXxh3Builder`, which re-hashes a `u64` in ~1 ns. That would no longer be a disaster, but the identity hasher is still free, so there is no reason to switch.)

`len` is a `std::sync::atomic::AtomicU32` that holds the authoritative public length. It starts at 256 (the ASCII range is always "present"), and is stored by the mutex holder in `insert_locked` only after the new string's slot is written. The store uses `Release` ordering and every read loads `len` with `Acquire` before it reads a slot, so observing the new length implies the slot and the string bytes are visible. That pairing is what lets reads skip the lock.

## String bytes: the arena

`StrArena` copies each new string to the end of the current chunk, a heap allocation of `chunk_size` bytes owned through a raw pointer (`ARENA_CHUNK_SIZE` = 128 KiB by default, overridable per store through `new_with_capacity(capacity, chunk_size)`, which clamps it to 1 byte – 1 GiB). Why raw pointers and not `Box<[u8]>` is in `unsafe-pointers.md`. When the string does not fit in what is left, a fresh chunk is started; the unused tail of the old one is wasted, bounded by one string length per chunk. A string longer than a whole chunk gets an exact-size chunk of its own in a separate `oversized` list, so the current chunk keeps filling. Chunks are never resized, moved or freed until the arena drops. The slot for each string is a fat pointer into its chunk, so a read is one slot load — identical cost to the old `Box<str>` deref.

Why: one `malloc` per string was the dominant memory cost for short strings. glibc's smallest block is 32 bytes, so a 9-byte string cost 32 bytes of heap plus its 16-byte slot. Measured with 2M strings per row (500k for the long row), release build, glibc, resident-set growth including the slot vector:

| average string | `Vec<Box<str>>` | arena | saved |
|---|---|---|---|
| 9 B (tokens) | 48.0 B/str | 25.4 B/str | 47 % |
| 31 B (paths) | 64.1 B/str | 47.7 B/str | 26 % |
| 150 B (lines) | 182.5 B/str | 167.8 B/str | 8 % |

Build time for 2M short strings dropped from 68 ms to 28 ms (the `malloc` call left the write-lock critical section), and drop from 14 ms to 3 ms (a few hundred chunks to free instead of millions of blocks). The index map and the slot vector are untouched by this and still cost roughly 24 + 16 bytes per string, which is why the whole-store picture (same datasets, through `UniqueStrStore::insert`, v0.3.13 vs. the arena) shows smaller percentages:

| average string | v0.3.13 | arena | `insert` ns/op | drop |
|---|---|---|---|---|
| 9 B | 88.2 B/str | 66.4 B/str (−25 %) | 165 → 124 | 18 → 8 ms |
| 31 B | 104.1 B/str | 88.4 B/str (−15 %) | 188 → 156 | 20 → 15 ms |
| 150 B | 222.8 B/str | 212.9 B/str (−4 %) | 222 → 221 | 14 → 11 ms |

`get` is unchanged at ~9.5 ns/op on these 2M-string stores. Under an allocator with less per-block overhead than glibc (jemalloc, mimalloc) the memory saving shrinks toward the long-string row.

`StrArena` holds raw pointers and is therefore `!Send + !Sync` by default; it carries explicit `unsafe impl`s, justified because every chunk pointer is a unique allocation owned by the struct, whose bytes are written once before a pointer to them is handed out and freed only with the arena. Behind the mutex this is no different from sharing a `Vec<Box<str>>`.

## The LATIN1_NUM offset

The constant `LATIN1_NUM = 256` is load-bearing. Any code touching indices must know which space it is in:

| Space | Range | Where it appears |
|---|---|---|
| Public (offset applied) | `0..len()` | All `pub` method args and returns, `idx()`, `get()`, `borrow_str(idx)`, `StoredStr.0`, etc. |
| Internal (slots) | `0..len() - 256` | DashMap values, the `idx` variable inside `insert_locked`, `SlotTable::read/write`, `slot(i)`. |

Translation points to watch:

- `idx()`: returns `self.index.get(...).map(|r| r.value() + LATIN1_NUM)` — DashMap value is internal, add the offset for the public answer.
- `lookup(idx)` / `get_str_ptr(idx)`: branch on `idx < LATIN1_NUM`. If yes, hit `ascii[idx]` directly. If no, read slot `idx - LATIN1_NUM`. `lookup` is the bounds-checked form (`get`, `borrow_str`, `reconstruct`), checking against `len`; `get_str_ptr` (via `slot`) is the unchecked form for callers that already hold a valid index (`get_ptr`, `StoredStr`). Neither takes a lock.
- `insert_locked`: the looked-up `indexed` and the new `idx = len - LATIN1_NUM` are internal; both return paths add `LATIN1_NUM`.
- `reconstruct`: `lookup` for each part; the store only grows, so an index validated in the sizing pass is still valid in the build pass.

## Why the split exists

The motivation is twofold:

1. **Hash/lock avoidance for the common case.** Single-character ISO-8859-1 strings are extremely common in tokenized output (whitespace, punctuation, digits). Routing them through `xxh3 → DashMap → slot` would dominate the cost of trivial inserts; the `ascii` short-circuit collapses these to a direct array index.
2. **Stable "well-known" indices.** Callers can assume `0` is always the empty string and `1..256` are the ISO-8859-1 codepoints, regardless of insertion order, without ever calling `insert`. This makes index 0 usable as a sentinel (see `splitting-and-paths.md`).

## Hash-only identity and the collision policy

The `index` DashMap is keyed by the xxh3 hash of the string bytes — within the index, string identity *is* the hash. Two distinct strings colliding on the full key width cannot both be represented. With the default 64-bit key the odds are ~n²/2⁶⁵, roughly 1 in 370k for a store holding 10M strings, which is why the insert path verifies.

The policy as of v0.3.9 (64-bit keys, the default):

- **`insert` verifies.** On a hash hit — both in the fast path and in the lost-race branch inside `insert_locked` — the stored string's contents are compared against the incoming string. A mismatch calls `collision_panic`: a deliberate panic, because silently returning the other string's index would corrupt every downstream index vector. There is no graceful recovery without re-keying the index (e.g. `DashMap<u64, SmallVec<u32>>`); revisit only if a collision is ever observed in the wild.
- **`contains` and `idx` do not verify.** They remain pure hash lookups (no content fetch) to keep them as cheap as possible. Consequence: for a string that was *never inserted* but collides with a stored one, `contains` returns a false positive and `idx` returns the colliding string's index. Strings that went through `insert` are unaffected — the insert-time check guarantees no two *stored* strings share a hash.

Cost of the insert-side check: every duplicate `insert` reads the stored string's slot (no lock) and performs one string comparison. The new-string path is unchanged (one hash, one DashMap miss, then the insert under the mutex).

### The `xxh128` feature: wider keys, no verification

With `--features xxh128`, `HashKey` becomes `u128` (xxh3-128 via `xxhash_rust::xxh3::xxh3_128`, default secret) and `verify_hit` / `collision_panic` are not compiled at all. The collision odds drop to ~n²/2¹²⁹ (about 10⁻²⁵ at 10M strings), and in exchange:

- The duplicate-insert path returns straight from the DashMap: no slot read, no string compare. `insert` of an already-interned string becomes as cheap as `idx`.
- The lost-race branch inside `insert_locked` likewise trusts the hash.
- `contains`/`idx` false positives become equally negligible.

What it costs: the index entry grows from `(u64, u32)` to `(u128, u32)`, which with alignment is 16 → 32 bytes per interned string in the map (plus hashbrown's control byte either way), and xxh3-128 is marginally slower to compute than xxh3-64. That doubling is why the feature is off by default: the 64-bit key plus verification is the memory-efficient "good enough" trade for regular use. `PreHashed` xor-folds the two halves of a 128-bit key into the 64-bit hash the map wants, so every key bit still participates in shard and bucket selection.

`validate_contents` works identically under both, since it recomputes `hash_key` for every stored string.

Measured on one machine (release; a store of 1M distinct 21-byte strings looked up at random, so most accesses miss the cache; new strings are 2M inserts into a pre-sized store; multi-threaded rows are aggregate throughput):

| | `u64` (default) | `xxh128` |
|---|---|---|
| `insert` of an existing string, 1 thread | 263 ns | 167 ns |
| `idx` | 135 ns | 159 ns |
| `insert` of a new string | 118 ns | 173 ns |
| duplicate inserts, 32 threads | 48 M/s | 72 M/s |
| 16 threads of `get` + duplicate `insert` beside a writer | 45 M/s | 66 M/s |

What `xxh128` saves is the content check: on a store this size the stored string is usually a cache miss into the arena. What it costs is the wider hash and index entries twice as large, which show up in `idx` and in new inserts.

Up to v0.4.1 the feature bought far more under contention: with 64-bit keys every duplicate insert took the store read lock, which queued behind the writer. That version's benchmark (100k strings, 8 threads re-inserting them while a ninth inserted fresh ones) took 1505 ms with `u64` keys and 153 ms with `xxh128`. Reads no longer lock, so that gap is gone; in the same comparison at 32 threads, duplicate inserts went from 12.5 M/s (`u64`) against 75 M/s (`xxh128`) to 48 against 72.

## Index lifetime guarantee

Once a string is inserted, its public index is permanent. There is no removal, shrink, or compaction API — by design. This is what makes the unsafe pointer surface sound; see `unsafe-pointers.md`.

The maximum number of *user-inserted* unique strings is `MAX_USER_STRINGS = u32::MAX - LATIN1_NUM`. The last permitted string lands at public index `u32::MAX - 1` and pushes the `u32` length counter to exactly `u32::MAX`; one more would wrap `len` to 0. `insert_locked` returns `Err(StoreFull)` if the stored count reaches that ceiling, before mutating any state. `try_insert` / `try_insert_many` surface that error to the caller; `insert` / `insert_many` panic on it. (A hash collision panics on *both* paths — it is unrepresentable, not recoverable; see the collision policy above.)

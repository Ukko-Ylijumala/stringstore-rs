# Storage architecture

`UniqueStrStore` is not one container — it is three, glued together by a single index space that hides the seam from callers. Understanding the seam is the prerequisite for touching almost anything else in `src/lib.rs`.

## The three backing containers

`UniqueStrStore` itself is a newtype around `Arc<StoreInner>`; all four fields below live in that one allocation, so `Clone` is a single refcount increment and every access dereferences one pointer.

```text
public index space:    0 ─────────────── 255 │ 256 ────────────── u32::MAX
                       └── ascii Vec ───┘    │ └── store Vec (offset by 256) ──┘
                       (fixed, no lock)      │ (RwLock<Vec<Box<str>>>)

content lookup:        index: DashMap<HashKey xxh3 hash, u32 internal index, PreHashed>
                       (HashKey = u64, or u128 with the `xxh128` feature)
length:                len: AtomicU32  // public length, starts at 256
```

1. **`ascii: Vec<Box<str>>`** — a fixed 256-entry vector populated once at construction with every ISO-8859-1 codepoint as a one-character `Box<str>`. The empty string `""` replaces `'\0'` at index 0, so a NUL string is *not* covered by this table: `return_iso8859_1_cp` rejects codepoint 0 and `"\0"` is interned through the regular hash-indexed path like any other content. Read access skips both the `RwLock` and the hash map entirely.
2. **`store: RwLock<Vec<Box<str>>>`** — the actual interned strings. Internally indexed `0..N`, but every public-facing index is offset by `LATIN1_NUM` (256).
3. **`index: DashMap<HashKey, u32, PreHashed>`** — content-to-position lookup, keyed by the xxh3 hash of the bytes (`HashKey` is `u64` by default, `u128` with the `xxh128` feature; see below). The stored value is the *internal* `store` index (pre-offset). `PreHashed` is an identity `BuildHasher`: the keys are already uniformly distributed 64-bit digests, so the map passes them straight through instead of running them through a second (streaming, 832-byte-state) Xxh3 pass per lookup. That second pass used to cost ~140 ns per map operation versus ~30 ns now; it is the single largest cost that was removed from `contains`/`idx`/`insert`.

`len` is a `std::sync::atomic::AtomicU32` that holds the authoritative public length. It starts at 256 (the ASCII range is always "present"), and is incremented under the write lock in `insert_unchecked` only after a successful new insertion. The increment uses `Release` ordering and `len()` loads with `Acquire`, so observing the new length implies the corresponding push is visible.

## The LATIN1_NUM offset

The constant `LATIN1_NUM = 256` is load-bearing. Any code touching indices must know which space it is in:

| Space | Range | Where it appears |
|---|---|---|
| Public (offset applied) | `0..len()` | All `pub` method args and returns, `idx()`, `get()`, `borrow_str(idx)`, `StoredStr.0`, etc. |
| Internal `store` | `0..store.len()` | DashMap values, the `idx` variable inside `insert_unchecked`, direct `store[i]` access in `reconstruct`. |

Translation points to watch:

- `idx()`: returns `self.index.get(...).map(|r| r.value() + LATIN1_NUM)` — DashMap value is internal, add the offset for the public answer.
- `lookup(idx)` / `get_str_ptr(idx)`: branch on `idx < LATIN1_NUM`. If yes, hit `ascii[idx]` directly. If no, take the read lock and index into `store[idx - LATIN1_NUM]`. `lookup` is the bounds-checked form (`get`, `borrow_str`) and takes the lock exactly once; `get_str_ptr` is the unchecked form for callers that already hold a valid index (`get_ptr`, `StoredStr`).
- `insert_unchecked`: `let idx: u32 = store.len() as u32;` is internal; the return value is `indexed + LATIN1_NUM`.
- `reconstruct`: same branch as `lookup` when fetching each part, under one read lock for the whole rebuild.

## Why the split exists

The motivation is twofold:

1. **Hash/lock avoidance for the common case.** Single-character ISO-8859-1 strings are extremely common in tokenized output (whitespace, punctuation, digits). Routing them through `xxh3 → DashMap → RwLock` would dominate the cost of trivial inserts; the `ascii` short-circuit collapses these to a direct array index.
2. **Stable "well-known" indices.** Callers can assume `0` is always the empty string and `1..256` are the ISO-8859-1 codepoints, regardless of insertion order, without ever calling `insert`. This makes index 0 usable as a sentinel (see `splitting-and-paths.md`).

## Hash-only identity and the collision policy

The `index` DashMap is keyed by the xxh3 hash of the string bytes — within the index, string identity *is* the hash. Two distinct strings colliding on the full key width cannot both be represented. With the default 64-bit key the odds are ~n²/2⁶⁵, roughly 1 in 370k for a store holding 10M strings, which is why the insert path verifies.

The policy as of v0.3.9 (64-bit keys, the default):

- **`insert` verifies.** On a hash hit — both in the fast path and in the lost-race branch inside `insert_unchecked` — the stored string's contents are compared against the incoming string. A mismatch calls `collision_panic`: a deliberate panic, because silently returning the other string's index would corrupt every downstream index vector. There is no graceful recovery without re-keying the index (e.g. `DashMap<u64, SmallVec<u32>>`); revisit only if a collision is ever observed in the wild.
- **`contains` and `idx` do not verify.** They remain pure hash lookups (no lock, no content fetch) to keep the read path free of `RwLock` involvement. Consequence: for a string that was *never inserted* but collides with a stored one, `contains` returns a false positive and `idx` returns the colliding string's index. Strings that went through `insert` are unaffected — the insert-time check guarantees no two *stored* strings share a hash.

Cost of the insert-side check: every duplicate `insert` takes the store read lock and performs one string comparison. The new-string path is unchanged (one hash, one DashMap miss, then the write-locked insert).

### The `xxh128` feature: wider keys, no verification

With `--features xxh128`, `HashKey` becomes `u128` (xxh3-128 via `xxhash_rust::xxh3::xxh3_128`, default secret) and `verify_hit` / `collision_panic` are not compiled at all. The collision odds drop to ~n²/2¹²⁹ (about 10⁻²⁵ at 10M strings), and in exchange:

- The duplicate-insert path returns straight from the DashMap: no store read lock, no string compare. `insert` of an already-interned string becomes as cheap as `idx`.
- The lost-race branch inside `insert_unchecked` likewise trusts the hash.
- `contains`/`idx` false positives become equally negligible.

What it costs: the index entry grows from `(u64, u32)` to `(u128, u32)`, which with alignment is 16 → 32 bytes per interned string in the map (plus hashbrown's control byte either way), and xxh3-128 is marginally slower to compute than xxh3-64. That doubling is why the feature is off by default: the 64-bit key plus verification is the memory-efficient "good enough" trade for regular use. `PreHashed` xor-folds the two halves of a 128-bit key into the 64-bit hash the map wants, so every key bit still participates in shard and bucket selection.

`validate_contents` works identically under both, since it recomputes `hash_key` for every stored string.

Measured on one machine (release, 100k distinct 16-byte strings; the contended row is 8 threads re-inserting all of them 20 times while a ninth thread inserts 200k fresh strings):

| | `u64` (default) | `xxh128` |
|---|---|---|
| `insert` of an existing string, uncontended | 26 ns | 19 ns |
| `idx` / `contains` | 15 / 12 ns | 19 / 17 ns |
| `insert` of a new string | 160 ns | 192 ns |
| contended duplicate inserts alongside a writer | 1505 ms | 153 ms |

The uncontended numbers are a wash: the wider hash costs about what the skipped compare saved. The contended row is the point of the feature — with 64-bit keys every duplicate insert has to acquire the store read lock, which queues behind whoever holds the write lock; with `xxh128` it never touches that lock.

## Index lifetime guarantee

Once a string is inserted, its public index is permanent. There is no removal, shrink, or compaction API — by design. This is what makes the unsafe pointer surface sound; see `unsafe-pointers.md`.

The maximum number of *user-inserted* unique strings is `MAX_USER_STRINGS = u32::MAX - LATIN1_NUM`. The last permitted string lands at public index `u32::MAX - 1` and pushes the `u32` length counter to exactly `u32::MAX`; one more would wrap `len` to 0. `insert_unchecked` returns `Err(StoreFull)` if `store.len()` reaches that ceiling, before mutating any state. `try_insert` surfaces that error to the caller; the public `insert` returns a bare `u32` and so panics on it. (A hash collision panics on *both* paths — it is unrepresentable, not recoverable; see the collision policy above.)

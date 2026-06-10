# Storage architecture

`UniqueStrStore` is not one container — it is three, glued together by a single index space that hides the seam from callers. Understanding the seam is the prerequisite for touching almost anything else in `src/lib.rs`.

## The three backing containers

```text
public index space:    0 ─────────────── 255 │ 256 ────────────── u32::MAX
                       └── ascii Vec ───┘    │ └── store Vec (offset by 256) ──┘
                       (fixed, no lock)      │ (Arc<RwLock<Vec<Box<str>>>>)

content lookup:        index: Arc<DashMap<u64 xxh3 hash, u32 internal index>>
length:                len: Arc<AtomicU32>  // public length, starts at 256
```

1. **`ascii: Arc<Vec<Box<str>>>`** — a fixed 256-entry vector populated once at construction with every ISO-8859-1 codepoint as a one-character `Box<str>`. The empty string `""` replaces `'\0'` at index 0. Read access skips both the `RwLock` and the hash map entirely.
2. **`store: Arc<RwLock<Vec<Box<str>>>>`** — the actual interned strings. Internally indexed `0..N`, but every public-facing index is offset by `LATIN1_NUM` (256).
3. **`index: Arc<DashMap<u64, u32, CustomXxh3Hasher>>`** — content-to-position lookup, keyed by the xxh3 hash of the bytes. The stored value is the *internal* `store` index (pre-offset).

`len` is a `std::sync::atomic::AtomicU32` that holds the authoritative public length. It starts at 256 (the ASCII range is always "present"), and is incremented under the write lock in `insert_unchecked` only after a successful new insertion. The increment uses `Release` ordering and `len()` loads with `Acquire`, so observing the new length implies the corresponding push is visible.

## The LATIN1_NUM offset

The constant `LATIN1_NUM = 256` is load-bearing. Any code touching indices must know which space it is in:

| Space | Range | Where it appears |
|---|---|---|
| Public (offset applied) | `0..len()` | All `pub` method args and returns, `idx()`, `get()`, `borrow_str(idx)`, `StoredStr.0`, etc. |
| Internal `store` | `0..store.len()` | DashMap values, the `idx` variable inside `insert_unchecked`, direct `store[i]` access in `reconstruct`. |

Translation points to watch:

- `idx()`: returns `self.index.get(...).map(|r| r.value() + LATIN1_NUM)` — DashMap value is internal, add the offset for the public answer.
- `get_str_ptr(idx)`: branches on `idx < LATIN1_NUM`. If yes, hit `ascii[idx]` directly. If no, take the read lock and index into `store[idx - LATIN1_NUM]`.
- `insert_unchecked`: `let idx: u32 = store.len() as u32;` is internal; the return value is `indexed + LATIN1_NUM`.
- `reconstruct`: same branch as `get_str_ptr` when fetching each part.

## Why the split exists

The motivation is twofold:

1. **Hash/lock avoidance for the common case.** Single-character ISO-8859-1 strings are extremely common in tokenized output (whitespace, punctuation, digits). Routing them through `xxh3 → DashMap → RwLock` would dominate the cost of trivial inserts; the `ascii` short-circuit collapses these to a direct array index.
2. **Stable "well-known" indices.** Callers can assume `0` is always the empty string and `1..256` are the ISO-8859-1 codepoints, regardless of insertion order, without ever calling `insert`. This makes index 0 usable as a sentinel (see `splitting-and-paths.md`).

## Hash-only identity and the collision policy

The `index` DashMap is keyed by the 64-bit xxh3 hash of the string bytes — within the index, string identity *is* the hash. Two distinct strings colliding on the full 64 bits cannot both be represented (odds are ~n²/2⁶⁵; roughly 1 in 370k for a store holding 10M strings).

The policy as of v0.3.9:

- **`insert` verifies.** On a hash hit — both in the fast path and in the lost-race branch inside `insert_unchecked` — the stored string's contents are compared against the incoming string. A mismatch calls `collision_panic`: a deliberate panic, because silently returning the other string's index would corrupt every downstream index vector. There is no graceful recovery without re-keying the index (e.g. `DashMap<u64, SmallVec<u32>>`); revisit only if a collision is ever observed in the wild.
- **`contains` and `idx` do not verify.** They remain pure hash lookups (no lock, no content fetch) to keep the read path free of `RwLock` involvement. Consequence: for a string that was *never inserted* but collides with a stored one, `contains` returns a false positive and `idx` returns the colliding string's index. Strings that went through `insert` are unaffected — the insert-time check guarantees no two *stored* strings share a hash.

Cost of the insert-side check: every duplicate `insert` now takes the store read lock and performs one string comparison. The new-string path is unchanged (one hash, one DashMap miss, then the write-locked insert).

## Index lifetime guarantee

Once a string is inserted, its public index is permanent. There is no removal, shrink, or compaction API — by design. This is what makes the unsafe pointer surface sound; see `unsafe-pointers.md`.

The maximum number of *user-inserted* unique strings is `u32::MAX - LATIN1_NUM + 1`. `insert_unchecked` returns `Err(StoreFull)` if `store.len()` reaches that ceiling, before mutating any state. `try_insert` surfaces that error to the caller; the public `insert` returns a bare `u32` and so panics on it. (A hash collision panics on *both* paths — it is unrepresentable, not recoverable; see the collision policy above.)

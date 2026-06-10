# Concurrency model

`UniqueStrStore` is thread-safe and cheap to `Clone` (all backing state is in `Arc`s). The interesting decisions are in the **insert path** and in **how reads escape the lock**. This doc focuses on inserts; see `unsafe-pointers.md` for the read-side soundness story.

## Locking primitives

- `store`: `parking_lot::RwLock<Vec<Box<str>>>` — writers exclusive, readers shared.
- `index`: `dashmap::DashMap<u64, u32, CustomXxh3Hasher>` — internally sharded, lock-free for non-conflicting keys.
- `len`: `std::sync::atomic::AtomicU32` — atomic public length counter. Incremented with `Release` (under the write lock, after the push); `len()` loads with `Acquire`, so a reader that observes the new length is guaranteed to see the pushed element.
- `ascii`: no synchronization — built once at construction, never mutated.

The `RwLock` and `DashMap` are independent locks. The invariant they jointly uphold (every entry in `index` points to a valid slot in `store` with a matching hash) is preserved by the **lock ordering and post-lock recheck** described below.

## The fast path: read-only `idx()` / `contains()`

For non-ASCII content the read path is:

```text
hash_bytes(s) ── DashMap::get ──► Option<u32 internal index>
```

No `RwLock` involvement at all. The DashMap value alone is sufficient — we do not need to dereference into `store` to answer "does this string exist?" or "what is its index?".

This is also what makes `contains` and `idx` close to free under contention: a write lock on `store` does not stall them.

## The insert path

```text
                       ┌─── empty? ──► return 0
        insert(s) ─────┼─── single ISO-8859-1 char? ──► return codepoint index
                       │
                       │    key = hash_bytes(s)
                       └─── index.get(&key) hit? ──► take store READ lock,
                       │         compare contents:                (fast path,
                       │           equal    ──► return existing index    no write lock)
                       │           mismatch ──► collision_panic
                       │
                       └── miss ──► insert_unchecked(s, key):
                                      1. take store write lock
                                      2. idx = store.len() as u32        // tentative internal index
                                      3. indexed = *index.entry(key).or_insert(idx)
                                      4. if indexed == idx:
                                            store.push(s.into())
                                            len.fetch_add(1)
                                         else:
                                            compare store[indexed] vs s   // lost the race —
                                            mismatch ──► collision_panic  // same string or collision?
                                      5. return indexed + LATIN1_NUM
```

The hash is computed once (in `insert_internal`, shared by `insert` and `try_insert`) and passed into `insert_unchecked`. `try_insert` is the identical flow with one difference: a full store surfaces as `Err(StoreFull)` instead of a panic.

The whole path borrows: `insert<T: AsRef<str>>` never copies the input string. The single allocation is the `Box<str>` created inside `store.push(s.into())` — and only on the thread that actually inserts. The already-interned case (the hot path) allocates nothing.

### Why the recheck after taking the write lock

Step 3 is the atomic decision point — **not** the earlier hash lookup in the public `insert`. Between that lookup and acquiring the write lock, another thread can win the race and insert the same string. The DashMap `entry().or_insert(idx)` resolves this: only the thread whose `idx` was actually inserted into the map is permitted to push into `store`.

If a refactor moves the hash-hit check inside the write-lock region, the recheck still has to remain — two threads can both miss before *either* takes the write lock.

### The lock-ordering rule in the fast path

The fast path copies the `u32` out of the DashMap guard (`self.index.get(&key).map(|r| *r.value())`) **before** taking the store read lock. Holding a shard guard across the store lock acquisition would invert the store → index lock order used by `insert_unchecked` (which takes the store write lock first, then touches the DashMap) and deadlock under contention. Keep it that way.

The copied index stays valid after the guard drops: index entries are never modified or removed, and the slot is guaranteed to be populated by the time the read lock is acquired — the entry is only ever published inside the same write-lock critical section that pushes the string.

### Why the write lock is taken before consulting the DashMap

Acquiring the `RwLock` write lock first guarantees that the `idx = store.len()` reservation cannot be invalidated by a concurrent `store.push`. If we touched the DashMap first, a different thread could push into `store` between our `len()` snapshot and our own push, corrupting the index-to-slot mapping.

Concretely: the lock pins `store.len()` for the entire critical section, so `idx` is both the tentative DashMap value *and* the actual slot the push will land in.

## What survives if the inserting thread loses the race

The losing thread:

- Allocated nothing (`insert<T: AsRef<str>>` only borrows the input).
- Did not call `store.push`.
- Did not call `len.fetch_add`.
- Compared its string against the winner's (collision check; panics on mismatch).
- Returns the *winner's* index.

No `Box<str>` is allocated on the losing path — the only allocation on the entire insert path happens inside the winner's `store.push(s.into())`.

## Transient `idx()`/`get()` disagreement during insert

Inside `insert_unchecked`, the DashMap entry is published (step 3) *before* `store.push` completes (step 4) — both inside the write-lock critical section, but the DashMap is readable without that lock. A concurrent bare `idx(s)` can therefore return an index for which `get(idx)` momentarily returns `Err(IndexOutOfBounds)`: `get` consults the `len` counter, which is incremented last. The window closes when the writer releases the lock.

Nothing dangles — `borrow_str`/`get_str_ptr` block on the read lock, so they cannot observe the half-inserted state; the disagreement is only between `idx()`'s answer and `get()`'s bounds check. The `insert` fast path is immune: it acquires the read lock before dereferencing, which serializes it after the writer's critical section. Callers who treat `idx() == Some(i)` as a promise that `get(i)` succeeds *right now* (rather than eventually) are the only ones who can notice.

## `validate_contents` takes a write lock

`validate_contents` acquires `store.write()`, not `store.read()`. This is intentional: it freezes both `store.len()` and the DashMap snapshot it iterates so they are consistent. Calling `validate_contents` from one thread while another is hammering `insert` will serialize against the writer.

In **debug builds** the function panics with the error list on any inconsistency; in release it returns `Err(Vec<String>)`. The concurrent-insert tests (`test_concurrent_inserts`, `test_competing_inserts`) call it as a `.ok()`-style assertion to rely on the debug-mode panic.

## What you must not do

- **Do not** call `insert` while holding the `store` write lock externally — there is no external way to take that lock, and any future API that exposed one would deadlock against `insert_unchecked`.
- **Do not** reorder the steps in `insert_unchecked` so that `store.push` runs before the DashMap `entry` resolves. The `idx == store.len()` invariant relies on the push happening exactly when (and only when) the DashMap entry is fresh.
- **Do not** add a path that increments `len` without pushing, or pushes without incrementing `len`. The `validate_contents` check `store.len() + LATIN1_NUM == len()` catches this, but only after the fact.

# Concurrency model

`UniqueStrStore` is thread-safe and cheap to `Clone` (all backing state is in one `Arc`). **Reads take no lock at all**; only inserts serialize, on a mutex. The interesting decisions are in the **insert path** and in **how a reader finds a slot without a lock**, including the one moment when a published index is not yet readable. See `unsafe-pointers.md` for why the returned references stay valid.

## Synchronization primitives

- `store`: `parking_lot::Mutex<StrArena>` — the arena chunks that hold the string bytes. Taken only by inserts and by whole-store walks (`Debug`, `validate_contents`, `SizeOf`). No read takes it, except the rare wait for an in-flight copy described below.
- `slots`: `SlotTable` — one `*const str` per user string, in fixed segments of doubling size (`32 << k` slots in segment `k`) that are never moved or freed while the store lives. Written only by the mutex holder, read by anyone without locking.
- `index`: `dashmap::DashMap<HashKey, u32, PreHashed>` — internally sharded, lock-free for non-conflicting keys. `PreHashed` is an identity hasher (the keys are xxh3 digests already; see `storage-architecture.md`).
- `len`: `std::sync::atomic::AtomicU32` — the public length, and the publication point for slots. The mutex holder writes a slot, then stores `len` past it with `Release`; a reader loads `len` with `Acquire` before it reads a slot below it. Only the mutex holder writes `len`, so it is a plain `store`, not a read-modify-write.
- `ascii`: no synchronization — built once at construction, never mutated.

The invariant these uphold together — every entry in `index` points to a slot that is, or is about to be, written with a string of matching hash — is preserved by the **lock ordering, the post-lock recheck, and doing every fallible step before an entry is published**, all described below.

## Reads

### Content lookup: `idx()` / `contains()`

For non-ASCII content the read path is:

```text
hash_bytes(s) ── DashMap::get ──► Option<u32 internal index>
```

The DashMap value alone answers "does this string exist?" and "what is its index?"; no slot is read.

### Index lookup: `get`, `borrow_str`, `reconstruct`, `StoredStr`

```text
get(idx) ─┬─ idx < 256?                ──► ascii[idx]
          ├─ idx < len.load(Acquire)?  ──► slots.read(idx - 256)       (no lock, no atomic RMW)
          ├─ idx == len?               ──► may be in flight: wait, then re-check
          └─ otherwise                 ──► Err(IndexOutOfBounds)
```

Up to v0.4.1 every such read took the store `RwLock` in read mode: two atomic read-modify-writes on one shared cache line, which bounced between cores and made reads *slower* as threads were added. Measured on one machine (release, 1M distinct strings, random indices, aggregate throughput):

| threads | `get`, v0.4.1 | `get`, lock-free |
|---|---|---|
| 1 | 43 M/s | 63 M/s |
| 32 | 12.5 M/s | 1230 M/s |

The same change lifted duplicate inserts from 12.5 to ~50 M/s at 32 threads (64-bit keys, whose content check now reads the slot without a lock), and readers running beside a writer from 5.6 to ~46 M/s at 16 threads, with the writer itself going from 0.4 to 1.3 M inserts/s.

`get` and the internal `slot` keep the wait in a `#[cold]`, `#[inline(never)]` function reached by a tail call. A plain call there made the compiler save three registers on *every* read, which cost `get` about a quarter of its throughput at 32 threads; with the tail call the inlined fast path needs no saves. The `StoredStr` trait impls are `#[inline]` so this fast path also inlines into other crates.

### The one index that may be in flight

Inserts publish the index entry *before* copying the string (see "Why publish first" below). So a reader can learn an index — from `idx`, from the insert fast path, from a `StoredStr` built on either — a moment before its slot is written. Only the mutex holder inserts, one string at a time, so the only index that can be in that state is `len` itself.

- **An index the store handed out** (`slot`, used by `StoredStr`, `get_ptr` and the duplicate-insert check) is known to be published. If it is not below `len`, the reader spins up to `SLOT_WAIT_SPINS` rounds on `len`, then takes and releases the mutex: the insert that published it writes the slot before it unlocks, so after the lock the slot is readable. The writer allocates nothing in that window (except for a string longer than a chunk): the wait covers one memcpy and one slot write.
- **An index the caller supplied** (`get`, `borrow_str`, `reconstruct`) may not be published at all. For `idx == len`, the reader spins only while the mutex is held, then takes and releases the mutex either way, and re-checks `len`: below it means the string was in flight, otherwise the index is out of bounds. Seeing the mutex free with a plain load proves nothing on its own — the insert may have finished after the reader's `len` load without the reader seeing its store; `test_reader_follows_writer` caught exactly that in an earlier version. Taking the lock supplies the missing synchronization.

A reader never reads an unwritten slot and never gets `IndexOutOfBounds` for an index the store has handed out.

## The insert path

```text
                       ┌─── empty? ──► return 0
        insert(s) ─────┼─── single ISO-8859-1 char? ──► return codepoint index
                       │
                       │    key = hash_bytes(s)
                       └─── index.get(&key) hit? ──► read the slot (no lock),     (fast path)
                       │         compare contents:
                       │           equal    ──► return existing index
                       │           mismatch ──► collision_panic
                       │         (`xxh128` feature: no slot read, no compare —
                       │          return the index straight away)
                       │
                       └── miss ──► insert_unchecked(s, key):
                                      1. lock the store mutex
                                      2. idx = len - LATIN1_NUM           // tentative internal index
                                                                          // (full? ──► Err(StoreFull))
                                      3. arena.reserve(s.len())           // everything that can fail:
                                         slots.ensure(idx)                //   a new chunk, a new segment
                                      4. match index.entry(key):
                                           Vacant(v)   ──► v.insert(idx)             // publish
                                                           ptr = arena.push(s)       // memcpy, cannot unwind
                                                           slots.write(idx, ptr)
                                                           len.store(.., Release)    // slot now readable
                                                           return idx + LATIN1_NUM
                                           Occupied(i) ──► compare slot i vs s       // lost the race —
                                                           mismatch ──► collision_panic
                                                           return i + LATIN1_NUM
```

The hash is computed once (in `find_or_hash`, the lock-free first half shared by `insert`, `try_insert` and the batched inserts below) and passed into `insert_unchecked`, which takes the mutex and runs steps 2-4 as `insert_locked`. `try_insert` is the identical flow with one difference: a full store surfaces as `Err(StoreFull)` instead of a panic.

The whole path borrows: `insert<T: AsRef<str>>` never copies the input string until `arena.push(s)` memcpys it into the arena — and only on the thread that actually inserts. That copy allocates nothing; a fresh chunk (128 KiB by default) or slot segment is allocated in step 3 only when the current one is full, and a string longer than a chunk gets a dedicated allocation inside `push`. The already-interned case (the hot path) allocates nothing.

### Batched inserts: `insert_many` / `try_insert_many`

A batch runs the same two halves, just regrouped: `find_or_hash` resolves every string of the batch lock-free first (hits, the empty string and single ISO-8859-1 characters never reach the lock), then the remaining strings are inserted one after another by `insert_locked` under **one** mutex hold. A batch of hits only never takes the lock.

- **Each `insert_locked` completes before the next starts** — entry, copy, slot write and `len` store. A later string of the batch may duplicate an earlier one (both missed in the first half); its recheck finds the entry occupied and compares against that slot, which is then already below `len`. That keeps the "no slot at or past `len` under the mutex" rule intact without any special casing. Do not "optimize" a batch into publishing several entries before copying.
- **Lock ordering is unchanged**: mutex → shard, one shard at a time, no guard held across strings.
- **Failure**: the only error is `StoreFull`. Strings before the one that did not fit stay inserted (each was a complete insert); nothing of the failing string is published.
- **The cost is the hold time.** A reader that has to wait out an in-flight index (it learned the index from `idx` a moment before the copy) spins on `len` first, which normally suffices — the copy is one memcpy away. Only a reader that exhausts its spins and falls back to taking the mutex waits for the rest of the batch; so do other writers. Batches of tens to a few hundred strings keep that bounded.

The mutex handoff between writers is what the batch saves, and that cost grows with the number of writers. Measured on one machine (release, 2M distinct strings split over the threads, aggregate throughput; batches of 64):

| threads | `insert` | `insert_many` |
|---|---|---|
| 1 | 6.8 M/s | 7.2 M/s |
| 4 | 5.5 M/s | 5.8 M/s |
| 8 | 3.1 M/s | 6.9 M/s |
| 16 | 1.2 M/s | 6.4 M/s |
| 32 | 0.9 M/s | 5.6 M/s |

In a parallel directory walker (statter) interning file names per 64-entry batch, the same change cut a 200k-file scan at 16 workers from 0.15 s to 0.04 s.

### Why the recheck after taking the mutex

Step 4 is the atomic decision point — **not** the earlier hash lookup in the public `insert`. Between that lookup and acquiring the mutex, another thread can win the race and insert the same string. The DashMap `entry()` resolves this: only the thread that finds the entry vacant, and fills it with its `idx`, is permitted to write the slot.

If a refactor moves the hash-hit check inside the mutex region, the recheck still has to remain — two threads can both miss before *either* takes the mutex.

Under the `xxh128` feature the compare in step 4's occupied branch and in the fast path is compiled out (`verify_hit` does not exist); the `entry()` decision point is unchanged, so the race handling is identical.

### Nothing may fail between publishing and writing the slot

Neither lock poisons. If anything between `v.insert` and the `len` store unwound, the store would keep an entry pointing at a slot that never gets written. A reader of that index waits for the mutex, which the unwind releases, and then reads the unwritten slot: undefined behavior from safe code (before the lock-free reads, the same bug read out of bounds through `StoredStr`'s unchecked lookup). Up to v0.4.0 that was reachable: the chunk allocation ran inside `push`, and a `chunk_size` above `isize::MAX` made it panic with a capacity overflow.

So step 3 runs every step of an append that can fail — the new chunk and the new slot segment — while nothing is published yet. After it, `push` only copies bytes into reserved room, and the slot write lands in an existing segment. The one allocation left in `push` is the exact-size chunk of a string longer than `chunk_size`, which cannot unwind: a `&str` is at most `isize::MAX` bytes, so its layout is always valid, and a failed allocation aborts. `new_with_capacity` also clamps `chunk_size` to 1 GiB, so the chunk allocation in `reserve` cannot overflow either. `test_failed_append_publishes_nothing` and `test_reserve_front_loads_allocation` cover both halves.

### Why publish first

Publishing after the copy needs no reader-side waiting at all, and was measured twice: with the old `RwLock` (a fresh `insert` into a store of 2M strings took ~145 ns instead of ~115 ns) and again with lock-free reads (~147 ns instead of ~120 ns), single-threaded, with no difference at 100k strings where the index fits in cache. The extra time was memory stalls rather than instructions, and every variant paid it that either did arena work between the entry lookup and the entry insert or copied the string before the entry insert; the exact cause was not pinned down.

A readiness counter (a second atomic, bumped at publish and caught up by `len` after the copy) was also measured: its one extra store cost ~30 ns per fresh insert, wherever it was placed. Using the mutex as the wait primitive costs the writer nothing, because the only index that can be in flight is `len`.

The trade-off: a benchmark in which readers busy-poll `idx` for strings a writer is still inserting ran slower with lock-free reads than with the `RwLock` (which had throttled those readers), and slower still with publish-first. A pipeline in which readers receive indices the writer has finished inserting showed no such penalty.

### The lock-ordering rule

The order is **store mutex → index shard**: `insert_locked` runs with the mutex held and takes a shard lock in `entry()`.

The fast path copies the `u32` out of the DashMap guard (`self.index.get(&key).map(|r| *r.value())`) **before** reading the slot. Reading a slot can wait on the mutex (an in-flight copy); holding a shard guard during that wait inverts the order — the mutex may by then belong to the *next* insert, which could be waiting for exactly that shard — and deadlocks. Keep it that way everywhere a slot is read.

The copied index stays valid after the guard drops: index entries are never modified or removed.

### Why the mutex is taken before consulting the DashMap

Holding the mutex first guarantees that the `idx = len - LATIN1_NUM` reservation cannot be invalidated by a concurrent insert. If we touched the DashMap first, a different thread could claim the same slot between our `len` snapshot and our write, corrupting the index-to-slot mapping.

Concretely: the mutex pins `len` for the entire critical section, so `idx` is both the tentative DashMap value *and* the slot the write will land in.

## What survives if the inserting thread loses the race

The losing thread:

- Allocated nothing for its string (`insert<T: AsRef<str>>` only borrows the input; step 3 may have started a chunk or segment, which the next insert uses).
- Did not copy the string, write a slot, or store `len`.
- Compared its string against the winner's (collision check; panics on mismatch — 64-bit keys only).
- Returns the *winner's* index.

## `idx()`, `len()` and `get()` agree, even mid-insert

The entry is published (step 4, `v.insert`) *before* the slot is written, and the DashMap is readable without the mutex. A concurrent bare `idx(s)` can therefore return `len` itself while `len()` still reports the old length.

That index is never an error through the read paths: `get`, `borrow_str`, `reconstruct` and `StoredStr` all wait out the copy, as described in "The one index that may be in flight". `len()` lags by at most that one index, and remains a safe lower bound on what `get` will accept. `test_reader_follows_writer` has readers chase a writer through every read path.

## `validate_contents` takes the mutex

`validate_contents` locks the store mutex, which keeps inserts out: `len` and the index hold still while it walks both. Calling it from one thread while another is hammering `insert` serializes against the writers.

In **debug builds** the function panics with the error list on any inconsistency; in release it returns `Err(Vec<String>)`. The concurrent-insert tests (`test_concurrent_inserts`, `test_competing_inserts`, `test_reader_follows_writer`) call it as a `.ok()`-style assertion to rely on the debug-mode panic.

## What you must not do

- **Do not** read a slot at or past `len` while holding the store mutex. The mutex is not reentrant, and the wait for an in-flight copy takes it: the holder would deadlock on itself. Code that runs under the mutex (the occupied branch of `insert_unchecked`, `Debug`, `validate_contents`) only reads indices below `len`; there is also no external way to take the mutex.
- **Do not** hold a DashMap shard guard while reading a slot; see "The lock-ordering rule".
- **Do not** reorder the steps in `insert_locked` so that the copy runs before the DashMap `entry` resolves. The `idx == len` invariant relies on the slot being written exactly when (and only when) the entry is fresh.
- **Do not** start the next string of a batch before the previous one's `len` store; see "Batched inserts".
- **Do not** add anything that can panic or otherwise unwind between `v.insert(idx)` and the `len` store. Fallible work (allocation, `Vec` growth) belongs in step 3, before the entry is published.
- **Do not** store `len` before the slot is written, or write a slot that `len` has already passed. `len` is the only thing that makes a slot visible to lock-free readers.
- **Do not** add a path that stores `len` without writing a slot, or writes a slot without storing `len`. The `validate_contents` check that the stored count equals `index.len()` catches this, but only after the fact.

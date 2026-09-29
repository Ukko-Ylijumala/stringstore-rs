# Unsafe pointer surface

`UniqueStrStore` hands out three different "borrowed view" types of an interned string. Two of them are safe to use; one bypasses the lifetime system entirely. All three are sound only because of a specific append-only invariant. This doc explains the contract.

## The four ways to read a stored string

| API | Return | Lifetime tracked? | Lock taken? |
|---|---|---|---|
| `get(idx) -> Result<&str>` | bounds-checked `&'a str` | yes (tied to `&self`) | no |
| `borrow_str(idx) -> &str` *(unsafe)* | bounds-checked `&'a str`, panics out of bounds | yes (tied to `&self`) | no |
| `get_ptr(idx) -> StoredStrPtr` *(unsafe)* | raw `*const str` wrapper, unchecked | **no** | no |
| `StoredStr<'a>` (returned by `get_ref`/`insert_or_get`) | safe handle holding `&'a UniqueStrStore` | yes | no |

No read takes a lock, except the brief wait for an in-flight copy (see `concurrency.md`, "The one index that may be in flight"). Two internal building blocks sit underneath: `lookup` (bounds-checked against `len`, returns `Option<*const str>`) for `get`, `borrow_str` and `reconstruct`, and `slot` / `get_str_ptr` (unchecked) for `get_ptr`, `StoredStr` and the duplicate-insert check, which only ever hold indices the store itself handed out.

## Why the references stay valid without a lock

A read loads the slot's `*const str` out of the `SlotTable` and hands it back as `&'a str` (in `get`, `borrow_str`, `StoredStr`) or wraps it in `StoredStrPtr` (in `get_ptr`). Nothing is locked while the reference lives, yet it stays valid. Two things make that sound:

- **The slot itself never moves.** The `SlotTable` is a fixed array of segments; a segment is allocated once and freed only when the store drops, so a reader can load a slot while a writer adds more. (A `Vec` of slots could not be read this way: growing it moves its buffer.) A slot is written exactly once, before `len` is released past it, and a reader loads it only after an `Acquire` load of `len` shows it — or after waiting out the insert that published it. That pairing is the whole synchronization; the slots are plain memory.
- **The pointer addresses bytes inside an arena chunk**, and a chunk is never resized, moved or dropped while the store is alive, because **the store never removes, replaces, or shrinks**. The chunk list (`Vec<*mut [u8]>`) may reallocate when it grows; that moves the chunk *pointers*, never the bytes. Once a string is copied into a chunk, that chunk lives until the entire `UniqueStrStore` is dropped.

(Before the arena the same argument held with one `Box<str>` per string, and until v0.4.1 the slot vector sat behind an `RwLock` that every read took; the shape of the invariant is unchanged.)

### Why the chunks are raw pointers, not `Box<[u8]>`

Address stability is not enough: the pointers also must not be invalidated by Rust's aliasing rules. A `Box<[u8]>`, and any `&mut [u8]` taken through it, asserts exclusive access to *every* byte of the chunk. Up to v0.4.0 the arena held `Box<[u8]>` chunks and appended through `chunks.last_mut()`, which borrows the whole chunk mutably; that invalidated every `&str` already handed out into the chunk. The oversized path took a pointer from a fresh `Box` and then moved the `Box` into its list, which invalidated that pointer too. Miri's default model (Stacked Borrows) reported both as undefined behavior on the existing tests; Tree Borrows accepted them, and no miscompile was observed.

`StrArena` now holds each chunk as the raw pointer `Box::into_raw` returns, appends with `ptr::copy_nonoverlapping` into the unfilled tail, derives each slot from that raw pointer, and turns the chunks back into `Box`es only in `Drop`. Once a slot points into a chunk, no reference to that chunk as a whole is ever created again.

Check any change in this area under Miri (nightly `miri` component; slow — the two skipped tests insert 100k strings each):

```sh
cargo +nightly miri test -- --skip test_concurrent_inserts --skip test_competing_inserts
```

The borrow checker is satisfied for `borrow_str` because the returned `&'a str` is reborrowed through `&'a self`, so Rust treats it as having the store's lifetime.

## The append-only contract

The soundness story above relies entirely on these invariants:

1. **No removal.** No `remove`, `pop`, `clear`, `truncate`, `shrink_to_fit`, `drain`, or any other operation that would drop or shrink an arena chunk before the store itself.
2. **No replacement.** No method that rewrites a slot or reuses chunk bytes, which would invalidate any outstanding pointer to the old contents.
3. **No interior mutation of stored strings.** Chunk bytes are written exactly once, inside `StrArena::push`, through a raw pointer, before the slot is written; each slot is written exactly once, before `len` passes it; preserve both. Do not introduce anything that creates a `&mut str` or `&mut [u8]` into a chunk, or a `Box` of one before `Drop` — not even transiently to write the unfilled tail (see above).

If any of these need to change, the unsafe APIs must be rethought from scratch — most likely by switching to reference-counted slots, or to an epoch or hazard-pointer scheme that tells a writer when no reader can still hold the old pointer. Reads take no lock, so there is no lock to tie a pointer's lifetime to.

## `StoredStrPtr` lifetime is the caller's problem

`StoredStrPtr` wraps a `*const str` with no lifetime parameter. It implements `Clone`, `Copy`-shaped patterns, `Hash`, `Ord`, `Display`, `Deref<Target = *const str>`, and `From<StoredStrPtr> for &'a str` for *any* `'a`. None of this is checked.

> $\color{red}{\textbf{WARNING:}}$ **the `From<StoredStrPtr> for &'a str` impl has a completely unconstrained target lifetime.** Safe code can use it to conjure a `&'static str` with no compile-time tie to the originating store. This is *intentional*: a `UniqueStrStore` is meant to live for the remainder of the program — effectively `'static` — so the pointer is assumed to never dangle. If a store in your application is **not** program-lifetime, do not use this conversion (or `StoredStrPtr` at all); the resulting reference can outlive the storage, and using it afterwards is undefined behavior.

The documented contract is: **the pointer is valid only as long as the originating `UniqueStrStore` is alive**. Concretely:

- Storing a `StoredStrPtr` in a `'static` collection is sound *only* if the originating store is itself in a `'static` location (e.g. behind a `OnceLock` or `lazy_static`).
- `StoredStrPtr` wraps a raw pointer, so it is automatically `!Send` and `!Sync`: it cannot be moved to or shared with another thread at all, and the crate deliberately provides no `unsafe impl` to change that. The `&str` you get out of it (`as_str`, `From<StoredStrPtr>`) *is* `Send + Sync`, so the "thread outlives the store" hazard still exists through that conversion — be careful.
- Cloning the store (`Arc` clone) keeps the pointer valid as long as *any* clone is live, because all clones share the same underlying arena chunks.

`StoredStr<'a>` is the safe alternative for almost every use case: it carries a `&'a UniqueStrStore` so the lifetime is checked, at the cost of an extra word per handle.

## `borrow_str`'s panic check

```rust
match self.lookup(idx) {
    Some(ptr) => &*ptr,
    None => panic!("Store index {idx} out of bounds (max: {})", self.len() - 1),
}
```

`lookup` returns `None` for any `idx >= len` that is not an in-flight insert, so `idx == LATIN1_NUM` (256) on an empty user-string store panics rather than reaching an unwritten slot. Indices in `0..LATIN1_NUM` are always valid (the `ascii` vector is fixed-size and pre-populated) and never look at `len`.

Even with the bounds check, `borrow_str` is `unsafe` because the returned `&str` is not tied to anything that keeps the append-only contract honest — callers must uphold it. Use `get` for any path where the index is not statically known to be valid.

## Recommended use by call site

- **Library consumers**: prefer `get` (returns `Result`) or `StoredStr` (returned by `insert_or_get`). These compose with normal Rust lifetimes.
- **Hot internal loops** (e.g. inside `reconstruct`): `borrow_str` with locally-verified indices is fine.
- **Cross-structure references** (e.g. embedding an interned string in another long-lived data structure): use `StoredStr<'a>` if the structure can carry the lifetime, or `StoredStrPtr` plus a documented store-lifetime invariant if it cannot.

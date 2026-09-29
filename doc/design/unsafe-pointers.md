# Unsafe pointer surface

`UniqueStrStore` hands out three different "borrowed view" types of an interned string. Two of them are safe to use; one bypasses the lifetime system entirely. All three are sound only because of a specific append-only invariant. This doc explains the contract.

## The four ways to read a stored string

| API | Return | Lifetime tracked? | Lock held? |
|---|---|---|---|
| `get(idx) -> Result<&str>` | bounds-checked `&'a str` | yes (tied to `&self`) | no (released before return) |
| `borrow_str(idx) -> &str` *(unsafe)* | unchecked `&'a str` | yes (tied to `&self`) | no |
| `get_ptr(idx) -> StoredStrPtr` *(unsafe)* | raw `*const str` wrapper | **no** | no |
| `StoredStr<'a>` (returned by internal `get_ref`/`insert_or_get`) | safe handle holding `&'a UniqueStrStore` | yes | no |

Two internal building blocks sit underneath: `lookup` (bounds-checked, one read-lock acquisition, returns `Option<*const str>`) for `get` and `borrow_str`, and `get_str_ptr` (unchecked) for `get_ptr` and `StoredStr`, which only ever hold indices the store itself handed out.

## Why the references can outlive the read lock

`get_str_ptr` does this:

```rust
let store = self.store.read();                 // read guard on the StrArena
store.get_unchecked(i) as *const str           // the slot: a pointer into a chunk
```

The returned `*const str` is then handed back as `&'a str` (in `borrow_str`) or wrapped in `StoredStrPtr` (in `get_ptr`). The read guard is dropped at the end of the function — yet the pointer is still considered valid.

This is sound because the pointer addresses **bytes inside an arena chunk owned by the `StrArena`**, not the slot vector's buffer:

- The slot `Vec<*const str>` and the chunk list `Vec<Box<[u8]>>` may reallocate their buffers when growing. That moves the slots and the `Box` *handles* — but the chunk bytes stay where they are on the heap.
- A chunk is never resized, moved or dropped while the store is alive, because **the store never removes, replaces, or shrinks**. Once a string is copied into a chunk, that chunk lives until the entire `UniqueStrStore` is dropped.

(Before the arena the same argument held with one `Box<str>` per string; the shape of the invariant is unchanged.)

The borrow checker is satisfied for `borrow_str` because the returned `&'a str` is reborrowed through `&'a self`, so Rust treats it as having the store's lifetime.

## The append-only contract

The soundness story above relies entirely on these invariants:

1. **No removal.** No `remove`, `pop`, `clear`, `truncate`, `shrink_to_fit`, `drain`, or any other operation that would drop or shrink an arena chunk before the store itself.
2. **No replacement.** No method that rewrites a slot or reuses chunk bytes, which would invalidate any outstanding pointer to the old contents.
3. **No interior mutation of stored strings.** Chunk bytes are written exactly once, inside `StrArena::push`, before the slot is published; preserve this. Do not introduce anything that exposes `&mut str` or `&mut [u8]` into a chunk after that.

If any of these need to change, the unsafe APIs must be rethought from scratch — most likely by switching to reference-counted slots or by making the unsafe APIs require an explicit guard type that ties the pointer's lifetime to the read lock.

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

`lookup` returns `None` when `store.get(idx - LATIN1_NUM)` does, evaluated under the read lock, so `idx == LATIN1_NUM` (256) on an empty user-string store panics rather than reaching an unchecked slot. Indices in `0..LATIN1_NUM` are always valid (the `ascii` vector is fixed-size and pre-populated) and never touch the lock. An earlier version did the length check and the pointer fetch as two separate read-lock acquisitions; they are now one.

Even with the bounds check, `borrow_str` is `unsafe` because the returned `&str` outlives the read lock — callers must uphold the append-only contract described above. Use `get` for any path where the index is not statically known to be valid.

## Recommended use by call site

- **Library consumers**: prefer `get` (returns `Result`) or `StoredStr` (returned by `insert_or_get`). These compose with normal Rust lifetimes.
- **Hot internal loops** (e.g. inside `reconstruct`): `borrow_str` with locally-verified indices is fine.
- **Cross-structure references** (e.g. embedding an interned string in another long-lived data structure): use `StoredStr<'a>` if the structure can carry the lifetime, or `StoredStrPtr` plus a documented store-lifetime invariant if it cannot.

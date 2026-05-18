# Unsafe pointer surface

`UniqueStrStore` hands out three different "borrowed view" types of an interned string. Two of them are safe to use; one bypasses the lifetime system entirely. All three are sound only because of a specific append-only invariant. This doc explains the contract.

## The four ways to read a stored string

| API | Return | Lifetime tracked? | Lock held? |
|---|---|---|---|
| `get(idx) -> Result<&str>` | bounds-checked `&'a str` | yes (tied to `&self`) | no (released before return) |
| `borrow_str(idx) -> &str` *(unsafe)* | unchecked `&'a str` | yes (tied to `&self`) | no |
| `get_ptr(idx) -> StoredStrPtr` *(unsafe)* | raw `*const str` wrapper | **no** | no |
| `StoredStr<'a>` (returned by internal `get_ref`/`insert_or_get`) | safe handle holding `&'a UniqueStrStore` | yes | no |

`get_str_ptr` is the internal building block all four sit on top of.

## Why the references can outlive the read lock

`get_str_ptr` does this:

```rust
let store = self.store.read();             // read guard
let b: &Box<str> = store.get_unchecked(i); // borrow inside the Vec
b.as_ref() as *const str                   // pointer to the heap str
```

The returned `*const str` is then handed back as `&'a str` (in `borrow_str`) or wrapped in `StoredStrPtr` (in `get_ptr`). The read guard is dropped at the end of the function — yet the pointer is still considered valid.

This is sound because the pointer addresses the **heap allocation owned by the `Box<str>`**, not the `Vec`'s buffer:

- The `Vec<Box<str>>` may reallocate its internal buffer when growing. That moves the `Box` *handles* — but the boxed `str` contents stay where they are on the heap.
- The `Box<str>` itself is never dropped while the store is alive, because **the store never removes, replaces, or shrinks**. Once pushed, a `Box<str>` lives until the entire `UniqueStrStore` is dropped.

The borrow checker is satisfied for `borrow_str` because the returned `&'a str` is reborrowed through `&'a self`, so Rust treats it as having the store's lifetime.

## The append-only contract

The soundness story above relies entirely on these invariants:

1. **No removal.** No `remove`, `pop`, `clear`, `truncate`, `shrink_to_fit`, `drain`, or any other operation that would drop a `Box<str>` before the store itself.
2. **No replacement.** No method that reassigns `store[i] = new_box`, which would drop the old `Box<str>` and invalidate any outstanding pointer to its contents.
3. **No interior mutation of stored strings.** `Box<str>` is fundamentally immutable; preserve this. Do not introduce a wrapper that exposes `&mut str` or `Box<str>` mutation.

If any of these need to change, the unsafe APIs must be rethought from scratch — most likely by switching to reference-counted slots or by making the unsafe APIs require an explicit guard type that ties the pointer's lifetime to the read lock.

## `StoredStrPtr` lifetime is the caller's problem

`StoredStrPtr` wraps a `*const str` with no lifetime parameter. It implements `Clone`, `Copy`-shaped patterns, `Hash`, `Ord`, `Display`, `Deref<Target = *const str>`, and `From<StoredStrPtr> for &'a str` for *any* `'a`. None of this is checked.

The documented contract is: **the pointer is valid only as long as the originating `UniqueStrStore` is alive**. Concretely:

- Storing a `StoredStrPtr` in a `'static` collection is sound *only* if the originating store is itself in a `'static` location (e.g. behind a `OnceLock` or `lazy_static`).
- Sending a `StoredStrPtr` to a thread that may outlive the store is unsound. There is no `Send`/`Sync` bound preventing this — be careful.
- Cloning the store (`Arc` clone) keeps the pointer valid as long as *any* clone is live, because all clones share the same underlying `Box<str>` allocations.

`StoredStr<'a>` is the safe alternative for almost every use case: it carries a `&'a UniqueStrStore` so the lifetime is checked, at the cost of an extra word per handle.

## `borrow_str`'s panic check

```rust
if idx >= LATIN1_NUM && (idx - LATIN1_NUM) as usize >= self.store.read().len() {
    panic!("Store index {idx} out of bounds (max: {})", self.len() - 1);
}
```

The first condition is `>=`, not `>`. This ensures that `idx == LATIN1_NUM` (256) on an empty user-string store panics rather than passing through to `store.get_unchecked(0)`. Indices in `0..LATIN1_NUM` are always valid (the `ascii` vector is fixed-size and pre-populated), so the check only needs to fire for `idx >= LATIN1_NUM`.

Even with the bounds check, `borrow_str` is `unsafe` because the returned `&str` outlives the read lock — callers must uphold the append-only contract described above. Use `get` for any path where the index is not statically known to be valid.

## Recommended use by call site

- **Library consumers**: prefer `get` (returns `Result`) or `StoredStr` (returned by `insert_or_get`). These compose with normal Rust lifetimes.
- **Hot internal loops** (e.g. inside `reconstruct`): `borrow_str` with locally-verified indices is fine.
- **Cross-structure references** (e.g. embedding an interned string in another long-lived data structure): use `StoredStr<'a>` if the structure can carry the lifetime, or `StoredStrPtr` plus a documented store-lifetime invariant if it cannot.

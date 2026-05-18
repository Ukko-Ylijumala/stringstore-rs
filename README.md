# stringstore

A memory-efficient, thread-safe string interning library for Rust with stable `u32` indices.

> **Status: heavily WIP.** This crate is pre-1.0 (currently `0.3.x`) and *not* published to crates.io. APIs, internal layout, error variants, and tokenizer behavior may all change without notice. The dormant `TextElement` / `StructuredLine` scaffolding in `src/lib.rs` is an explicit work-in-progress and is not part of the supported surface. If you depend on this crate, pin to an exact git rev.

## What it does

`UniqueStrStore` stores each distinct string once and hands out a stable `u32` index for it. Subsequent inserts of the same string return the same index. Both lookup-by-index and lookup-by-content are O(1) average.

Indices are append-only — once assigned, an index never changes and never points to a different string. There is no removal API.

The first 256 indices are reserved for ISO-8859-1 codepoints (with `""` at index 0). Single-character ASCII / Latin-1 strings short-circuit the hash map entirely, so common tokens like spaces and punctuation are essentially free to insert.

## Quick example

```rust
use stringstore::UniqueStrStore;

let store = UniqueStrStore::new();
assert_eq!(store.len(), 256); // ISO-8859-1 codepoints are pre-populated

let hello = store.insert("Hello, world!");
assert_eq!(hello, 256); // first user-inserted string

// inserting again returns the same index
assert_eq!(store.insert("Hello, world!"), hello);

// retrieve by index
assert_eq!(store.get(hello).unwrap(), "Hello, world!");

// internal consistency check (panics in debug, returns Err in release)
store.validate_contents().expect("store is consistent");
```

The crate also provides:

- `split_and_store` / `split_and_store_multi` — split a string on one or more delimiters and intern each part.
- `store_path` — normalize a filesystem path and intern each segment.
- `reconstruct` — rebuild the original string from a slice of indices.
- `StoredStr<'a>` — a safe handle that derefs to `&str`.
- `StoredStrPtr` — a raw-pointer handle for callers who can guarantee the store outlives the pointer.

## Installation

```toml
[dependencies]
stringstore = { git = "https://github.com/Ukko-Ylijumala/stringstore-rs" }
```

The crate pulls in a few git-pinned dependencies under the same author (`custom_xxh3`, `miniutils`, `timesince`, and a fork of `size-of`). They are required for the crate to build.

## Features

- `size_of` *(off by default)* — implements `size_of::SizeOf` for `UniqueStrStore`, allowing memory footprint accounting. Requires the forked `size-of` git dependency.

## Design

See [`doc/design/`](doc/design/README.md) for per-feature design notes — storage layout, concurrency model, unsafe pointer contract, tokenization, and the splitting/path encoding. These are the documents to read before contributing.

## Limitations

- No string removal or modification (this is what makes the index stability and unsafe pointer surface sound — see [`doc/design/unsafe-pointers.md`](doc/design/unsafe-pointers.md)).
- No partial deduplication; substrings of stored strings are not themselves shared.
- Maximum unique strings: `u32::MAX - 255`. `insert` panics on overflow rather than returning a `StoreFull` error (the public signature returns `u32`; a future `try_insert -> Result<u32>` could surface this cleanly).
- The two tokenizers used by `split_and_store_multi` can disagree on inputs with overlapping delimiters. See [`doc/design/tokenization.md`](doc/design/tokenization.md).

## License

Copyright (c) 2024-2025 Mikko Tanner. Licensed under MIT OR Apache-2.0.

## Contributing

Issues and pull requests are welcome. Because the project is WIP, please open an issue to discuss any non-trivial change before sending a patch — internal layout is still in flux.

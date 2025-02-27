# Stringstore - a string interning library for Rust

## Overview
**StringStore** is a Rust library that provides memory efficient and thread-safe string interning and deduplication functionality for unique string slices with stable indexing. This should significantly reduce memory usage in scenarios where many duplicate strings need to be stored in memory.

## Features
- Efficient: each unique string is stored only once.
- Fast lookups: O(1) average complexity for both index and content-based lookups.
- Stable indexing: once a string is stored, its index remains constant.
- Allocations: uses [Box<str>] for heap allocation of strings.
- the empty string ("") always occupies the first index (0).
- ISO-8859-1 codepoints: contained explicitly, at indices 1-255 (minus '\0').

## Design Considerations
- Uses a [Vec<Box<str>>] for string storage, which is efficient for random access, and should help with cache locality as well.
- Uses a [DashMap] with `u64` Xxh3 string hashes as keys for fast lookups.
- Custom [xxhash_rust] hasher ([CustomXxh3Hasher]) for potentially faster hashing.
- Thread-safe.
- trait [SizeOf]: provides a way to measure the size of the structure in memory.
- ISO-8859-1: separate non-locking [Vec] for indices 0-255 to avoid locking and hashing overhead for common characters.
- First inserted string is always at index 256.

## Performance Characteristics
- Insertion: O(1) average
- Lookup by content: O(1) average
- Lookup by index: O(1)
- Memory overhead: small fixed cost per unique string

## Usage
This structure should work nicely for scenarios where you need to store many duplicate strings and require fast lookups by both content and stable indices.

## Safety
While most operations are safe, the `get_unchecked` method provides an unsafe, non-bounds-checking lookup (meant mostly for internal use with known indices).

## Limitations
- Does not support string removal to maintain index stability.
- Does not support string modification after insertion.
- No partial deduplication of strings (e.g. substrings).
- The maximum number of unique strings is limited by the [u32] index.

## Installation

Add this to your `Cargo.toml`:

```toml
[dependencies]
stringstore = { git = "https://github.com/Ukko-Ylijumala/stringstore-rs" }
```

## Usage
```rust
use stringstore::UniqueStrStore;

let hello: &'static str = "Hello, world!";
let store = UniqueStrStore::new();
assert_eq!(store.len(), 256); // incl. ISO-8859-1 codepoints by default

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
```

## License

Copyright (c) 2024-2025 Mikko Tanner. All rights reserved.

License: MIT OR Apache-2.0

## Contributing

Contributions are welcome! Please feel free to submit a Pull Request.

## Version History

- 0.3.5: Initial library version
    - Filter StringStore code to a separate crate

This library started its life as a component of a larger application, but at some point it made more sense to separate the code into its own little project and here we are.

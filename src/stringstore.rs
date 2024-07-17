// Copyright (c) 2024 Mikko Tanner. All rights reserved.

#![allow(dead_code)]

use crate::hashing::CustomXxh3Hasher;
use size_of::SizeOf;
use std::{
    cmp::Ordering,
    collections::HashMap,
    fmt::{self, Debug, Display, Formatter},
    hash::{BuildHasher, Hash, Hasher},
    ops::Deref,
};

/**
A memory-efficient storage for unique string slices with stable indexing.

[UniqueStrStore] implements a string interning system, which stores only one
copy of each distinct string. This should significantly reduce memory usage
in scenarios where many duplicate strings are used.

## Key Features
- Efficient: each unique string is stored only once.
- Fast lookups: O(1) average complexity for both index and content-based lookups.
- Stable indexing: once a string is stored, its index remains constant.
- Allocations: uses [Box<str>] for heap allocation of strings.

## Design Considerations
- Uses a [Vec<Box<str>>] for string storage, which should ensure good cache
  locality and efficient random access.
- Uses a [HashMap] with `*const str` keys for fast lookups.
- Custom [xxhash_rust] hasher ([CustomXxh3Hasher]) for potentially faster hashing.
- No removal operations to guarantee index stability.
- Not thread-safe.

## Performance Characteristics
- Insertion: O(1) average
- Lookup by content: O(1) average
- Lookup by index: O(1)
- Memory overhead: small fixed cost per unique string

## Usage
This structure should work nicely for scenarios where you need to store many
duplicate strings and require fast lookups by both content and stable indices.

## Safety
While most operations are safe, the `get_unchecked` method provides an unsafe,
non-bounds-checking lookup (meant mostly for internal use with known indexes).

## Limitations
- Does not support string removal to maintain index stability.
- Not thread-safe.

## Example
```
use statter::stringstore::UniqueStrStore;

let mut store = UniqueStrStore::new();
assert_eq!(store.len(), 0);

let hello: &'static str = "Hello, world!";
let stored = store.insert(hello);

assert_eq!(stored.idx(), 0);
assert_eq!(stored.as_ref(), hello);
assert_eq!(store.get(0).unwrap(), hello);
assert_ne!(store.get_unchecked(0), "foo");
*/
#[derive(Default, Debug, SizeOf)]
pub struct UniqueStrStore {
    store: Vec<Box<str>>,
    index: HashMap<*const str, u32, CustomXxh3Hasher>,
}

impl UniqueStrStore {
    pub fn new() -> Self {
        let capacity: usize = 64 * 1024; // 65536 entries to start with
        UniqueStrStore {
            store: Vec::with_capacity(capacity),
            index: HashMap::with_hasher(CustomXxh3Hasher::default().build_hasher()),
        }
    }

    /// The number of unique string slices stored.
    #[inline]
    pub fn len(&self) -> usize {
        self.store.len()
    }

    /// Whether we already have this string slice stored.
    #[inline]
    pub fn contains(&self, s: &str) -> bool {
        self.index.contains_key(&(s as *const str))
    }

    /// The reference of a stored string slice, if it exists.
    #[inline]
    pub fn get_ref(&self, s: &str) -> Option<StoredStr> {
        self.index
            .get(&(s as *const str))
            .copied()
            .map(|idx: u32| StoredStr(idx, self))
    }

    /// Internal function to add a new string slice to the store.
    fn add_new(&mut self, s: &str) -> StoredStr {
        let idx: u32 = self.len() as u32;
        let boxed: Box<str> = s.into();
        let ptr: *const str = boxed.as_ref();
        self.index.insert(ptr, idx);
        self.store.push(boxed);
        StoredStr(idx, self)
    }

    /// Insert a new string (slice) and return its [StoredStr] reference.
    ///
    /// If the string (slice) already exists, return its reference instead.
    pub fn insert<T>(&mut self, s: T) -> StoredStr
    where
        T: Into<String>,
    {
        let s: String = s.into();
        if self.contains(&s) {
            self.get_ref(&s).unwrap()
        } else {
            self.add_new(&s)
        }
    }

    /// Get a reference to a stored string slice by its index, if it exists.
    pub fn get<'a>(&'a self, idx: u32) -> Option<&'a str> {
        self.store.get(idx as usize).map(|s: &Box<str>| s.as_ref())
    }

    /**
    Returns a reference to a stored [str] without doing bounds checking.

    For a safe alternative, use `get`.
    ### Safety
    Calling this method with an out-of-bounds index is undefined behavior
    even if the resulting reference is not used.
    */
    #[inline]
    pub fn get_unchecked<'a>(&'a self, idx: u32) -> &'a str {
        unsafe { self.store.get_unchecked(idx as usize).as_ref() }
    }

    /// Get a stored string slice by its reference.
    pub fn from_ref<'a>(&'a self, r: StoredStr) -> &'a str {
        self.get_unchecked(r.into())
    }
}

/// A reference (index) to a stored string slice in a [UniqueStrStore].
#[derive(Clone)]
pub struct StoredStr<'a>(u32, &'a UniqueStrStore);

impl StoredStr<'_> {
    #[inline]
    fn reference(&self) -> &str {
        self.1.get_unchecked(self.0)
    }

    /// Get the index of the stored string slice.
    #[inline]
    pub fn idx(&self) -> u32 {
        self.0
    }

    /// Get the reference to the [UniqueStrStore] that contains this string.
    pub fn store(&self) -> &UniqueStrStore {
        self.1
    }

    pub fn cloned(&self) -> String {
        self.reference().to_string()
    }
}

/* --------------------------------- */

impl AsRef<str> for StoredStr<'_> {
    fn as_ref(&self) -> &str {
        self.reference()
    }
}

impl Deref for StoredStr<'_> {
    type Target = str;

    fn deref(&self) -> &Self::Target {
        self.reference()
    }
}

/* --------------------------------- */

impl Eq for StoredStr<'_> {}

impl PartialEq for StoredStr<'_> {
    fn eq(&self, other: &Self) -> bool {
        self.0 == other.0
    }
}

impl PartialOrd for StoredStr<'_> {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        self.0.partial_cmp(&other.0)
    }
}

impl Ord for StoredStr<'_> {
    fn cmp(&self, other: &Self) -> Ordering {
        self.0.cmp(&other.0)
    }
}

impl Hash for StoredStr<'_> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.0.hash(state)
    }
}

/* --------------------------------- */

impl Debug for StoredStr<'_> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "StoredStr({}: {})", self.idx(), self.reference())
    }
}

impl Display for StoredStr<'_> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.reference())
    }
}

/* --------------------------------- */

// Implement `From` for converting `StoredStr` into `u32`.
impl<'a> From<StoredStr<'a>> for u32 {
    fn from(v: StoredStr) -> u32 {
        v.0
    }
}

impl<'a> From<StoredStr<'a>> for &'a str {
    fn from(v: StoredStr<'a>) -> &'a str {
        v.1.get_unchecked(v.0)
    }
}

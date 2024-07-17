// Copyright (c) 2024 Mikko Tanner. All rights reserved.

#![allow(dead_code)]

use crate::hashing::{hash_bytes, CustomXxh3Hasher};
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

let hello: &'static str = "Hello, world!";
let mut store = UniqueStrStore::new();

let stored = store.insert(hello);
assert_eq!(store.len(), 1);
assert!(store.contains(hello));

let again = store.insert(hello);
let foo = store.insert("foo");
assert_eq!(store.len(), 2);

assert_eq!(stored.idx(), 0);
assert_eq!(again.idx(), 0);
assert_eq!(foo.idx(), 1);

assert_eq!(stored.as_ref(), hello);
assert_eq!(stored, again);
assert_eq!(store.get(0).unwrap(), hello);
assert_eq!(store.get_unchecked(1), "foo");
*/
#[derive(Default, Debug, SizeOf)]
pub struct UniqueStrStore {
    store: Vec<Box<str>>,
    index: HashMap<u64, u32, CustomXxh3Hasher>,
}

impl UniqueStrStore {
    pub fn new() -> Self {
        let capacity: usize = 64 * 1024; // 65536 entries to start with
        UniqueStrStore {
            store: Vec::with_capacity(capacity),
            index: HashMap::with_capacity_and_hasher(
                capacity,
                CustomXxh3Hasher::default().build_hasher(),
            ),
        }
    }

    /// Put this [UniqueStrStore] into a [Box].
    pub fn boxed(self) -> Box<Self> {
        Box::new(self)
    }

    /// The number of unique string slices stored.
    #[inline]
    pub fn len(&self) -> usize {
        self.store.len()
    }

    /// Whether we already have this string slice stored.
    #[inline]
    pub fn contains(&self, s: &str) -> bool {
        self.index.contains_key(&hash_bytes(s.as_bytes()))
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

    #[inline]
    fn as_ptr(&self) -> *const UniqueStrStore {
        self as *const UniqueStrStore
    }

    /// The reference of a stored string slice, if it exists.
    #[inline]
    pub fn get_ref(&self, s: &str) -> Option<StoredStr> {
        self.index
            .get(&hash_bytes(s.as_bytes()))
            .copied()
            .map(|idx: u32| StoredStr(idx, self.as_ptr()))
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
            let idx: u32 = self.len() as u32;
            self.index.insert(hash_bytes(s.as_bytes()), idx);
            self.store.push(s.into());
            StoredStr(idx, self.as_ptr())
        }
    }
}

/// A reference (index) to a stored string slice in a [UniqueStrStore].
#[derive(Clone)]
pub struct StoredStr(u32, *const UniqueStrStore);

impl StoredStr {
    #[inline]
    fn reference(&self) -> &str {
        unsafe { (*self.1).get_unchecked(self.0) }
    }

    /// Get the index of the stored string slice.
    #[inline]
    pub fn idx(&self) -> u32 {
        self.0
    }

    /// Get the reference to the [UniqueStrStore] that contains this string.
    pub fn store(&self) -> &UniqueStrStore {
        unsafe { &*self.1 }
    }

    pub fn cloned(&self) -> String {
        self.reference().to_string()
    }
}

/* --------------------------------- */

impl AsRef<str> for StoredStr {
    fn as_ref(&self) -> &str {
        self.reference()
    }
}

impl Deref for StoredStr {
    type Target = str;

    fn deref(&self) -> &Self::Target {
        self.reference()
    }
}

/* --------------------------------- */

impl Eq for StoredStr {}

impl PartialEq for StoredStr {
    fn eq(&self, other: &Self) -> bool {
        self.0 == other.0
    }
}

impl PartialOrd for StoredStr {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        self.0.partial_cmp(&other.0)
    }
}

impl Ord for StoredStr {
    fn cmp(&self, other: &Self) -> Ordering {
        self.0.cmp(&other.0)
    }
}

impl Hash for StoredStr {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.0.hash(state)
    }
}

/* --------------------------------- */

impl Debug for StoredStr {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "StoredStr({}: {})", self.idx(), self.reference())
    }
}

impl Display for StoredStr {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.reference())
    }
}

/* --------------------------------- */

// Implement `From` for converting `StoredStr` into `u32`.
impl<'a> From<StoredStr> for u32 {
    fn from(v: StoredStr) -> u32 {
        v.0
    }
}

impl<'a> From<StoredStr> for &'a str {
    fn from(v: StoredStr) -> &'a str {
        unsafe { (*v.1).get_unchecked(v.0) }
    }
}

/* ######################################################################### */

mod tests {
    #[allow(unused_imports)]
    use super::*;

    #[test]
    fn test_unique_store_basic() {
        let hello: &'static str = "Hello, world!";
        let mut store = UniqueStrStore::new();

        let stored = store.insert(hello);
        assert_eq!(store.len(), 1, "Store length should be 1");
        assert!(store.contains(hello), "Store does not contain 'Hello, world!': {store:?}");

        let again = store.insert(hello);
        let foo = store.insert("foo");
        assert_eq!(store.len(), 2, "Store length should be 2");

        assert_eq!(stored.idx(), 0, "'Hello, world!' index should be 0: {stored:?}");
        assert_eq!(again.idx(), 0, "Second 'Hello, world!' index should be again 0: {again:?}");
        assert_eq!(foo.idx(), 1, "'foo' index should be 1: {foo:?}");

        assert_eq!(stored.as_ref(), hello, "as_ref() should == 'Hello, world!': {stored:?}");
        assert_eq!(stored, again, "StoredStr instances should be equal: {stored:?} != {again:?}");
        assert_eq!(store.get(0).unwrap(), hello, "get(0) should == 'Hello, world!'");
        assert_eq!(store.get_unchecked(1), "foo", "get_unchecked(1) should == 'foo'");
    }
}

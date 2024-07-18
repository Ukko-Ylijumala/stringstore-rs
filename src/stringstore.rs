// Copyright (c) 2024 Mikko Tanner. All rights reserved.

#![allow(dead_code)]

use crate::hashing::{hash_bytes, CustomXxh3Hasher};
use parking_lot::RwLock;
use size_of::{Context, SizeOf};
use std::{
    cmp::Ordering,
    collections::HashMap,
    fmt::{self, Debug, Display, Formatter},
    hash::{BuildHasher, Hash, Hasher},
    ops::Deref,
    sync::Arc,
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
- Uses a [Vec<Box<str>>] for string storage, which is efficient for random access,
  and should help with cache locality as well.
- Uses a [HashMap] with `u64` Xxh3 string hashes as keys for fast lookups.
- Custom [xxhash_rust] hasher ([CustomXxh3Hasher]) for potentially faster hashing.
- Thread-safe due to wrapping storage/index fields with [RwLock]s.
- trait [SharedStrStore]: is used to lock certain methods behind a shared reference.

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
non-bounds-checking lookup (meant mostly for internal use with known indices).

## Limitations
- Does not support string removal to maintain index stability.
- Does not support string modification after insertion.
- No partial deduplication of strings (e.g. substrings).
- The maximum number of unique strings is limited by the [u32] index.
- Methods which return a [StoredStr] reference can only be used if the
  [UniqueStrStore] is wrapped in an [Arc] (as it must point back to the store).

## Example
```
use statter::stringstore::UniqueStrStore;

let hello: &'static str = "Hello, world!";
let store = UniqueStrStore::new();

store.insert(hello);
assert_eq!(store.len(), 1);
assert!(store.contains(hello));

store.insert(hello);
store.insert("foo");
assert_eq!(store.len(), 2);
assert_eq!(store.get(0).unwrap(), hello);
// unsafe if the index is out of bounds
assert_eq!(unsafe { store.get_unchecked(1) }, "foo");
*/
#[derive(Default, Debug)]
pub struct UniqueStrStore {
    store: RwLock<Vec<Box<str>>>,
    index: RwLock<HashMap<u64, u32, CustomXxh3Hasher>>,
}

impl UniqueStrStore {
    pub fn new() -> Self {
        Self::new_with_capacity(128)
    }

    pub fn new_with_capacity(capacity: usize) -> Self {
        UniqueStrStore {
            store: Vec::with_capacity(capacity).into(),
            index: HashMap::with_capacity_and_hasher(
                capacity,
                CustomXxh3Hasher::default().build_hasher(),
            )
            .into(),
        }
    }

    /// Put this [UniqueStrStore] into an [Arc].
    pub fn shared(self) -> Arc<Self> {
        self.into()
    }

    /// The number of unique string slices stored.
    #[inline]
    pub fn len(&self) -> usize {
        self.store.read().len()
    }

    /// Whether we already have this string slice stored.
    #[inline]
    pub fn contains(&self, s: &str) -> bool {
        self.index.read().contains_key(&hash_bytes(s.as_bytes()))
    }

    /// Get the index of a stored string slice by its content, if it exists.
    pub fn idx(&self, s: &str) -> Option<u32> {
        self.index.read().get(&hash_bytes(s.as_bytes())).copied()
    }

    /// Get a reference to a stored string slice by its index, if it exists.
    pub fn get<'a>(&'a self, idx: u32) -> Option<&'a str> {
        let store = self.store.read();
        if idx as usize >= store.len() {
            None
        } else {
            drop(store); // must release the lock to avoid a deadlock
            unsafe { Some(self.get_unchecked(idx)) }
        }
    }

    /**
    Returns a reference to a stored [str] without doing bounds checking.

    For a safe alternative, use `get`.
    ### Safety
    Calling this method with an out-of-bounds index is undefined behavior
    even if the resulting reference is not used.

    Since we're going through a [RwLock], we need to do some extra pointer
    magic, as returning a [Box::as_ref()] directly would make the borrow
    checker complain about "cannot return value referencing local variable".

    This should work too, but is more complicated:
    ```ignore
    let ptr: *const u8 = b.as_ptr();
    let len: usize = b.len();
    let bytes: &[u8] = core::slice::from_raw_parts(ptr, len);
    std::str::from_utf8_unchecked(bytes)
    */
    #[inline]
    pub unsafe fn get_unchecked<'a>(&'a self, idx: u32) -> &'a str {
        let store = self.store.read();
        let b: &Box<str> = store.get_unchecked(idx as usize);
        let ptr: *const str = b.as_ref() as *const str;
        &*ptr
    }

    /**
    Insert a new string foregoing the first index check before write locking.

    We still must check again after acquiring the write locks, as another
    thread might have gone behind our back in the meantime.
    */
    fn insert_unchecked(&self, s: String) -> u32 {
        let mut store = self.store.write();
        let mut index = self.index.write();
        let key: u64 = hash_bytes(s.as_bytes());

        // Check again to be safe.
        if index.contains_key(&key) {
            return index.get(&key).copied().unwrap();
        }

        let idx: u32 = store.len() as u32;
        index.insert(key, idx);
        store.push(s.into());
        idx
    }

    /// Insert a new string (slice), if it doesn't already exist.
    /// Returns the index in either case.
    pub fn insert<T>(&self, s: T) -> u32
    where
        T: Into<String>,
    {
        let s: String = s.into();
        if let Some(idx) = self.idx(&s) {
            idx
        } else {
            self.insert_unchecked(s)
        }
    }

    /**
    Validate the contents of the store and index.

    ### Release mode
    Returns a list of errors if any are found.

    ### Debug mode
    Panics with the error list if any are found.
    */
    pub fn validate_contents(&self) -> Result<(), Vec<String>> {
        // we want exclusive locks for validation to ensure consistency
        let store = self.store.write();
        let index = self.index.write();
        let mut errs: Vec<String> = Vec::new();

        let l_store: usize = store.len();
        let l_index: usize = index.len();
        if l_store != l_index {
            errs.push(format!("store.len() ({l_store}) != index.len() ({l_index})"));
        };

        // Check that each store entry has a corresponding index.
        for (sid, s) in store.iter().enumerate() {
            let key: u64 = hash_bytes(s.as_bytes());
            if !index.contains_key(&key) {
                errs.push(format!("missing hash: 0x{key:x} (str_id: {sid}, str: '{s}')"));
            };
            let found: u32 = index[&key];
            if found != sid as u32 {
                errs.push(format!(
                    "index mismatch for str_id {sid} ('{s}'): hash 0x{key:x} -> {found} ('{}')",
                    &*store[found as usize]
                ));
            }
        }

        // Check that each index is valid wrt. the store.
        for (key, sid) in index.iter() {
            if (*sid as usize) >= l_store {
                errs.push(format!("index out of bounds: {sid} >= {l_store} (hash: 0x{key:x})"));
            }
            let s: &Box<str> = unsafe { store.get_unchecked(*sid as usize) };
            let csum: u64 = hash_bytes(s.as_bytes());
            if csum != *key {
                errs.push(format!(
                    "hash mismatch for '{s}' (stored: 0x{key:x}, calculated: 0x{csum:x})"
                ));
            }
        }

        #[cfg(debug_assertions)]
        if !errs.is_empty() {
            panic!("UniqueStrStore validation failed:\n{}", errs.join("\n"));
        }

        if !errs.is_empty() {
            return Err(errs);
        }
        Ok(())
    }
}

/**
This trait allows locking certain methods behind a shared reference.

For now, this concerns methods which return a [StoredStr] reference, as that
struct needs a stable pointer to the [UniqueStrStore] to function properly.
*/
pub trait SharedStrStore {
    type Inner: Deref<Target = UniqueStrStore>;

    fn get_ref(&self, s: &str) -> Option<StoredStr>;
    fn insert_or_get<T>(&self, s: T) -> StoredStr
    where
        T: Into<String>;
}

impl SharedStrStore for Arc<UniqueStrStore> {
    type Inner = Self;

    /// The reference of a stored string slice, if it exists.
    #[inline]
    fn get_ref(&self, s: &str) -> Option<StoredStr> {
        self.index
            .read()
            .get(&hash_bytes(s.as_bytes()))
            .copied()
            .map(|idx: u32| StoredStr(idx, self.clone()))
    }

    /// Insert a new string (slice) and return its [StoredStr] reference.
    ///
    /// If the string (slice) already exists, return its reference instead.
    fn insert_or_get<T>(&self, s: T) -> StoredStr
    where
        T: Into<String>,
    {
        let s: String = s.into();
        if !self.contains(&s) {
            self.insert_unchecked(s.clone());
        }
        self.get_ref(&s).unwrap()
    }
}

// We have to implement our own since `size_of::SizeOf` does not support `RwLock`.
impl SizeOf for UniqueStrStore {
    fn size_of_children(&self, context: &mut Context) {
        self.store.read().size_of_children(context);
        self.index.read().size_of_children(context);
    }
}

/* ######################################################################### */

/// A reference (index) to a stored string slice in a [UniqueStrStore].
#[derive(Clone)]
pub struct StoredStr(u32, Arc<UniqueStrStore>);

impl StoredStr {
    #[inline]
    /// This method is safe to call, as our reference is guaranteed to be valid.
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
        &*self.1
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
        self.reference() == other.reference()
    }
}

impl PartialOrd for StoredStr {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        self.reference().partial_cmp(&other.reference())
    }
}

impl Ord for StoredStr {
    fn cmp(&self, other: &Self) -> Ordering {
        self.reference().cmp(&other.reference())
    }
}

impl Hash for StoredStr {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.reference().hash(state)
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

impl PartialEq<StoredStr> for &str {
    fn eq(&self, other: &StoredStr) -> bool {
        *self == other.reference()
    }
}

impl PartialEq<&str> for StoredStr {
    fn eq(&self, other: &&str) -> bool {
        self.reference() == *other
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
        unsafe {
            let ptr: *const str = (*v.1).get_unchecked(v.0) as *const str;
            &*ptr
        }
    }
}

/* ######################################################################### */

mod tests {
    #[allow(unused_imports)]
    use super::*;

    #[test]
    fn test_unique_store_basic() {
        let hello: &'static str = "Hello, world!";
        let store = UniqueStrStore::new_with_capacity(10);
        let i = store.insert(hello);

        assert_eq!(store.len(), 1, "Store length should be 1");
        assert!(store.contains(hello), "Store does not contain '{hello}': {store:?}");
        assert_eq!(store.get(i).unwrap(), hello, "get(0) should == '{hello}'");
        assert_eq!(
            unsafe { store.get_unchecked(i) },
            hello,
            "get_unchecked(0) should == '{hello}'"
        );
    }

    #[test]
    fn test_unique_store_shared() {
        let hello: &'static str = "Hello, world!";
        let foo_s: &'static str = "foo";
        let store = UniqueStrStore::new_with_capacity(10).shared();
        let stored = store.insert_or_get(hello);

        assert_eq!(store.len(), 1, "Store length should be 1");
        assert!(store.contains(hello), "Store does not contain '{hello}': {store:?}");

        let again = store.insert_or_get(hello);
        let foo = store.insert_or_get(foo_s);
        assert_eq!(store.len(), 2, "Store length should be 2");

        assert_eq!(stored.idx(), 0, "'{hello}' index should be 0: {stored:?}");
        assert_eq!(again.idx(), 0, "Second '{hello}!' index should be again 0: {again:?}");
        assert_eq!(foo.idx(), 1, "'{foo_s}' index should be 1: {foo:?}");

        assert_eq!(stored.as_ref(), hello, "as_ref() should == '{hello}': {stored:?}");
        assert_eq!(stored, again, "StoredStr instances should be equal: {stored:?} != {again:?}");
        assert_eq!(store.get(0).unwrap(), hello, "get(0) should == '{hello}'");
        assert_eq!(
            unsafe { store.get_unchecked(1) },
            foo_s,
            "get_unchecked(1) should == '{foo_s}'"
        );
    }

    #[test]
    fn test_concurrent_inserts() {
        use std::thread;

        let s_num = 100_000;
        let t_num = 10;
        let store = UniqueStrStore::new_with_capacity(s_num).shared();
        let threads: Vec<_> = (0..t_num)
            .map(|t| {
                let store = store.clone();
                thread::spawn(move || {
                    (0..s_num / t_num).for_each(|i| {
                        let s = format!("Hello, world! t: {t}, i: {i}");
                        let stored = store.insert_or_get(&s);
                        assert_eq!(stored.as_ref(), s, "Stored string should be '{s}': {stored:?}");
                    })
                })
            })
            .collect();

        for t in threads {
            t.join().unwrap();
        }

        store.validate_contents().ok(); // will panic on failure in debug mode
        assert_eq!(store.len(), s_num, "Stored num should be {s_num}");
    }
}

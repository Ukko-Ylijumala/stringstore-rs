// Copyright (c) 2024-2025 Mikko Tanner. All rights reserved.

#![allow(dead_code)]

use crossbeam::atomic::AtomicCell;
use custom_xxh3::{hash_bytes, CustomXxh3Hasher};
use dashmap::DashMap;
use miniutils::normalize_path;
use parking_lot::RwLock;
use regex::{escape, Regex};
use std::{
    cmp::Ordering,
    error::Error,
    fmt::{self, Debug, Display, Formatter},
    hash::{BuildHasher, Hash, Hasher},
    mem,
    net::IpAddr, //Ipv4Addr, Ipv6Addr},
    ops::Deref,
    path::{Path, PathBuf},
    str::{FromStr, Split},
    sync::Arc,
};
use timesince::SecondsSinceEpoch;
//use uuid::Uuid;

#[cfg(feature = "size_of")]
use size_of::{Context, SizeOf};

const EMPTY_STR: &str = "";
const PATH_SEP: &str = "/";
const LATIN1_NUM: u32 = 256;

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
- the empty string ("") always occupies the first index (0).
- ISO-8859-1 codepoints: contained explicitly, at indices 1-255 (minus '\0').

## Design Considerations
- Uses a [Vec<Box<str>>] for string storage, which is efficient for random access,
  and should help with cache locality as well.
- Uses a [DashMap] with `u64` Xxh3 string hashes as keys for fast lookups.
- Custom [xxhash_rust] hasher ([CustomXxh3Hasher]) for potentially faster hashing.
- Thread-safe.
- trait [SizeOf]: provides a way to measure the size of the structure in memory.
- ISO-8859-1: separate non-locking [Vec] for indices 0-255 to avoid locking
  and hashing overhead for common characters.
- First inserted string is always at index 256.

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

## Example
```
use stringstore::UniqueStrStore;

let hello: &'static str = "Hello, world!";
let store = UniqueStrStore::new();
assert_eq!(store.len(), 256); // incl. ISO-8859-1 codepoints

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
*/
#[derive(Default, Debug, Clone)]
pub struct UniqueStrStore {
    store: Arc<RwLock<Vec<Box<str>>>>,
    index: Arc<DashMap<u64, u32, CustomXxh3Hasher>>,
    ascii: Arc<Vec<Box<str>>>,
    len: Arc<AtomicCell<u32>>,
}

impl UniqueStrStore {
    /// Create a new [UniqueStrStore] with a default capacity of 128.
    pub fn new() -> Self {
        Self::new_with_capacity(128)
    }

    pub fn new_with_capacity(capacity: usize) -> Self {
        // Make the ISO-8859-1 codepoint Vec. Its first element
        // is always the empty string.
        let mut latin1: Vec<Box<str>> = (0..LATIN1_NUM)
            .into_iter()
            .map(|i: u32| {
                // this is safe because we stay in a safe range
                unsafe { char::from_u32_unchecked(i as u32) }
                    .to_string()
                    .into()
            })
            .collect();
        // replace the null string ('\0') with an empty string
        latin1[0] = EMPTY_STR.into();

        UniqueStrStore {
            store: RwLock::new(Vec::with_capacity(capacity)).into(),
            index: DashMap::with_capacity_and_hasher(
                capacity,
                CustomXxh3Hasher::default().build_hasher(),
            )
            .into(),
            ascii: latin1.into(),
            len: AtomicCell::new(LATIN1_NUM).into(),
        }
    }

    /// Put this [UniqueStrStore] into an [Arc].
    pub fn shared(self) -> Arc<Self> {
        self.into()
    }

    /// The number of unique string slices. Includes the ISO-8859-1 codepoints.
    #[inline]
    pub fn len(&self) -> usize {
        self.len.load() as usize
    }

    /// Whether we already have this string slice stored.
    #[inline]
    pub fn contains(&self, s: &str) -> bool {
        if s.len() == 0 {
            return true; // empty string is always contained
        }

        if s.len() == 1 {
            if return_iso8859_1_cp(s).is_some() {
                return true; // ISO-8859-1 implicitly contained
            }
        }

        self.index.contains_key(&hash_bytes(s.as_bytes()))
    }

    /// Get the index of a stored string slice by its content, if it exists.
    pub fn idx(&self, s: &str) -> Option<u32> {
        if s.len() == 0 {
            return Some(0);
        }

        if s.len() == 1 {
            if let Some(c) = return_iso8859_1_cp(s) {
                return Some(c);
            }
        }

        self.index
            .get(&hash_bytes(s.as_bytes()))
            .map(|r| r.value() + LATIN1_NUM)
    }

    /// Get a reference to a stored string slice by its index, if it exists.
    pub fn get<'a>(&'a self, idx: u32) -> StringStoreResult<&'a str> {
        let len: usize = self.len();
        if idx as usize >= len {
            return Err(StringStoreError::oob(idx, len - 1));
        }
        unsafe { Ok(self.borrow_str(idx)) }
    }

    /**
    Get a raw pointer by index to a string slice in the store.
    Does no bounds checking apart from deciding whether to get the pointer
    from the ISO-8859-1 range Vec, or from the store Vec.

    ### Details
    If we're going through a [RwLock], we need to do some extra pointer
    magic, as returning a [Box::as_ref] directly would make the borrow
    checker complain of "cannot return value referencing local variable".

    This is safe because we're returning a pointer to a stored string in the
    heap, which shouldn't move, while the owning [Box] could be moved around.

    This should work too, but is more complicated:
    ```ignore
    let ptr: *const u8 = b.as_ptr();
    let len: usize = b.len();
    let bytes: &[u8] = core::slice::from_raw_parts(ptr, len);
    std::str::from_utf8_unchecked(bytes)
    */
    #[inline]
    unsafe fn get_str_ptr<'a>(&'a self, idx: u32) -> *const str {
        // ISO-8859-1 range
        if idx < LATIN1_NUM {
            return self.ascii[idx as usize].as_ref() as *const str;
        } else {
            let store = self.store.read();
            let b: &Box<str> = store.get_unchecked((idx - LATIN1_NUM) as usize);
            b.as_ref() as *const str
        }
    }

    /**
    Borrow a raw reference to a stored [str]. For a safe alternative, use `get`.

    ### Safety
    Calling this method with an out-of-bounds index will panic.
    */
    #[inline]
    pub unsafe fn borrow_str<'a>(&'a self, idx: u32) -> &'a str {
        if idx > LATIN1_NUM && (idx - LATIN1_NUM) as usize >= self.store.read().len() {
            panic!("Store index {idx} out of bounds (max: {})", self.len());
        } else {
            let ptr: *const str = self.get_str_ptr(idx);
            &*ptr
        }
    }

    /**
    Get a pointer to a stored string slice.

    WARNING: THIS IS AN UNSAFE FN AND SHOULD BE USED WITH CAUTION.
    NO BOUNDS CHECKING IS PERFORMED

    NOTE: The pointer is only valid as long as the store is alive, but this
    is not enforced. The lifetime is the responsibility of the user.
    */
    pub unsafe fn get_ptr(&self, idx: u32) -> StoredStrPtr {
        StoredStrPtr(self.get_str_ptr(idx))
    }

    /**
    Insert a new string foregoing the first index check before write locking.

    We still must check again after acquiring the write locks, as another
    thread might have gone behind our back in the meantime.
    */
    fn insert_unchecked(&self, s: String) -> u32 {
        let mut store = self.store.write();
        let key: u64 = hash_bytes(s.as_bytes());
        // next free index
        let idx: u32 = store.len() as u32;

        // atomic get or insert
        let indexed: u32 = *self.index.entry(key).or_insert(idx);
        if indexed == idx {
            // we did in fact insert a new string
            store.push(s.into());
            self.len.fetch_add(1);
        }
        indexed + LATIN1_NUM
    }

    /// Insert a new string (slice), if it doesn't already exist.
    /// Returns the index in either case.
    pub fn insert<T>(&self, s: T) -> u32
    where
        T: Into<String>,
    {
        let s: String = s.into();
        if s.is_empty() {
            return 0;
        }

        if s.len() == 1 {
            if let Some(c) = return_iso8859_1_cp(&s) {
                return c; // ISO-8859-1 code point
            }
        }

        // For non-ASCII or multi-character strings
        if let Some(idx) = self.idx(&s) {
            idx
        } else {
            self.insert_unchecked(s)
        }
    }

    /// The reference of a stored string slice, if it exists.
    #[inline]
    fn get_ref(&'_ self, s: &str) -> Option<StoredStr<'_>> {
        self.idx(s).map(|idx: u32| StoredStr(idx, self))
    }

    /// Insert a new string (slice) and return its [StoredStr] reference.
    ///
    /// If the string (slice) already exists, return its reference instead.
    fn insert_or_get<T>(&'_ self, s: T) -> StoredStr<'_>
    where
        T: Into<String>,
    {
        let s: String = s.into();
        if s.is_empty() {
            return StoredStr(0, self);
        }
        if !self.contains(&s) {
            self.insert_unchecked(s.clone());
        }
        self.get_ref(&s).unwrap()
    }

    /// Store the parts and return their indices.
    fn store_parts(&self, s: &str, delim: &str) -> Vec<u32> {
        let mut result: Vec<u32> = Vec::new();
        let mut parts: Split<&str> = s.split(delim);
        while let Some(part) = parts.next() {
            if part.is_empty() {
                // empty string here means one of the following:
                // - 2 contiguous delimiters
                // - delimiter at the start or end of the string
                result.push(0);
            } else {
                result.push(self.insert(part));
            }
        }
        result
    }

    /**
    Splits a string by a delimiter, stores each part and the delimiter,
    and returns a [Vec] of part indices in the same order, plus the delimiter
    index separately.

    The index 0 (empty string) in the returned Vec means:
    - at start/end: delimiter found at start/end of the string
    - elsewhere: 2 contiguous delimiters (or more with subsequent zero indices)
    */
    pub fn split_and_store(&self, s: &str, delim: &str) -> (Vec<u32>, u32) {
        if s.is_empty() {
            // special case: empty string
            return (vec![0], self.insert(delim));
        }

        // Store the delimiter first
        let delim_idx: u32 = match delim.is_empty() {
            true => 0,
            false => self.insert(delim),
        };

        (self.store_parts(s, delim), delim_idx)
    }

    /**
    Splits a given string into multiple parts based on multiple delimiters
    and stores each part, returning their indices in the storage, along with
    the storage indices of the provided delimiters.

    First, the delimiters provided in `delims` are stored, then the string
    `s` is split based on these delimiters. Each unique part obtained from
    splitting is stored and its index returned. By default, a simple tokenizer
    is used for shorter strings, while a regex-based tokenizer handles longer
    strings and/or larger sets of delimiters. This can be overridden by the
    `force_regex` optional boolean.

    ## Arguments
    * `s` - a string slice to be atomized
    * `delims` - string slices, based on which `s` shall be split
    * `force_regex` - whether to use the regex-based tokenizer
      - `None` - auto-detect based on string length and number of delimiters
      - `Some(true)` - force regex-based tokenizer
      - `Some(false)` - force simple tokenizer

    ## Returns
    A tuple of two [Vec]s:
    - first one contains the indices of the parts of `s` (including delims!)
    - second one contains the indices of the delimiters themselves

    ## Special Cases
    - If `s` is empty, index `0` is returned, along with the delimiter indices.
    - If `delims` is empty, the function returns a Vec of the index of `s`
      itself (assuming `s` is not empty), and an empty Vec for delimiters.

    NOTE: the two tokenizers are based on different logic and might yield
    differing results for the same input, especially if there is any overlap
    between the provided delimiters. YMMV, buyer beware etc. (WIP)
    */
    pub fn split_and_store_multi(
        &self,
        s: &str,
        delims: &[&str],
        force_regex: Option<bool>,
    ) -> (Vec<u32>, Vec<u32>) {
        let complexity: usize = s.len() * delims.len(); // rough estimate
        let mut result: Vec<u32> = vec![];
        let mut delim_indices: Vec<u32> = vec![];
        // TODO: evaluate thresholds for switching between tokenizers
        let regex: bool = force_regex.unwrap_or_else(|| complexity > 10000 || delims.len() > 10);

        if !delims.is_empty() {
            // store the delimiters first
            for delim in delims {
                if delim.is_empty() {
                    delim_indices.push(0);
                } else {
                    delim_indices.push(self.insert(*delim));
                }
            }
        }

        if s.is_empty() {
            // special case: empty string
            return (vec![0], delim_indices);
        } else if delims.is_empty() {
            // special case: no delimiters
            return (vec![self.insert(s)], vec![]);
        }

        if !regex {
            // use the simple tokenizer for shorter strings
            for token in tokenize(s, delims).iter() {
                result.push(self.insert(&token.content));
            }
        } else {
            for token in tokenize_regex(s, delims).iter() {
                result.push(self.insert(&token.content));
            }
        }

        (result, delim_indices)
    }

    /**
    Insert a new string (which can be coerced into a [Path]) and return
    a [Vec] of parts' indices. The delimiter is assumed to be a '/'.

    NOTE: the path will be normalized before storing, hence the result may
    not be the same as the input if it contains relative paths, escape
    sequences or control characters.

    NOTE: non-unicode sequences will be replaced with the replacement
    character [`U+FFFD REPLACEMENT CHARACTER`][U+FFFD].

    NOTE: if `index[0] == 0` && `index.len() > 1`, it means that the path is
    absolute and starts with "delimiter", in this case the forward slash.
    Especially, for the root path ('/'), the resultant Vec is `[0, 0]`.

    [U+FFFD]: core::char::REPLACEMENT_CHARACTER
    */
    pub fn store_path<P>(&self, s: P) -> Vec<u32>
    where
        P: AsRef<Path>,
    {
        let s: PathBuf = normalize_path(s, false);
        if s.as_os_str().is_empty() {
            return [0].into();
        }
        // we can unwrap safely, as the path is guaranteed to be valid
        self.store_parts(s.to_str().unwrap(), PATH_SEP)
    }

    /**
    Reconstruct a string from stored parts' and delimiter indices.
    Returns an error if any index is out of bounds.

    The same effect can be achieved by something like:
    ```ignore
    let built: String = indices
        .iter()
        .map(|&idx| store.get(idx).unwrap())
        .collect::<Vec<&str>>()
        .join(store.get(delim).unwrap());
    */
    pub fn reconstruct(&self, indices: &[u32], delim: u32) -> StringStoreResult<String> {
        let parts_num: usize = indices.len();
        // special case: empty string
        if parts_num == 0 || (parts_num == 1 && indices[0] == 0) {
            return Ok(EMPTY_STR.to_string());
        } else if parts_num > u32::MAX as usize {
            return Err(StringStoreError::ReconstructionTooLarge {
                requested: parts_num,
                max: u32::MAX as usize,
            });
        }

        // delimiter check
        // we lock the store at this point so that the length is stable
        let store = self.store.read();
        let stored_num: u32 = self.len() as u32;
        if delim >= stored_num {
            return Err(StringStoreError::IndexOutOfBounds {
                idx: delim,
                max: (stored_num - 1) as usize,
            });
        }

        // get the delimiter string
        let delim_str: &Box<str> = match delim < LATIN1_NUM {
            true => &self.ascii[delim as usize],
            false => &store[(delim - LATIN1_NUM) as usize],
        };

        // construct the string
        let mut result: String = String::new();
        for (i, idx) in indices.iter().enumerate() {
            if idx >= &stored_num {
                return Err(StringStoreError::reconstruction(*idx, i, stored_num));
            }

            if idx != &0 {
                result.push_str(match idx < &LATIN1_NUM {
                    true => &self.ascii[*idx as usize],
                    false => &store[(*idx - LATIN1_NUM) as usize],
                });
            }

            if i < parts_num - 1 {
                // no delimiter after the last part
                result.push_str(delim_str);
            }
        }

        Ok(result)
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
        let len: usize = self.len();
        let mut errs: Vec<String> = Vec::new();

        let l_store: usize = store.len();
        let l_index: usize = self.index.len();
        if l_store + LATIN1_NUM as usize != len {
            errs.push(format!("store.len() ({l_store}) != stored length ({len})"));
        };
        if l_store != l_index {
            errs.push(format!("store.len() ({l_store}) != index.len() ({l_index})"));
        };

        // Check that each store entry has a corresponding index.
        for (sid, s) in store.iter().enumerate() {
            let key: u64 = hash_bytes(s.as_bytes());
            if !self.index.contains_key(&key) {
                errs.push(format!("missing hash: 0x{key:x} (str_id: {sid}, str: '{s}')"));
            } else {
                let found: u32 = *self.index.get(&key).unwrap();
                if found != sid as u32 {
                    errs.push(format!(
                        "index mismatch for str_id {sid} ('{s}'): hash 0x{key:x} -> {found} ('{}')",
                        &*store[found as usize]
                    ));
                }
            }
        }

        // Check that each index is valid wrt. the store.
        for itm in self.index.iter() {
            let (key, sid) = itm.pair();
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

// We have to implement our own since `size_of::SizeOf` does not support
// `RwLock` nor `DashMap`.
#[cfg(feature = "size_of")]
impl SizeOf for UniqueStrStore {
    fn size_of_children(&self, context: &mut Context) {
        self.store.read().size_of_children(context);
        self.ascii.size_of_children(context);

        if self.index.capacity() > 0 {
            // key + value + RwLock
            let used: usize = (8 + 4 + 8) * self.index.len();
            let total: usize = (8 + 4 + 8) * self.index.capacity();
            context
                .add(used)
                .add_excess(total - used)
                .add_distinct_allocation();

            self.index.iter().for_each(|itm| {
                itm.key().size_of_children(context);
                itm.value().size_of_children(context);
            });
        };

        self.index.hasher().size_of_children(context);
    }
}

/* ######################################################################### */

/**
Pointer to a string slice living in a [UniqueStrStore]. This is the return
type of [StoredStr::as_ptr].

NOTE: this pointer is only valid as long as the store is alive. The lifetime
is not enforced, as the store is expected to outlive any references to its
contents. This is the responsibility of the user of this struct to enforce.
*/
#[derive(Clone, PartialEq, Eq)]
pub struct StoredStrPtr(*const str);

impl StoredStrPtr {
    #[inline]
    pub fn as_str(&self) -> &str {
        unsafe { &*self.0 }
    }

    pub fn to_string(&self) -> String {
        unsafe { &*self.0 }.to_string()
    }
}

/* --------------------------------- */

impl AsRef<str> for StoredStrPtr {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl Deref for StoredStrPtr {
    type Target = *const str;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

/* --------------------------------- */

impl PartialOrd for StoredStrPtr {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        self.as_str().partial_cmp(&other.as_str())
    }
}

impl Ord for StoredStrPtr {
    fn cmp(&self, other: &Self) -> Ordering {
        self.as_str().cmp(&other.as_str())
    }
}

impl Hash for StoredStrPtr {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.as_str().hash(state)
    }
}

/* --------------------------------- */

impl Debug for StoredStrPtr {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "StoredStrPtr({:?} -> {:?})", self.0, self.as_str())
    }
}

impl Display for StoredStrPtr {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

/* --------------------------------- */

impl PartialEq<StoredStrPtr> for &str {
    fn eq(&self, other: &StoredStrPtr) -> bool {
        *self == other.as_str()
    }
}

impl PartialEq<&str> for StoredStrPtr {
    fn eq(&self, other: &&str) -> bool {
        self.as_str() == *other
    }
}

/* --------------------------------- */

impl<'a> From<StoredStrPtr> for &'a str {
    fn from(v: StoredStrPtr) -> &'a str {
        unsafe { &*v.0 }
    }
}

/* ######################################################################### */

/**
A reference (index) to a stored string slice in a [UniqueStrStore].

This is a self-contained version which has a reference back to the store,
which allows it to be used in place of a "normal" string slice.
*/
#[derive(Clone)]
pub struct StoredStr<'a>(u32, &'a UniqueStrStore);

impl<'a> StoredStr<'a> {
    /// This method is safe to call, as our reference is guaranteed to be valid.
    #[inline]
    fn reference(&self) -> &str {
        unsafe { (*self.1).borrow_str(self.0) }
    }

    pub fn as_ptr(&self) -> StoredStrPtr {
        StoredStrPtr(unsafe { (*self.1).get_str_ptr(self.0) })
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

impl<'a> AsRef<str> for StoredStr<'a> {
    fn as_ref(&self) -> &str {
        self.reference()
    }
}

impl<'a> Deref for StoredStr<'a> {
    type Target = str;

    fn deref(&self) -> &Self::Target {
        self.reference()
    }
}

/* --------------------------------- */

impl<'a> Eq for StoredStr<'a> {}

impl<'a> PartialEq for StoredStr<'a> {
    fn eq(&self, other: &Self) -> bool {
        self.reference() == other.reference()
    }
}

impl<'a> PartialOrd for StoredStr<'a> {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        self.reference().partial_cmp(&other.reference())
    }
}

impl<'a> Ord for StoredStr<'a> {
    fn cmp(&self, other: &Self) -> Ordering {
        self.reference().cmp(&other.reference())
    }
}

impl<'a> Hash for StoredStr<'a> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.reference().hash(state)
    }
}

/* --------------------------------- */

impl<'a> Debug for StoredStr<'a> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "StoredStr({}: {:?})", self.0, self.reference())
    }
}

impl<'a> Display for StoredStr<'a> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.reference())
    }
}

/* --------------------------------- */

impl<'a> PartialEq<StoredStr<'a>> for &str {
    fn eq(&self, other: &StoredStr) -> bool {
        *self == other.reference()
    }
}

impl<'a> PartialEq<&str> for StoredStr<'a> {
    fn eq(&self, other: &&str) -> bool {
        self.reference() == *other
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
    fn from(v: StoredStr) -> &'a str {
        unsafe {
            let ptr: *const str = (*v.1).borrow_str(v.0) as *const str;
            &*ptr
        }
    }
}

/* ######################################################################### */

/**
A reference (index) to a stored string slice in a [UniqueStrStore].

This is a compact version which lacks a reference back to the containing store,
hence it is only usable as a part of a larger structure with a reference.
*/
#[derive(Debug, Clone, PartialEq, Eq)]
struct CompactStr(u32);

impl CompactStr {
    #[inline]
    fn idx(&self) -> u32 {
        self.0
    }

    fn get<'a>(&self, store: &'a UniqueStrStore) -> &'a str {
        unsafe { store.borrow_str(self.0) }
    }

    fn to_string(&self, store: &UniqueStrStore) -> String {
        self.get(store).to_string()
    }
}

/// This is a single or repeated character stored in a [UniqueStrStore].
#[derive(Debug, Clone, PartialEq, Eq)]
struct Character(CompactStr, u8);

impl Character {
    #[inline]
    fn idx(&self) -> u32 {
        self.0 .0
    }

    #[inline]
    fn num(&self) -> u8 {
        self.1
    }

    fn get<'a>(&self, store: &'a UniqueStrStore) -> &'a str {
        unsafe { store.borrow_str(self.idx()) }
    }

    fn to_string(&self, store: &UniqueStrStore) -> String {
        self.get(store).repeat(self.num() as usize)
    }
}

/* --------------------------------- */

/// Possible text elements in a structured line.
#[derive(Debug, PartialEq)]
enum TextElement<I: Integer = i64> {
    /// An element which is explicitly a delimiter, f.ex. space (`" "`).
    Delimiter(Character),
    /// A single (or repeated) character, f.ex. `*` or `***`.
    Char(Character),
    Word(CompactStr),
    /// A key-value pair, f.ex. `foo="bar"`.
    KeyVal(CompactStr, CompactStr),
    /// Integer type. Define like this (i64 is default):
    /// ```ignore
    /// let elem = TextElement::<i32>::Integer(42);
    Integer(I),
    Float(f64),
    /// A range of integers with "range" marker, f.ex. `-15..10` or `0-100`.
    Range(I, I, Character),
    /// A date in the format `YYYY-MM-DD`.
    Day(i16, u8, u8),
    /// A time in the format `HH:MM:SS`.
    Time(u8, u8, u8),
    /// A timestamp as seconds since the Unix epoch.
    Timestamp(SecondsSinceEpoch),
    /// An IPv4 or IPv6 address.
    IPAddress(IpAddr),
    /// A host name, f.ex. `www.example.org`. Usually a FQDN.
    Hostname(CompactStr),
    /// An username, f.ex. `john@workstation`.
    Username(CompactStr, CompactStr),
    /// An email address, f.ex. `john.doe@example.org`.
    Email(CompactStr, CompactStr),
    /// A hexadecimal number, f.ex. `0xdeadbeef` or `feedf00d`.
    HexStr(Hex, HexFormat),
    /// A sentence as a single element, f.ex. `Mary had a little lamb.`.
    Sentence(Vec<CompactStr>),
    /// A URL, f.ex. `https://www.example.org:8080/path/to/file.html`.
    URLStr(Vec<CompactStr>),
    /// URL query params, f.ex. `?foo=bar&baz=qux`. Question mark is implicit.
    URLParams(Vec<CompactStr>),
    /// Enclosed [TextElement] with a start and end delimiter.
    EnclosedElem(Box<TextElement>, CompactStr, CompactStr),
    /// Unprocessed text.
    RawText(String),
    // Maybe for future...?
    //UuidStr(Uuid),
    //PhoneNumber,
    //GeoCoordinate,
    //Duration,
}

/* --------------------------------- */

/// A unit of structured text, which can be a line or a block.
/// Contains a reference to the [UniqueStrStore] for string retrieval.
#[derive(Debug)]
struct StructuredLine {
    elems: Vec<TextElement>,
    store: Arc<UniqueStrStore>,
}

impl StructuredLine {
    fn new(store: &Arc<UniqueStrStore>) -> Self {
        Self {
            elems: Vec::new(),
            store: store.clone(),
        }
    }

    fn len(&self) -> usize {
        self.elems.len()
    }

    fn push(&mut self, elem: TextElement) {
        self.elems.push(elem);
    }
}

impl PartialEq for StructuredLine {
    fn eq(&self, other: &Self) -> bool {
        self.elems == other.elems
    }
}

/* --------------------------------- */

/// A representation of a hexadecimal number.
#[derive(Clone, Copy, PartialEq)]
struct Hex(u64);

impl Hex {
    fn get(&self) -> u64 {
        self.0
    }

    fn to_string(&self, fmt: HexFormat) -> String {
        let mut result: String = format!("{:x}", self.0);
        if fmt.is_prefix() {
            result = format!("0x{}", result);
        }
        if fmt.is_upper() {
            result = result.to_uppercase();
        }
        if fmt.is_columns() {
            result = result
                .chars()
                .enumerate()
                .map(|(i, c)| {
                    if i % 4 == 0 && i != 0 {
                        format!(":{}{}", c, i)
                    } else {
                        c.to_string()
                    }
                })
                .collect();
        }
        result
    }
}

impl Debug for Hex {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "Hex({:x})", self.0)
    }
}

impl Display for Hex {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "{:x}", self.0)
    }
}

/* --------------------------------- */

/// Bitmap of options for hex string display.
#[derive(Clone, Copy, PartialEq)]
pub struct HexFormat(u8);

impl HexFormat {
    pub const PLAIN: Self = Self(0b0);
    pub const UPPER: Self = Self(0b1);
    pub const PREFIX: Self = Self(0b10);
    pub const COLUMNS: Self = Self(0b100);

    /// Whether the hex string is uppercase.
    pub fn is_upper(&self) -> bool {
        self.0 & Self::UPPER.0 != 0
    }
    /// Whether the "0x" prefix should be shown.
    pub fn is_prefix(&self) -> bool {
        self.0 & Self::PREFIX.0 != 0
    }
    /// Whether the parts are divided by columns (":").
    pub fn is_columns(&self) -> bool {
        self.0 & Self::COLUMNS.0 != 0
    }

    #[rustfmt::skip]
    pub fn to_string(&self) -> String {
        if *self == Self::PLAIN {
            return "Plain".to_string();
        }

        let mut parts: Vec<&str> = Vec::new();
        if self.is_upper() { parts.push("Upper"); }
        if self.is_prefix() { parts.push("Prefix"); }
        if self.is_columns() { parts.push("Columns"); }
        parts.join("|")
    }
}

impl Debug for HexFormat {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "HexFmt({}: {})", self.0, self.to_string())
    }
}

/* --------------------------------- */

trait Integer: FromStr + Display + Debug + Copy + PartialOrd + Send + Sync + 'static {
    fn as_i64(&self) -> i64;
    fn as_u64(&self) -> u64;
}

macro_rules! impl_integer {
    ($($t:ty),*) => {
        $(
            impl Integer for $t {
                fn as_i64(&self) -> i64 {
                    *self as i64
                }
                fn as_u64(&self) -> u64 {
                    *self as u64
                }
            }
        )*
    }
}

impl_integer!(i8, i16, i32, i64, i128, isize, u8, u16, u32, u64, u128, usize);

/* ############################# TOKENIZATION ############################## */

#[derive(Default, Debug, PartialEq, Eq)]
/// A token (part) of a delimited string, which has been processed (tokenized).
/// It can be a delimiter, or a regular part.
pub struct Token {
    content: String,
    is_delim: bool,           // default: false
    delim_idx: Option<usize>, // default: None
}

/**
Tokenize a string by a set of delimiters and return a [Vec] of [Token]s.
The delimiters are included as separate tokens.

This version uses a simple string search for delimiters.
*/
pub fn tokenize(s: &str, delims: &[&str]) -> Vec<Token> {
    let mut tokens: Vec<Token> = Vec::new();
    let mut current_token: String = String::new();
    let mut i: usize = 0;

    while i < s.len() {
        if let Some((d_idx, delimiter)) = delims
            .iter()
            .enumerate()
            .find(|(_, &d)| s[i..].starts_with(d))
        {
            if !current_token.is_empty() {
                tokens.push(Token {
                    content: mem::take(&mut current_token),
                    ..Default::default()
                });
            }

            tokens.push(Token {
                content: delimiter.to_string(),
                is_delim: true,
                delim_idx: Some(d_idx),
            });

            i += delimiter.len();
        } else {
            current_token.push(s[i..].chars().next().unwrap());
            i += 1;
        }
    }

    if !current_token.is_empty() {
        tokens.push(Token {
            content: current_token,
            ..Default::default()
        });
    }

    tokens
}

/**
Tokenize a string by a set of delimiters and return a [Vec] of [Token]s.
The delimiters are included as separate tokens.

In contrast to `tokenize()`, this version compiles a regex to find the
delimiters, which should be faster for larger strings and more delimiters.
*/
pub fn tokenize_regex(s: &str, delims: &[&str]) -> Vec<Token> {
    let pattern: String = delims
        .iter()
        .map(|p: &&str| escape(*p))
        .collect::<Vec<_>>()
        .join("|");
    let re: Regex = Regex::new(&pattern).unwrap();
    let mut tokens: Vec<Token> = Vec::new();
    let mut last_end: usize = 0;

    for found in re.find_iter(s) {
        let start: usize = found.start();
        let end: usize = found.end();

        // Add non-delimiter token if there's text before this delimiter
        if start > last_end {
            tokens.push(Token {
                content: s[last_end..start].to_string(),
                ..Default::default()
            });
        }

        // Add delimiter token
        let delimiter = found.as_str();
        tokens.push(Token {
            content: delimiter.to_string(),
            is_delim: true,
            delim_idx: Some(delims.iter().position(|&d| d == delimiter).unwrap()),
        });

        last_end = end;
    }

    // Add final non-delimiter token if there's remaining text
    if last_end < s.len() {
        tokens.push(Token {
            content: s[last_end..].to_string(),
            ..Default::default()
        });
    }

    tokens
}

/* ########################## UTILITY FUNCTIONS ############################ */

/// Check whether the first character of a string is an ISO-8859-1 codepoint,
/// and if so, return it. Otherwise, return None. Zero-length string will panic.
#[inline]
fn return_iso8859_1_cp(s: &str) -> Option<u32> {
    let c: u32 = s.chars().next().unwrap() as u32;
    if c < LATIN1_NUM {
        return Some(c);
    }
    None
}

/* ############################# ERROR HANDLING ############################ */

/// Error type for string store operations.
#[derive(Debug, Clone, PartialEq)]
pub enum StringStoreError {
    /// Index out of bounds error. Contains the invalid index and max index.
    IndexOutOfBounds { idx: u32, max: usize },
    /// Error when the store has reached its maximum capacity (u32::MAX).
    StoreFull,
    /// Error when attempting to reconstruct a string with invalid parts.
    /// Contains details about which part caused the error.
    InvalidReconstruction { idx: u32, pos: usize, max: u32 },
    /// Error when string reconstruction would exceed the maximum allowed size.
    ReconstructionTooLarge { requested: usize, max: usize },
    /// Error when a string contains invalid UTF-8 sequences.
    InvalidUtf8(String),
    /// Error when path contains invalid characters or sequences.
    InvalidPath(String),
    /// Error when delimiter is invalid (e.g., empty when not allowed).
    InvalidDelimiter(String),
    /// Internal error, used when invariants are violated. Should never happen normally.
    InternalError(String),
}

impl Error for StringStoreError {}

impl Display for StringStoreError {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::IndexOutOfBounds { idx, max } => {
                write!(f, "index {idx} out of bounds (max: {max})")
            }
            Self::StoreFull => write!(f, "string store has reached maximum capacity"),
            Self::InvalidReconstruction { idx, pos, max } => {
                write!(f, "invalid index {idx} at position {pos} (max: {max})")
            }
            Self::ReconstructionTooLarge { requested, max } => {
                write!(f, "too many indexes to reconstruct (requested: {requested}, max: {max})")
            }
            Self::InvalidUtf8(info) => write!(f, "invalid UTF-8 sequence: {info}"),
            Self::InvalidPath(info) => write!(f, "invalid path: {info}"),
            Self::InvalidDelimiter(info) => write!(f, "invalid delimiter: {info}"),
            Self::InternalError(info) => write!(f, "internal error: {info}"),
        }
    }
}

/// Type alias for Result with StringStoreError.
pub type StringStoreResult<T> = Result<T, StringStoreError>;

// Helper methods for creating errors
impl StringStoreError {
    /// Create a new IndexOutOfBounds error.
    pub fn oob(idx: u32, max: usize) -> Self {
        Self::IndexOutOfBounds { idx, max }
    }

    /// Create a new InvalidReconstruction error.
    pub fn reconstruction(idx: u32, pos: usize, max: u32) -> Self {
        Self::InvalidReconstruction { idx, pos, max }
    }

    /// Create a new InvalidPath error with details.
    pub fn path<S: Into<String>>(info: S) -> Self {
        Self::InvalidPath(info.into())
    }

    /// Create a new InvalidDelimiter error with details.
    pub fn delimiter<S: Into<String>>(info: S) -> Self {
        Self::InvalidDelimiter(info.into())
    }

    /// Create a new InternalError with details.
    pub fn internal_error<S: Into<String>>(info: S) -> Self {
        Self::InternalError(info.into())
    }
}

/* ################################ TESTS ################################## */

mod tests {
    #[allow(unused_imports)]
    use super::*;

    const HELLO: &str = "Hello, world!";
    const CONC_S_NUM: usize = 100_000;
    const CONC_T_NUM: usize = 10;
    const TOKEN_TEST: &str = "apple,banana,cherry,cake,,cake";
    const TOKEN_DELIMS: [&str; 8] = [",", "cherry", " ", ",", ".", "!", "?", "\n"];
    const TOKENS_LEN: usize = 10;
    const TOKENS_EXPECTED: [&str; TOKENS_LEN] = [
        "apple", ",", "banana", ",", "cherry", ",", "cake", ",", ",", "cake",
    ];

    #[rustfmt::skip]
    #[test]
    fn test_tokenize() {
        let tokens: Vec<Token> = tokenize(TOKEN_TEST, &TOKEN_DELIMS);

        // Check that the length and tokenization is correct
        assert_eq!(tokens.len(), TOKENS_LEN, "len failed, tokens:\n{tokens:#?}");
        for (i, &ref token) in tokens.iter().enumerate() {
            let exp: &str = TOKENS_EXPECTED[i];
            assert_eq!(token.content, exp, "token {i}: {token:?} (tokens: {tokens:#?})");
        }
    }

    #[rustfmt::skip]
    #[test]
    fn test_tokenize_regex() {
        let tokens: Vec<Token> = tokenize_regex(TOKEN_TEST, &TOKEN_DELIMS);

        assert_eq!(tokens.len(), TOKENS_LEN, "len failed, tokens:\n{tokens:#?}");
        for (i, &ref token) in tokens.iter().enumerate() {
            let exp: &str = TOKENS_EXPECTED[i];
            assert_eq!(token.content, exp, "token {i}: {token:?}, tokens:\n{tokens:#?})");
        }

        // detailed check for the tokenization
        let t: [Token; 10] = [
            Token { content: "apple".to_string(),  is_delim: false, delim_idx: None },
            Token { content: ",".to_string(),      is_delim: true,  delim_idx: Some(0) },
            Token { content: "banana".to_string(), is_delim: false, delim_idx: None },
            Token { content: ",".to_string(),      is_delim: true,  delim_idx: Some(0) },
            Token { content: "cherry".to_string(), is_delim: true,  delim_idx: Some(1) },
            Token { content: ",".to_string(),      is_delim: true,  delim_idx: Some(0) },
            Token { content: "cake".to_string(),   is_delim: false, delim_idx: None },
            Token { content: ",".to_string(),      is_delim: true,  delim_idx: Some(0) },
            Token { content: ",".to_string(),      is_delim: true,  delim_idx: Some(0) },
            Token { content: "cake".to_string(),   is_delim: false, delim_idx: None }
            ];

        assert!(tokens == t, "tokens don't match expected:\n{tokens:#?}");
    }

    #[rustfmt::skip]
    #[test]
    fn test_tokenize_long() {
        let test: &str = "Mary had a little lamb, its fleece was white as snow.\n";
        let tokens: Vec<Token> = tokenize(test, &TOKEN_DELIMS);

        assert_eq!(tokens.len(), 24, "len failed, tokens:\n{tokens:#?}");

        let t: [Token; 24] = [
            Token { content: "Mary".to_string(),   is_delim: false, delim_idx: None },
            Token { content: " ".to_string(),      is_delim: true,  delim_idx: Some(2) },
            Token { content: "had".to_string(),    is_delim: false, delim_idx: None },
            Token { content: " ".to_string(),      is_delim: true,  delim_idx: Some(2) },
            Token { content: "a".to_string(),      is_delim: false, delim_idx: None },
            Token { content: " ".to_string(),      is_delim: true,  delim_idx: Some(2) },
            Token { content: "little".to_string(), is_delim: false, delim_idx: None },
            Token { content: " ".to_string(),      is_delim: true,  delim_idx: Some(2) },
            Token { content: "lamb".to_string(),   is_delim: false, delim_idx: None },
            Token { content: ",".to_string(),      is_delim: true,  delim_idx: Some(0) },
            Token { content: " ".to_string(),      is_delim: true,  delim_idx: Some(2) },
            Token { content: "its".to_string(),    is_delim: false, delim_idx: None },
            Token { content: " ".to_string(),      is_delim: true,  delim_idx: Some(2) },
            Token { content: "fleece".to_string(), is_delim: false, delim_idx: None },
            Token { content: " ".to_string(),      is_delim: true,  delim_idx: Some(2) },
            Token { content: "was".to_string(),    is_delim: false, delim_idx: None },
            Token { content: " ".to_string(),      is_delim: true,  delim_idx: Some(2) },
            Token { content: "white".to_string(),  is_delim: false, delim_idx: None },
            Token { content: " ".to_string(),      is_delim: true,  delim_idx: Some(2) },
            Token { content: "as".to_string(),     is_delim: false, delim_idx: None },
            Token { content: " ".to_string(),      is_delim: true,  delim_idx: Some(2) },
            Token { content: "snow".to_string(),   is_delim: false, delim_idx: None },
            Token { content: ".".to_string(),      is_delim: true,  delim_idx: Some(4) },
            Token { content: "\n".to_string(),     is_delim: true,  delim_idx: Some(7) },
            ];

            assert!(tokens == t, "tokens don't match expected:\n{tokens:#?}");

            // regex part
            let tokens: Vec<Token> = tokenize_regex(test, &TOKEN_DELIMS);
            assert_eq!(tokens.len(), 24, "regex len failed, tokens:\n{tokens:#?}");
            assert!(tokens == t, "regex tokens don't match expected:\n{tokens:#?}");

    }

    #[test]
    fn test_unique_store_basic() {
        let store: UniqueStrStore = UniqueStrStore::new_with_capacity(10);
        assert_eq!(store.len(), LATIN1_NUM as usize, "Store length should be {LATIN1_NUM}");

        let test = ["", " ", "a", "Z", "1", "2", "3", "/", ",", ")"];
        for s in test {
            assert!(store.contains(s), "store should contains('{s}')");
        }

        let i: u32 = store.insert(HELLO);
        let num: usize = LATIN1_NUM as usize + 1;

        assert_eq!(store.len(), num, "Store length should be {num}");
        assert!(store.contains(HELLO), "Store does not contain '{HELLO}': {store:?}");
        assert_eq!(store.get(i).unwrap(), HELLO, "get({i}) should == '{HELLO}'");
        assert_eq!(unsafe { store.borrow_str(i) }, HELLO, "get_unchecked({i}) should == '{HELLO}'");

        // Test the pointer structs.
        let stored: StoredStr = store.get_ref(HELLO).expect("StoredStr should be returned");
        assert_eq!(stored.idx(), i, "StoredStr index should be {i}");
        let ptr: StoredStrPtr = stored.as_ptr();
        assert!(!ptr.is_null(), "StoredStrPtr should not be null: {ptr:?}");
        assert_eq!(ptr.as_str(), HELLO, "Pointer string should be '{HELLO}': {ptr:?}");
    }

    #[test]
    fn test_unique_store_shared() {
        let foo_s: &'static str = "foo";
        let store: Arc<UniqueStrStore> = UniqueStrStore::new_with_capacity(10).shared();
        let stored: StoredStr = store.insert_or_get(HELLO);
        let start: u32 = LATIN1_NUM;

        assert_eq!(store.len(), start as usize + 1, "Store length should be {}", start + 1);
        assert!(store.contains(HELLO), "Store does not contain '{HELLO}': {store:?}");

        let again: StoredStr = store.insert_or_get(HELLO);
        let foo: StoredStr = store.insert_or_get(foo_s);
        assert_eq!(store.len(), start as usize + 2, "Store length should be {}", start + 2);

        assert_eq!(stored.idx(), start, "'{HELLO}' idx should be {start}: {stored:?}");
        assert_eq!(again.idx(), start, "Second '{HELLO}!' idx should be again {start}: {again:?}");
        assert_eq!(foo.idx(), start + 1, "'{foo_s}' idx should be {}: {foo:?}", start + 1);

        assert_eq!(stored.as_ref(), HELLO, "as_ref() should == '{HELLO}': {stored:?}");
        assert_eq!(stored, again, "StoredStr instances should be equal: {stored:?} != {again:?}");
        assert_eq!(store.get(start).unwrap(), HELLO, "get({start}) should == '{HELLO}'");
        assert_eq!(
            unsafe { store.borrow_str(start + 1) },
            foo_s,
            "get_unchecked({}) should == '{foo_s}'",
            start + 1
        );
    }

    #[test]
    fn test_concurrent_inserts() {
        use std::thread;

        let exp_len: usize = CONC_S_NUM + LATIN1_NUM as usize;
        let store: UniqueStrStore = UniqueStrStore::new_with_capacity(exp_len);

        let threads: Vec<_> = (0..CONC_T_NUM)
            .map(|t: usize| {
                let store = store.clone();
                thread::spawn(move || {
                    (0..CONC_S_NUM / CONC_T_NUM).for_each(|i: usize| {
                        let s: String = format!("{HELLO} t: {t}, i: {i}");
                        let stored: StoredStr = store.insert_or_get(&s);
                        assert_eq!(stored.as_ref(), s, "Stored string should be '{s}': {stored:?}");
                    })
                })
            })
            .collect();

        for t in threads {
            t.join().unwrap();
        }

        store.validate_contents().ok(); // will panic on failure in debug mode
        assert_eq!(store.len(), exp_len, "Stored num should be {}", exp_len);
    }

    #[test]
    fn test_competing_inserts() {
        use std::thread;

        let per_thread: usize = CONC_S_NUM / CONC_T_NUM;
        let exp_len: usize = per_thread + LATIN1_NUM as usize;
        let store: UniqueStrStore = UniqueStrStore::new_with_capacity(exp_len);

        let threads: Vec<_> = (0..CONC_T_NUM)
            .map(|_t| {
                let store = store.clone();
                thread::spawn(move || {
                    (0..per_thread).for_each(|i: usize| {
                        let s: String = format!("{HELLO} i: {i}");
                        let stored: StoredStr = store.insert_or_get(&s);
                        assert_eq!(stored.as_ref(), s, "Stored string should be '{s}': {stored:?}");
                    })
                })
            })
            .collect();

        for t in threads {
            t.join().unwrap();
        }

        store.validate_contents().ok(); // will panic on failure in debug mode
        assert_eq!(store.len(), exp_len, "Stored num should be {}", exp_len);
    }

    #[rustfmt::skip]
    #[test]
    fn test_split_and_store() {
        let store: UniqueStrStore = UniqueStrStore::new();
        let input: &str = ",apple,banana,cherry,cake,,cake,,,";
        let exp_v: Vec<&str> = vec!["", "apple", "banana", "cherry", "cake", "", "cake", "", "", ""];
        let delim: &str = ",";
        // 4 uniq parts (delim + empty string already should exist)
        let mut exp_store_len: usize = (LATIN1_NUM + 4) as usize;

        let (indices, d) = store.split_and_store(input, delim);
        assert_eq!(indices.len(), exp_v.len(), "{indices:?}");
        assert_eq!(store.len(), exp_store_len);

        // Check that the delimiter is stored
        assert!(store.contains(delim), "Store should contain the delim: '{delim}'");
        assert_eq!(store.get(d).unwrap(), delim);

        // Check that the parts are stored correctly
        for (i, &idx) in indices.iter().enumerate() {
            let exp: &str = exp_v[i];
            assert_eq!(store.get(idx).unwrap(), exp, "index {i}: '{exp}'");
        }

        // Check that the original string can be reconstructed
        let built: String = indices
            .iter()
            .map(|&idx| unsafe { store.borrow_str(idx) })
            .collect::<Vec<&str>>()
            .join(delim);

        assert_eq!(input, built, "Reconstructed string should be '{input}'");
        assert_eq!(
            input,
            store.reconstruct(&indices, d).unwrap(),
            "input <-> reconstruct() mismatch"
        );

        /* --------------------------------- */

        // Check for incorrect delimiter handling
        let delim: &str = ";";
        let (indices, d) = store.split_and_store(input, delim);
        exp_store_len += 1; // +1 new part
        assert_eq!(indices.len(), 1, "{indices:?}");
        assert_eq!(store.len(), exp_store_len);
        assert_eq!(store.get(d).unwrap(), delim, "Store should have the next delim: '{delim}'");
        assert!(store.contains(input), "Store should contain '{input}' (not split)");

        assert_eq!(
            input,
            store.reconstruct(&indices, d).unwrap(),
            "input <-> reconstruct() mismatch (not split)"
        );

        store.validate_contents().ok();
    }

    #[rustfmt::skip]
    #[test]
    fn test_store_path() {
        let store: UniqueStrStore = UniqueStrStore::new();
        let path1: &str = "/home/user/foo/bar/garbage.txt";
        let exp_1: Vec<&str> = vec!["", "home", "user", "foo", "bar", "garbage.txt"];
        let parts1: Vec<u32> = store.store_path(path1);

        // 5 uniq parts (delim + empty string already should exist)
        let mut exp_store_len: usize = (LATIN1_NUM + 5) as usize;

        // 6 returned indices expected, not 5, since it includes the
        // empty string at the 1st index, as this is an absolute path
        assert_eq!(parts1.len(), exp_1.len(), "{parts1:?}");
        assert_eq!(store.len(), exp_store_len);

        // Check that the delimiter is stored
        assert!(store.contains(PATH_SEP), "Store should contain the delim: '{PATH_SEP}'");

        // Check that the parts are stored correctly
        for (i, &idx) in parts1.iter().enumerate() {
            let exp: &str = exp_1[i];
            assert_eq!(store.get(idx).unwrap(), exp, "parts1 {i}: '{exp}'");
        }

        // Check that the original string can be reconstructed
        let built: String = parts1
            .iter()
            .map(|&idx| unsafe { store.borrow_str(idx) })
            .collect::<Vec<&str>>()
            .join(PATH_SEP);

        assert_eq!(path1, built, "Reconstructed path should be '{path1}'");
        assert_eq!(
            path1,
            store.reconstruct(&parts1, store.idx(PATH_SEP).unwrap()).unwrap(),
            "input <-> reconstruct() mismatch (path1)"
        );

        /* --------------------------------- */

        // Check for canonicalized path handling
        let path2: &str = "/home/user/./..../foo/../bar/garbage2.txt";
        let exp_2: Vec<&str> = vec!["", "home", "user", "bar", "garbage2.txt"];
        let parts2: Vec<u32> = store.store_path(path2);

        exp_store_len += 1; // 1 new part, as the "dots" should be normalized away
        assert_eq!(parts2.len(), exp_2.len(), "{parts2:?}");
        assert_eq!(store.len(), exp_store_len);

        for (i, &idx) in parts2.iter().enumerate() {
            let exp: &str = exp_2[i];
            assert_eq!(store.get(idx).unwrap(), exp, "parts2 {i}: '{exp}'");
        }

        assert_eq!(
            exp_2.iter().map(|s| *s).collect::<Vec<&str>>().join(PATH_SEP),
            store.reconstruct(&parts2, store.idx(PATH_SEP).unwrap()).unwrap(),
            "input <-> reconstruct() mismatch (path2)"
        );

        /* --------------------------------- */

        // Check for relative path handling
        let path3: &str = "veri/sekrit/.///hidn/../lokas\0juun/garbage.1";
        let exp_3: Vec<&str> = vec!["veri", "sekrit", "lokasjuun", "garbage.1"];
        let parts3: Vec<u32> = store.store_path(path3);

        exp_store_len += exp_3.len();
        assert_eq!(parts3.len(), exp_3.len(), "{parts3:?}");
        assert_eq!(store.len(), exp_store_len);

        for (i, &idx) in parts3.iter().enumerate() {
            let exp: &str = exp_3[i];
            assert_eq!(store.get(idx).unwrap(), exp, "parts3 {i}: '{exp}'");
        }

        assert_eq!(
            exp_3.iter().map(|s| *s).collect::<Vec<&str>>().join(PATH_SEP),
            store.reconstruct(&parts3, store.idx(PATH_SEP).unwrap()).unwrap(),
            "input <-> reconstruct() mismatch (path3)"
        );
    }

    #[test]
    fn test_error_display() {
        let err = StringStoreError::oob(42, 10);
        assert_eq!(err.to_string(), "index 42 out of bounds (max: 10)");

        let err = StringStoreError::reconstruction(5, 2, 4);
        assert_eq!(err.to_string(), "invalid index 5 at position 2 (max: 4)");

        let err = StringStoreError::InvalidPath("contains null byte".to_string());
        assert_eq!(err.to_string(), "invalid path: contains null byte");
    }

    #[test]
    fn test_error_debug() {
        let err = StringStoreError::StoreFull;
        assert_eq!(format!("{err:?}"), "StoreFull");

        let err = StringStoreError::delimiter("Empty delimiter");
        assert_eq!(format!("{err:?}"), r#"InvalidDelimiter("Empty delimiter")"#);
    }
}

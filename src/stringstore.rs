// Copyright (c) 2024 Mikko Tanner. All rights reserved.

#![allow(dead_code)]

use crate::hashing::{hash_bytes, CustomXxh3Hasher};
use crate::utils::normalize_path;
use parking_lot::RwLock;
use regex::{escape, Regex};
use size_of::{Context, SizeOf};
use std::{
    cmp::Ordering,
    collections::HashMap,
    fmt::{self, Debug, Display, Formatter},
    hash::{BuildHasher, Hash, Hasher},
    mem,
    ops::Deref,
    path::{Path, PathBuf},
    str::Split,
    sync::{
        atomic::{AtomicU32, Ordering::Relaxed},
        Arc,
    },
};

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
- ISO-8859-1 codepoints: contained implicitly, occupying indices 0-255.

## Design Considerations
- Uses a [Vec<Box<str>>] for string storage, which is efficient for random access,
  and should help with cache locality as well.
- Uses a [HashMap] with `u64` Xxh3 string hashes as keys for fast lookups.
- Custom [xxhash_rust] hasher ([CustomXxh3Hasher]) for potentially faster hashing.
- Thread-safe due to wrapping storage/index fields with [RwLock]s.
- trait [SharedStrStore]: is used to lock certain methods behind a shared reference.
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
- Methods which return a [StoredStr] reference can only be used if the
  [UniqueStrStore] is wrapped in an [Arc] (as it must point back to the store).

## Example
```
use statter::stringstore::UniqueStrStore;

let hello: &'static str = "Hello, world!";
let store = UniqueStrStore::new();
assert_eq!(store.len(), 256); // incl. ISO-8859-1 codepoints

let hello_id = store.insert(hello);
assert_eq!(store.len(), 256 + 1);
assert!(store.contains(hello));
assert_eq!(hello_id, 256, "hello string stored at index 256");

// try to insert the same string again
let hello_id2 = store.insert(hello);
assert_eq!(hello_id, hello_id2);
assert_eq!(store.get(hello_id).unwrap(), hello);

let foo_id = store.insert("foo");
assert_eq!(store.len(), 256 + 2);
assert_eq!(foo_id, 257, "foo string stored at index 257");

// panics if the index is out of bounds
assert_eq!(unsafe { store.get_raw(foo_id) }, "foo");

// check internal consistency
store.validate_contents().expect("Store validation failed");
*/
#[derive(Default, Debug)]
pub struct UniqueStrStore {
    store: RwLock<Vec<Box<str>>>,
    index: RwLock<HashMap<u64, u32, CustomXxh3Hasher>>,
    ascii: Arc<Vec<Box<str>>>,
    len: AtomicU32,
}

impl UniqueStrStore {
    /// Create a new [UniqueStrStore] with a default capacity of 128.
    pub fn new() -> Self {
        Self::new_with_capacity(128)
    }

    pub fn new_with_capacity(capacity: usize) -> Self {
        let index =
            HashMap::with_capacity_and_hasher(capacity, CustomXxh3Hasher::default().build_hasher());

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
            store: Vec::with_capacity(capacity).into(),
            index: index.into(),
            ascii: latin1.into(),
            len: AtomicU32::new(LATIN1_NUM),
        }
    }

    /// Put this [UniqueStrStore] into an [Arc].
    pub fn shared(self) -> Arc<Self> {
        self.into()
    }

    /// The number of unique string slices. Includes the ISO-8859-1 codepoints.
    #[inline]
    pub fn len(&self) -> usize {
        self.len.load(Relaxed) as usize
    }

    /// Whether we already have this string slice stored.
    #[inline]
    pub fn contains(&self, s: &str) -> bool {
        if s.len() == 0 {
            return true; // empty string is always contained
        }

        if s.len() == 1 {
            let c: u32 = s.chars().next().unwrap() as u32;
            if c < LATIN1_NUM {
                return true; // ISO-8859-1 implicitly contained
            }
        }

        self.index.read().contains_key(&hash_bytes(s.as_bytes()))
    }

    /// Get the index of a stored string slice by its content, if it exists.
    pub fn idx(&self, s: &str) -> Option<u32> {
        if s.len() == 0 {
            return Some(0);
        }

        if s.len() == 1 {
            let c: u32 = s.chars().next().unwrap() as u32;
            if c < LATIN1_NUM {
                return Some(c);
            }
        }

        self.index
            .read()
            .get(&hash_bytes(s.as_bytes()))
            .map(|i: &u32| i + LATIN1_NUM)
    }

    /// Get a reference to a stored string slice by its index, if it exists.
    pub fn get<'a>(&'a self, idx: u32) -> Result<&'a str, String> {
        let len: usize = self.len();
        if idx as usize >= len {
            return Err("Index {idx} out of bounds (max: {len})".to_string());
        }
        unsafe { Ok(self.get_raw(idx)) }
    }

    /**
    Returns a raw reference to a stored [str]. For a safe alternative, use `get`.

    ### Safety
    Calling this method with an out-of-bounds index will panic.

    ### Details
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
    pub unsafe fn get_raw<'a>(&'a self, idx: u32) -> &'a str {
        // ISO-8859-1 range
        if idx < LATIN1_NUM {
            return &*self.ascii[idx as usize];
        }

        let idx: u32 = idx - LATIN1_NUM;
        let store = self.store.read();
        if idx as usize >= store.len() {
            panic!("Store index {idx} out of bounds (max: {})", store.len());
        }

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
            return index.get(&key).map(|i: &u32| i + LATIN1_NUM).unwrap();
        }

        let idx: u32 = store.len() as u32;
        index.insert(key, idx);
        store.push(s.into());
        self.len.fetch_add(1, Relaxed);
        idx + LATIN1_NUM
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
            let c: u32 = s.chars().next().unwrap() as u32;
            if c < LATIN1_NUM {
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
    splitting is stored and its index returned. A simple tokenizer is used
    for shorter strings, while a regex-based tokenizer handles longer strings
    and/or larger sets of delimiters.

    ## Arguments
    * `s` - a string slice to be atomized
    * `delims` - string slices, based on which `s` shall be split

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
    pub fn split_and_store_multi(&self, s: &str, delims: &[&str]) -> (Vec<u32>, Vec<u32>) {
        let complexity: usize = s.len() * delims.len(); // rough estimate
        let mut result: Vec<u32> = vec![];
        let mut delim_indices: Vec<u32> = vec![];

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

        // TODO: evaluate thresholds for switching between tokenizers
        if complexity < 10000 || delims.len() <= 10 {
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
        let s: PathBuf = normalize_path(s);
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
    pub fn reconstruct(&self, indices: &[u32], delim: u32) -> Result<String, String> {
        let parts_num: usize = indices.len();
        // special case: empty string
        if parts_num == 0 || (parts_num == 1 && indices[0] == 0) {
            return Ok(EMPTY_STR.to_string());
        } else if parts_num > u32::MAX as usize {
            return Err("Size is larger than u32::MAX".to_string());
        }

        // delimiter check
        // we lock the store at this point so that the length is stable
        let store = self.store.read();
        let stored_num: u32 = self.len() as u32;
        if delim >= stored_num {
            return Err("Delimiter index {idx} out of bounds".to_string());
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
                return Err("String index {idx} (pos: {i}) out of bounds".to_string());
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
        let index = self.index.write();
        let len: usize = self.len();
        let mut errs: Vec<String> = Vec::new();

        let l_store: usize = store.len();
        let l_index: usize = index.len();
        if l_store + LATIN1_NUM as usize != len {
            errs.push(format!("store.len() ({l_store}) != stored length ({len})"));
        };
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
        self.idx(s).map(|idx: u32| StoredStr(idx, self.clone()))
    }

    /// Insert a new string (slice) and return its [StoredStr] reference.
    ///
    /// If the string (slice) already exists, return its reference instead.
    fn insert_or_get<T>(&self, s: T) -> StoredStr
    where
        T: Into<String>,
    {
        let s: String = s.into();
        if s.is_empty() {
            return StoredStr(0, self.clone());
        }
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
        self.ascii.size_of_children(context);
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
        unsafe { (*self.1).get_raw(self.0) }
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
        write!(f, "StoredStr({}: {:?})", self.0, self.reference())
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
            let ptr: *const str = (*v.1).get_raw(v.0) as *const str;
            &*ptr
        }
    }
}

/* ######################################################################### */

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

/* ######################################################################### */

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
        assert_eq!(unsafe { store.get_raw(i) }, HELLO, "get_unchecked({i}) should == '{HELLO}'");
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
            unsafe { store.get_raw(start + 1) },
            foo_s,
            "get_unchecked({}) should == '{foo_s}'",
            start + 1
        );
    }

    #[test]
    fn test_concurrent_inserts() {
        use std::thread;

        let exp_len: usize = CONC_S_NUM + LATIN1_NUM as usize;
        let store: Arc<UniqueStrStore> = UniqueStrStore::new_with_capacity(exp_len).shared();

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
        let store: Arc<UniqueStrStore> = UniqueStrStore::new_with_capacity(exp_len).shared();

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
            .map(|&idx| unsafe { store.get_raw(idx) })
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
            .map(|&idx| unsafe { store.get_raw(idx) })
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
}

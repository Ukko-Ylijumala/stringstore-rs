// Copyright (c) 2024-2026 Mikko Tanner. All rights reserved.

/*!
Tokenization: the byte-class-table scanner and its entry points.

The scanner is pure — it does not depend on a [UniqueStrStore]; the
store-aware wrapper is `UniqueStrStore::split_and_store_multi`. See
`doc/design/tokenization.md` for the matching contract, the design of
the scanner and the empty-delimiter footgun.

[UniqueStrStore]: crate::UniqueStrStore
*/

use std::fmt::{self, Display, Formatter};

#[derive(Default, Debug, PartialEq, Eq)]
/// A token (part) of a delimited string, which has been processed (tokenized).
/// It can be a delimiter, or a regular part.
pub struct Token {
    content: String,
    is_delim: bool,           // default: false
    delim_idx: Option<usize>, // default: None
}

impl Token {
    /// The text content of this token.
    #[inline]
    pub fn content(&self) -> &str {
        &self.content
    }

    /// Whether this token matched one of the delimiters.
    #[inline]
    pub fn is_delim(&self) -> bool {
        self.is_delim
    }

    /// The position of the matched delimiter in the original `delims` slice
    /// (None for non-delimiter tokens).
    #[inline]
    pub fn delim_idx(&self) -> Option<usize> {
        self.delim_idx
    }
}

/// The token's text, delimiter or not.
impl Display for Token {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.write_str(&self.content)
    }
}

/// Classification of a byte for the scanner's 256-entry lookup table.
#[derive(Clone, Copy, PartialEq, Eq)]
enum ByteClass {
    /// Nothing to check here; the scanner just advances.
    Plain,
    /// At least one non-empty delimiter starts with this byte.
    DelimStart,
}

/**
The scanner's per-byte lookup table. Every byte of the input is looked up
here first, and only a `DelimStart` byte triggers the delimiter comparison
loop — so for typical text the per-byte cost is one table load.

This is the extension point for richer tokenization later on (character
class delimiters, repeated-character runs, enclosures that suppress
splitting): each becomes another [ByteClass] variant and a branch in
`scan_tokens`, without changing the entry points.
*/
struct ScanTable([ByteClass; 256]);

impl ScanTable {
    /// Build the table for `delims`; `None` if no delimiter is usable
    /// (all empty or none given), in which case nothing can ever match.
    fn new(delims: &[&str]) -> Option<Self> {
        let mut table: [ByteClass; 256] = [ByteClass::Plain; 256];
        let mut usable: bool = false;
        for d in delims {
            if let Some(&b) = d.as_bytes().first() {
                table[b as usize] = ByteClass::DelimStart;
                usable = true;
            }
        }
        usable.then_some(Self(table))
    }

    #[inline]
    fn class(&self, b: u8) -> ByteClass {
        self.0[b as usize]
    }
}

/**
The scanner core behind every tokenizing entry point. Calls `f` once per
token in order, with the token's slice of `s` and, for a delimiter, the
position of the matched delimiter in `delims`. Non-delimiter tokens are
never empty.

Matching is leftmost-first: at the leftmost position where any delimiter
matches, the one that appears earliest in `delims` wins, and scanning
resumes after it. Empty delimiters never match.

## Why stepping byte-wise is safe on UTF-8

A delimiter is valid UTF-8, so its first byte is never a continuation
byte (`0x80..=0xBF`) and the table marks no continuation byte as
`DelimStart`. A match can therefore only start on a char boundary, and
since a delimiter consists of whole chars it also ends on one, so every
slice taken below is on char boundaries. Bytes that do not start a match
are simply skipped, whether or not they are inside a multibyte char.
*/
pub(crate) fn scan_tokens<'a, F>(s: &'a str, delims: &[&str], mut f: F)
where
    F: FnMut(&'a str, Option<usize>),
{
    let Some(table) = ScanTable::new(delims) else {
        // no usable delimiters: the whole input is one token (or nothing)
        if !s.is_empty() {
            f(s, None);
        }
        return;
    };

    let bytes: &[u8] = s.as_bytes();
    let mut i: usize = 0; // scan cursor
    let mut start: usize = 0; // start of the pending non-delimiter token
    while i < bytes.len() {
        if table.class(bytes[i]) == ByteClass::DelimStart {
            let hit = delims
                .iter()
                .enumerate()
                .find(|(_, d)| !d.is_empty() && bytes[i..].starts_with(d.as_bytes()));
            if let Some((d_idx, d)) = hit {
                if start < i {
                    f(&s[start..i], None);
                }
                let end: usize = i + d.len();
                f(&s[i..end], Some(d_idx));
                i = end;
                start = end;
                continue;
            }
        }
        i += 1;
    }

    if start < bytes.len() {
        f(&s[start..], None);
    }
}

/**
Tokenize a string by a set of delimiters and return a [Vec] of [Token]s.
The delimiters are included as separate tokens.

Matching is leftmost-first with the earliest delimiter in `delims`
winning at a given position; see `scan_tokens` and
`doc/design/tokenization.md`. Empty delimiters are skipped without
renumbering the others.
*/
pub fn tokenize(s: &str, delims: &[&str]) -> Vec<Token> {
    let mut tokens: Vec<Token> = Vec::new();
    scan_tokens(s, delims, |content: &str, delim_idx: Option<usize>| {
        tokens.push(Token {
            content: content.to_string(),
            is_delim: delim_idx.is_some(),
            delim_idx,
        })
    });
    tokens
}

/**
Identical to [tokenize]; retained for API compatibility.

This used to compile a regex alternation of the delimiters. The single
scanner now outperforms that at every input size measured (the regex
compile alone cost ~12 µs per call), so both names share one
implementation and always produce the same output.
*/
#[inline]
pub fn tokenize_regex(s: &str, delims: &[&str]) -> Vec<Token> {
    tokenize(s, delims)
}

/* ################################ TESTS ################################## */

#[cfg(test)]
mod tests {
    use super::*;

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
        for (i, token) in tokens.iter().enumerate() {
            let exp: &str = TOKENS_EXPECTED[i];
            assert_eq!(token.content, exp, "token {i}: {token:?} (tokens: {tokens:#?})");
        }
    }

    #[rustfmt::skip]
    #[test]
    fn test_tokenize_regex() {
        let tokens: Vec<Token> = tokenize_regex(TOKEN_TEST, &TOKEN_DELIMS);

        assert_eq!(tokens.len(), TOKENS_LEN, "len failed, tokens:\n{tokens:#?}");
        for (i, token) in tokens.iter().enumerate() {
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
    fn test_tokenize_empty_delim() {
        // An empty delimiter must be skipped rather than hanging the loop.
        let delims: [&str; 3] = [",", "", " "];
        let tokens: Vec<Token> = tokenize("a, b", &delims);

        assert_eq!(tokens.len(), 4, "tokens: {tokens:#?}");
        assert_eq!(tokens[0].content, "a");
        assert_eq!(tokens[1].content, ",");
        assert_eq!(tokens[1].delim_idx, Some(0));
        assert_eq!(tokens[2].content, " ");
        assert_eq!(tokens[2].delim_idx, Some(2));
        assert_eq!(tokens[3].content, "b");
    }

    #[test]
    fn test_tokenize_regex_empty_delim() {
        let delims: [&str; 3] = [",", "", " "];
        let tokens: Vec<Token> = tokenize_regex("a, b", &delims);

        assert_eq!(tokens.len(), 4, "tokens: {tokens:#?}");
        assert_eq!(tokens[0].content, "a");
        assert_eq!(tokens[1].content, ",");
        assert_eq!(tokens[1].delim_idx, Some(0));
        assert_eq!(tokens[2].content, " ");
        assert_eq!(tokens[2].delim_idx, Some(2));
        assert_eq!(tokens[3].content, "b");
    }

    #[test]
    fn test_tokenize_all_empty_delims() {
        // When every delimiter is empty (or there are none), the input passes
        // through as a single non-delim token.
        let delims: [&str; 2] = ["", ""];
        let tokens: Vec<Token> = tokenize("hello", &delims);
        assert_eq!(tokens.len(), 1);
        assert_eq!(tokens[0].content, "hello");
        assert!(!tokens[0].is_delim);

        let tokens: Vec<Token> = tokenize_regex("hello", &delims);
        assert_eq!(tokens.len(), 1);
        assert_eq!(tokens[0].content, "hello");
        assert!(!tokens[0].is_delim);
    }

    #[test]
    fn test_tokenize_multibyte() {
        // The linear tokenizer used to advance by one byte after pushing a
        // full char, panicking on a mid-char slice for any non-ASCII input.
        let input: &str = "héllo wörld,日本語";
        let expected: [&str; 5] = ["héllo", " ", "wörld", ",", "日本語"];

        let tokens: Vec<Token> = tokenize(input, &[" ", ","]);
        assert_eq!(tokens.len(), expected.len(), "tokens: {tokens:#?}");
        for (token, exp) in tokens.iter().zip(expected) {
            assert_eq!(token.content, exp);
        }

        // Both tokenizers must agree on multibyte input.
        let tokens_re: Vec<Token> = tokenize_regex(input, &[" ", ","]);
        assert!(tokens == tokens_re, "tokenizers diverge:\n{tokens:#?}\nvs\n{tokens_re:#?}");

        // Multibyte delimiters must work too.
        let tokens: Vec<Token> = tokenize("a→b", &["→"]);
        assert_eq!(tokens.len(), 3, "tokens: {tokens:#?}");
        assert_eq!(tokens[0].content, "a");
        assert_eq!(tokens[1].content, "→");
        assert!(tokens[1].is_delim);
        assert_eq!(tokens[2].content, "b");
    }

    /**
    Reference oracle for `scan_tokens`: the original char-by-char linear
    tokenizer, kept verbatim. It is O(n * m) and allocates per char, but its
    semantics (leftmost-first, earliest delimiter wins, empty delimiters
    skipped, advance by whole chars) are exactly what the scanner must
    reproduce.
    */
    fn reference_tokenize(s: &str, delims: &[&str]) -> Vec<Token> {
        let mut tokens: Vec<Token> = Vec::new();
        let mut current_token: String = String::new();
        let mut i: usize = 0;

        while i < s.len() {
            if let Some((d_idx, delimiter)) = delims
                .iter()
                .enumerate()
                .find(|(_, &d)| !d.is_empty() && s[i..].starts_with(d))
            {
                if !current_token.is_empty() {
                    tokens.push(Token {
                        content: std::mem::take(&mut current_token),
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
                let c: char = s[i..].chars().next().unwrap();
                current_token.push(c);
                i += c.len_utf8();
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

    #[test]
    fn test_scanner_matches_reference() {
        /*
        The byte-class scanner must agree with the reference tokenizer on
        every input, including overlapping delimiters ("ab" vs "abc",
        "a" vs "aa"), multibyte chars, and duplicated or empty delimiters.
        Deterministic LCG so a failure is reproducible from the seed.
        */
        const ALPHABET: [&str; 4] = ["a", "b", "c", "é"];
        const DELIM_POOL: [&str; 11] =
            ["a", "b", "c", "ab", "ba", "abc", "bc", "aa", "é", "aé", ""];
        let mut seed: u64 = 42;
        let mut next = move |modulus: u64| -> usize {
            seed = seed
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            ((seed >> 33) % modulus) as usize
        };

        for case in 0..5000 {
            let n_delims: usize = 1 + next(5);
            let delims: Vec<&str> = (0..n_delims).map(|_| DELIM_POOL[next(11)]).collect();
            let len: usize = next(12);
            let s: String = (0..len).map(|_| ALPHABET[next(4)]).collect();

            let expected: Vec<Token> = reference_tokenize(&s, &delims);
            let actual: Vec<Token> = tokenize(&s, &delims);
            assert_eq!(
                actual, expected,
                "case {case}: scanner diverges on {s:?} with delims {delims:?}"
            );
            assert_eq!(tokenize_regex(&s, &delims), expected, "tokenize_regex must alias");
        }
    }

    #[test]
    fn test_token_accessors() {
        let tokens: Vec<Token> = tokenize("a,b", &[","]);
        assert_eq!(tokens.len(), 3);
        assert_eq!(tokens[0].content(), "a");
        assert!(!tokens[0].is_delim());
        assert_eq!(tokens[0].delim_idx(), None);
        assert_eq!(tokens[1].content(), ",");
        assert!(tokens[1].is_delim());
        assert_eq!(tokens[1].delim_idx(), Some(0));
    }

    #[test]
    fn test_token_display() {
        let tokens: Vec<Token> = tokenize("a,b", &[","]);
        let joined: String = tokens.iter().map(Token::to_string).collect();
        assert_eq!(joined, "a,b");
    }
}

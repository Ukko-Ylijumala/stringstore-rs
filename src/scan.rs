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

/// What kind of piece of the input a [Token] is.
#[derive(Default, Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenKind {
    /// Text between delimiters; never empty.
    #[default]
    Text,
    /// A literal delimiter from [ScanRules::new]'s slice.
    Delim,
    /// A maximal run of one run byte ([ScanRules::runs]); its length is the count.
    Run,
    /// The opening string of an enclosure pair ([ScanRules::enclosures]).
    Open,
    /// The closing string of an enclosure pair.
    Close,
}

#[derive(Default, Debug, PartialEq, Eq)]
/// A token (part) of a delimited string, which has been processed (tokenized).
/// It can be a delimiter, or a regular part.
pub struct Token {
    content: String,
    kind: TokenKind,          // default: Text
    delim_idx: Option<usize>, // default: None
}

impl Token {
    /// The text content of this token.
    #[inline]
    pub fn content(&self) -> &str {
        &self.content
    }

    /// Which kind of piece this token is.
    #[inline]
    pub fn kind(&self) -> TokenKind {
        self.kind
    }

    /// Whether this token split the text: anything but [TokenKind::Text].
    #[inline]
    pub fn is_delim(&self) -> bool {
        self.kind != TokenKind::Text
    }

    /// The position of the matched delimiter in the original `delims` slice,
    /// or for an `Open` / `Close` token the position of its pair in the
    /// `enclosures` slice (None for text and run tokens).
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

impl From<Scanned<'_>> for Token {
    fn from(piece: Scanned<'_>) -> Self {
        let (content, kind, delim_idx) = match piece {
            Scanned::Text(s) => (s, TokenKind::Text, None),
            Scanned::Delim(s, d) => (s, TokenKind::Delim, Some(d)),
            Scanned::Run(s) => (s, TokenKind::Run, None),
            Scanned::Open(s, p) => (s, TokenKind::Open, Some(p)),
            Scanned::Close(s, p) => (s, TokenKind::Close, Some(p)),
        };
        Self {
            content: content.to_string(),
            kind,
            delim_idx,
        }
    }
}

/**
The splitting rules for the scanner: what separates one piece of text from
the next. All three kinds are optional and borrowed, so a rule set made of
literals costs nothing to build; `tokenize` builds one per call.

- **Literal delimiters** (`new`): matched leftmost-first, earliest in the
  slice winning at a position. Empty entries never match.
- **Run bytes** (`runs`): ASCII characters, each maximal run of which is
  one piece of its own — `"   "` is one run of three, not three
  delimiters. Non-ASCII characters in the string are ignored.
- **Enclosures** (`enclosures`): `(open, close)` pairs. From an `open`
  to its matching `close` nothing splits: the enclosed text is one piece,
  whatever delimiters it contains. Distinct pairs nest (`(a (b) c)`
  closes at the last parenthesis); a pair whose two strings are equal,
  such as quotes, cannot. An unterminated enclosure runs to the end of
  the input. Pairs with an empty string never match.

Where several rules apply at one position, an enclosure opening wins
over a literal delimiter, which wins over a run.
*/
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ScanRules<'r> {
    delims: &'r [&'r str],
    runs: &'r str,
    enclosures: &'r [(&'r str, &'r str)],
}

impl<'r> ScanRules<'r> {
    /// Rules with these literal delimiters and nothing else.
    pub const fn new(delims: &'r [&'r str]) -> Self {
        Self {
            delims,
            runs: "",
            enclosures: &[],
        }
    }

    /// The ASCII characters whose runs are pieces of their own.
    pub const fn runs(self, runs: &'r str) -> Self {
        Self { runs, ..self }
    }

    /// The `(open, close)` pairs inside which nothing splits.
    pub const fn enclosures(self, enclosures: &'r [(&'r str, &'r str)]) -> Self {
        Self { enclosures, ..self }
    }

    /// The enclosure pair opening at `bytes[i..]`, earliest in the slice first.
    #[inline]
    fn opening_at(&self, bytes: &[u8]) -> Option<(usize, (&'r str, &'r str))> {
        self.enclosures
            .iter()
            .copied()
            .enumerate()
            .find(|(_, (open, close))| {
                !open.is_empty() && !close.is_empty() && bytes.starts_with(open.as_bytes())
            })
    }

    /// The literal delimiter matching at `bytes[i..]`, earliest in the slice first.
    #[inline]
    fn delim_at(&self, bytes: &[u8]) -> Option<(usize, &'r str)> {
        self.delims
            .iter()
            .copied()
            .enumerate()
            .find(|(_, d)| !d.is_empty() && bytes.starts_with(d.as_bytes()))
    }
}

/// One piece of the input, as [scan] reports it: always a slice of the
/// input, so that the pieces concatenate back to it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Scanned<'a> {
    /// Text between delimiters; never empty.
    Text(&'a str),
    /// A literal delimiter and its position in the `delims` slice.
    Delim(&'a str, usize),
    /// A maximal run of one run byte; the slice's length is the count.
    Run(&'a str),
    /// An enclosure's opening string and the pair's position; the enclosed
    /// text follows as one `Text` (if any), then the `Close`.
    Open(&'a str, usize),
    /// An enclosure's closing string and the pair's position.
    Close(&'a str, usize),
}

/**
Classification of a byte for the scanner's 256-entry lookup table: a set
of the rules that *may* apply at a byte. Most bytes are in no set, and
the scanner just advances over them; the others trigger the (slower)
rule-matching code for the sets they are in.
*/
#[derive(Clone, Copy, PartialEq, Eq)]
struct ByteClass(u8);

impl ByteClass {
    const PLAIN: Self = Self(0);
    /// At least one non-empty literal delimiter starts with this byte.
    const DELIM_START: u8 = 0b1;
    /// This byte is a run byte.
    const RUN: u8 = 0b10;
    /// At least one usable enclosure's opening string starts with this byte.
    const OPEN: u8 = 0b100;

    #[inline]
    fn starts_delim(self) -> bool {
        self.0 & Self::DELIM_START != 0
    }
    #[inline]
    fn runs(self) -> bool {
        self.0 & Self::RUN != 0
    }
    #[inline]
    fn opens(self) -> bool {
        self.0 & Self::OPEN != 0
    }
}

/**
The scanner's per-byte lookup table. Every byte of the input is looked up
here first, and only a byte in some [ByteClass] set triggers the matching
code for that set — so for typical text the per-byte cost is one table
load.

A new splitting rule is another `ByteClass` bit plus a branch in [scan];
the entry points and [Token] need not change.
*/
struct ScanTable([ByteClass; 256]);

impl ScanTable {
    /// Build the table for `rules`; `None` if no rule is usable (nothing
    /// given, or only empty strings), in which case nothing can ever match.
    fn new(rules: &ScanRules<'_>) -> Option<Self> {
        let mut table: [ByteClass; 256] = [ByteClass::PLAIN; 256];
        let mut mark = |b: u8, bit: u8| table[b as usize].0 |= bit;
        let mut usable: bool = false;
        for d in rules.delims {
            if let Some(&b) = d.as_bytes().first() {
                mark(b, ByteClass::DELIM_START);
                usable = true;
            }
        }
        // only ASCII: a run byte must be a whole char for the slices to stay valid
        for b in rules.runs.bytes().filter(u8::is_ascii) {
            mark(b, ByteClass::RUN);
            usable = true;
        }
        for (open, close) in rules.enclosures {
            if let (Some(&b), false) = (open.as_bytes().first(), close.is_empty()) {
                mark(b, ByteClass::OPEN);
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
piece of `s` in order; see [Scanned] for the pieces and [ScanRules] for
what splits. Text pieces are never empty, and the pieces concatenate back
to `s`.

## Why stepping byte-wise is safe on UTF-8

Every rule string is valid UTF-8, so its first byte is never a
continuation byte (`0x80..=0xBF`) and the table marks no continuation
byte; run bytes are ASCII. A match can therefore only start on a char
boundary, and since a rule string consists of whole chars it also ends
on one, so every slice taken below is on char boundaries. Bytes that do
not start a match are simply skipped, whether or not they are inside a
multibyte char. The search for a closing string compares at every byte,
but its first byte is not a continuation byte either, so it can only
match on a boundary too.
*/
pub(crate) fn scan<'a, F>(rules: &ScanRules<'_>, s: &'a str, mut f: F)
where
    F: FnMut(Scanned<'a>),
{
    let Some(table) = ScanTable::new(rules) else {
        // no usable rule: the whole input is one piece (or nothing)
        if !s.is_empty() {
            f(Scanned::Text(s));
        }
        return;
    };

    let bytes: &[u8] = s.as_bytes();
    let mut i: usize = 0; // scan cursor
    let mut start: usize = 0; // start of the pending text piece
    while i < bytes.len() {
        let class: ByteClass = table.class(bytes[i]);
        if class == ByteClass::PLAIN {
            i += 1;
            continue;
        }

        if class.opens() {
            if let Some((p, (open, close))) = rules.opening_at(&bytes[i..]) {
                emit_text(&mut f, s, start, i);
                let body: usize = i + open.len();
                f(Scanned::Open(&s[i..body], p));
                let Some(c) = find_close(bytes, body, open, close) else {
                    // unterminated: the rest of the input is the enclosed text
                    emit_text(&mut f, s, body, bytes.len());
                    return;
                };
                emit_text(&mut f, s, body, c);
                let end: usize = c + close.len();
                f(Scanned::Close(&s[c..end], p));
                i = end;
                start = end;
                continue;
            }
        }

        if class.starts_delim() {
            if let Some((d_idx, d)) = rules.delim_at(&bytes[i..]) {
                emit_text(&mut f, s, start, i);
                let end: usize = i + d.len();
                f(Scanned::Delim(&s[i..end], d_idx));
                i = end;
                start = end;
                continue;
            }
        }

        if class.runs() {
            let b: u8 = bytes[i];
            let mut end: usize = i + 1;
            while end < bytes.len() && bytes[end] == b {
                end += 1;
            }
            emit_text(&mut f, s, start, i);
            f(Scanned::Run(&s[i..end]));
            i = end;
            start = end;
            continue;
        }

        i += 1;
    }

    emit_text(&mut f, s, start, bytes.len());
}

/// Report `s[from..to]` as a text piece, unless it is empty.
#[inline]
fn emit_text<'a, F>(f: &mut F, s: &'a str, from: usize, to: usize)
where
    F: FnMut(Scanned<'a>),
{
    if from < to {
        f(Scanned::Text(&s[from..to]));
    }
}

/**
The byte position of the `close` that ends the enclosure whose body
starts at `from`, or `None` if there is none. Distinct `open` / `close`
strings nest: an inner `open` needs its own `close` first. Equal ones
cannot, so the first `close` ends the enclosure.
*/
fn find_close(bytes: &[u8], from: usize, open: &str, close: &str) -> Option<usize> {
    let (open, close) = (open.as_bytes(), close.as_bytes());
    let nests: bool = open != close;
    let mut depth: usize = 0;
    let mut i: usize = from;
    while i < bytes.len() {
        if bytes[i..].starts_with(close) {
            if depth == 0 {
                return Some(i);
            }
            depth -= 1;
            i += close.len();
        } else if nests && bytes[i..].starts_with(open) {
            depth += 1;
            i += open.len();
        } else {
            i += 1;
        }
    }
    None
}

/**
The literal-delimiter scanner behind `tokenize` and
`UniqueStrStore::split_and_store_multi`: [scan] with [ScanRules::new],
reporting each piece as its slice plus, for a delimiter, the position
of the matched delimiter in `delims`.
*/
pub(crate) fn scan_tokens<'a, F>(s: &'a str, delims: &[&str], mut f: F)
where
    F: FnMut(&'a str, Option<usize>),
{
    scan(
        &ScanRules::new(delims),
        s,
        |piece: Scanned<'a>| match piece {
            Scanned::Text(t) => f(t, None),
            Scanned::Delim(d, idx) => f(d, Some(idx)),
            // literal-only rules never produce these
            Scanned::Run(t) | Scanned::Open(t, _) | Scanned::Close(t, _) => f(t, None),
        },
    )
}

/**
Tokenize a string by a set of delimiters and return a [Vec] of [Token]s.
The delimiters are included as separate tokens.

Matching is leftmost-first with the earliest delimiter in `delims`
winning at a given position; see [ScanRules] and
`doc/design/tokenization.md`. Empty delimiters are skipped without
renumbering the others. For run and enclosure rules use [tokenize_with].
*/
pub fn tokenize(s: &str, delims: &[&str]) -> Vec<Token> {
    tokenize_with(&ScanRules::new(delims), s)
}

/**
Tokenize a string by a full set of [ScanRules] — literal delimiters, run
characters and enclosures — and return a [Vec] of [Token]s, each telling
its [TokenKind]. The tokens' contents concatenate back to `s`.
*/
pub fn tokenize_with(rules: &ScanRules<'_>, s: &str) -> Vec<Token> {
    let mut tokens: Vec<Token> = Vec::new();
    scan(rules, s, |piece: Scanned<'_>| {
        tokens.push(Token::from(piece))
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
    use std::mem;

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
            Token { content: "apple".to_string(),  kind: TokenKind::Text,  delim_idx: None },
            Token { content: ",".to_string(),      kind: TokenKind::Delim, delim_idx: Some(0) },
            Token { content: "banana".to_string(), kind: TokenKind::Text,  delim_idx: None },
            Token { content: ",".to_string(),      kind: TokenKind::Delim, delim_idx: Some(0) },
            Token { content: "cherry".to_string(), kind: TokenKind::Delim, delim_idx: Some(1) },
            Token { content: ",".to_string(),      kind: TokenKind::Delim, delim_idx: Some(0) },
            Token { content: "cake".to_string(),   kind: TokenKind::Text,  delim_idx: None },
            Token { content: ",".to_string(),      kind: TokenKind::Delim, delim_idx: Some(0) },
            Token { content: ",".to_string(),      kind: TokenKind::Delim, delim_idx: Some(0) },
            Token { content: "cake".to_string(),   kind: TokenKind::Text,  delim_idx: None }
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
            Token { content: "Mary".to_string(),   kind: TokenKind::Text,  delim_idx: None },
            Token { content: " ".to_string(),      kind: TokenKind::Delim, delim_idx: Some(2) },
            Token { content: "had".to_string(),    kind: TokenKind::Text,  delim_idx: None },
            Token { content: " ".to_string(),      kind: TokenKind::Delim, delim_idx: Some(2) },
            Token { content: "a".to_string(),      kind: TokenKind::Text,  delim_idx: None },
            Token { content: " ".to_string(),      kind: TokenKind::Delim, delim_idx: Some(2) },
            Token { content: "little".to_string(), kind: TokenKind::Text,  delim_idx: None },
            Token { content: " ".to_string(),      kind: TokenKind::Delim, delim_idx: Some(2) },
            Token { content: "lamb".to_string(),   kind: TokenKind::Text,  delim_idx: None },
            Token { content: ",".to_string(),      kind: TokenKind::Delim, delim_idx: Some(0) },
            Token { content: " ".to_string(),      kind: TokenKind::Delim, delim_idx: Some(2) },
            Token { content: "its".to_string(),    kind: TokenKind::Text,  delim_idx: None },
            Token { content: " ".to_string(),      kind: TokenKind::Delim, delim_idx: Some(2) },
            Token { content: "fleece".to_string(), kind: TokenKind::Text,  delim_idx: None },
            Token { content: " ".to_string(),      kind: TokenKind::Delim, delim_idx: Some(2) },
            Token { content: "was".to_string(),    kind: TokenKind::Text,  delim_idx: None },
            Token { content: " ".to_string(),      kind: TokenKind::Delim, delim_idx: Some(2) },
            Token { content: "white".to_string(),  kind: TokenKind::Text,  delim_idx: None },
            Token { content: " ".to_string(),      kind: TokenKind::Delim, delim_idx: Some(2) },
            Token { content: "as".to_string(),     kind: TokenKind::Text,  delim_idx: None },
            Token { content: " ".to_string(),      kind: TokenKind::Delim, delim_idx: Some(2) },
            Token { content: "snow".to_string(),   kind: TokenKind::Text,  delim_idx: None },
            Token { content: ".".to_string(),      kind: TokenKind::Delim, delim_idx: Some(4) },
            Token { content: "\n".to_string(),     kind: TokenKind::Delim, delim_idx: Some(7) },
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
        assert!(!tokens[0].is_delim());

        let tokens: Vec<Token> = tokenize_regex("hello", &delims);
        assert_eq!(tokens.len(), 1);
        assert_eq!(tokens[0].content, "hello");
        assert!(!tokens[0].is_delim());
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
        assert!(tokens[1].is_delim());
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
                    kind: TokenKind::Delim,
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

    /// The tokens of `tokenize_with` as (kind, content) pairs, for compact assertions.
    fn kinds(rules: &ScanRules<'_>, s: &str) -> Vec<(TokenKind, String)> {
        tokenize_with(rules, s)
            .into_iter()
            .map(|t| (t.kind(), t.content().to_string()))
            .collect()
    }

    /// Expected tokens from (kind, content) pairs.
    fn expect(pairs: &[(TokenKind, &str)]) -> Vec<(TokenKind, String)> {
        pairs.iter().map(|&(k, c)| (k, c.to_string())).collect()
    }

    #[rustfmt::skip]
    #[test]
    fn test_runs() {
        use TokenKind::*;
        let rules: ScanRules<'_> = ScanRules::new(&[","]).runs(" \t");
        assert_eq!(
            kinds(&rules, "a  b\t\tc, d"),
            expect(&[
                (Text, "a"), (Run, "  "), (Text, "b"), (Run, "\t\t"), (Text, "c"),
                (Delim, ","), (Run, " "), (Text, "d"),
            ])
        );
        // a run is of one byte: a mix is several runs
        assert_eq!(kinds(&rules, " \t "), expect(&[(Run, " "), (Run, "\t"), (Run, " ")]));
        assert_eq!(kinds(&rules, "  x  "), expect(&[(Run, "  "), (Text, "x"), (Run, "  ")]));
        // non-ASCII run characters are ignored
        assert_eq!(kinds(&ScanRules::new(&[]).runs("é"), "aéé"), expect(&[(Text, "aéé")]));
        // a run token's length is the count; it splits, but has no delimiter index
        let stars: Vec<Token> = tokenize_with(&ScanRules::new(&[]).runs("*"), "***");
        assert_eq!(stars.len(), 1);
        assert_eq!(stars[0].content().len(), 3);
        assert!(stars[0].is_delim());
        assert_eq!(stars[0].delim_idx(), None);
    }

    #[rustfmt::skip]
    #[test]
    fn test_enclosures() {
        use TokenKind::*;
        let pairs: [(&str, &str); 3] = [("\"", "\""), ("(", ")"), ("«", "»")];
        let rules: ScanRules<'_> = ScanRules::new(&[",", " "]).enclosures(&pairs);

        // nothing splits inside, and the pair's position rides along
        let tokens: Vec<Token> = tokenize_with(&rules, "k=\"a, b\",(c d)");
        let got: Vec<(TokenKind, &str, Option<usize>)> =
            tokens.iter().map(|t| (t.kind(), t.content(), t.delim_idx())).collect();
        assert_eq!(
            got,
            [
                (Text, "k=", None), (Open, "\"", Some(0)), (Text, "a, b", None),
                (Close, "\"", Some(0)), (Delim, ",", Some(0)), (Open, "(", Some(1)),
                (Text, "c d", None), (Close, ")", Some(1)),
            ]
        );
        // distinct pairs nest, equal ones cannot
        assert_eq!(
            kinds(&rules, "(a (b) c)"),
            expect(&[(Open, "("), (Text, "a (b) c"), (Close, ")")])
        );
        assert_eq!(
            kinds(&rules, "\"a \"b\" c\""),
            expect(&[
                (Open, "\""), (Text, "a "), (Close, "\""), (Text, "b"),
                (Open, "\""), (Text, " c"), (Close, "\""),
            ])
        );
        // multibyte pair, multibyte body
        assert_eq!(
            kinds(&rules, "x«é,é»y"),
            expect(&[(Text, "x"), (Open, "«"), (Text, "é,é"), (Close, "»"), (Text, "y")])
        );
        // unterminated: the rest of the input is the body, and there is no Close
        assert_eq!(
            kinds(&rules, "a (b, c"),
            expect(&[(Text, "a"), (Delim, " "), (Open, "("), (Text, "b, c")])
        );
        assert_eq!(kinds(&rules, "()"), expect(&[(Open, "("), (Close, ")")]));
        // a closing string outside any enclosure is plain text
        assert_eq!(kinds(&rules, "a) b"), expect(&[(Text, "a)"), (Delim, " "), (Text, "b")]));
        // pairs with an empty string never match
        let broken: [(&str, &str); 2] = [("(", ""), ("", ")")];
        assert_eq!(
            kinds(&ScanRules::new(&[]).enclosures(&broken), "(a)"),
            expect(&[(Text, "(a)")])
        );
    }

    #[rustfmt::skip]
    #[test]
    fn test_rule_precedence() {
        use TokenKind::*;
        // an enclosure opening beats a literal delimiter, which beats a run
        let pairs: [(&str, &str); 1] = [("\"", "\"")];
        let rules: ScanRules<'_> = ScanRules::new(&["\"", " "]).runs("\" ").enclosures(&pairs);
        assert_eq!(
            kinds(&rules, "a\"b c\"  d"),
            expect(&[
                (Text, "a"), (Open, "\""), (Text, "b c"), (Close, "\""),
                (Delim, " "), (Delim, " "), (Text, "d"),
            ])
        );
        // without the enclosure the quote is a literal delimiter ...
        let rules: ScanRules<'_> = ScanRules::new(&["\""]).runs("\"");
        assert_eq!(
            kinds(&rules, "a\"\"b"),
            expect(&[(Text, "a"), (Delim, "\""), (Delim, "\""), (Text, "b")])
        );
        // ... and without the literal it runs
        let rules: ScanRules<'_> = ScanRules::new(&[]).runs("\"");
        assert_eq!(kinds(&rules, "a\"\"b"), expect(&[(Text, "a"), (Run, "\"\""), (Text, "b")]));
    }

    #[test]
    fn test_tokenize_with_literal_rules_matches_tokenize() {
        let plain: Vec<Token> = tokenize(TOKEN_TEST, &TOKEN_DELIMS);
        let ruled: Vec<Token> = tokenize_with(&ScanRules::new(&TOKEN_DELIMS), TOKEN_TEST);
        assert_eq!(plain, ruled);
        assert!(plain
            .iter()
            .all(|t| matches!(t.kind(), TokenKind::Text | TokenKind::Delim)));
    }

    /// Push the pending text as a token, if there is any.
    fn flush(tokens: &mut Vec<Token>, text: &mut String) {
        if !text.is_empty() {
            tokens.push(Token {
                content: mem::take(text),
                ..Default::default()
            });
        }
    }

    /**
    Rule-by-rule reference for `scan`: on chars rather than bytes, with
    no lookup table, re-deriving the contract of `ScanRules` the slow
    way. The oracle for `test_scan_matches_reference`.
    */
    fn reference_scan(rules: &ScanRules<'_>, s: &str) -> Vec<Token> {
        let token = |content: &str, kind: TokenKind, delim_idx: Option<usize>| Token {
            content: content.to_string(),
            kind,
            delim_idx,
        };
        let mut tokens: Vec<Token> = Vec::new();
        let mut text: String = String::new();
        let mut i: usize = 0;

        while i < s.len() {
            let rest: &str = &s[i..];

            let opening = rules
                .enclosures
                .iter()
                .enumerate()
                .find(|(_, (o, c))| !o.is_empty() && !c.is_empty() && rest.starts_with(o));
            if let Some((p, &(open, close))) = opening {
                flush(&mut tokens, &mut text);
                tokens.push(token(open, TokenKind::Open, Some(p)));
                let body: usize = i + open.len();
                let nests: bool = open != close;
                let mut depth: usize = 0;
                let mut j: usize = body;
                let mut found: Option<usize> = None;
                while j < s.len() {
                    let r: &str = &s[j..];
                    if r.starts_with(close) {
                        if depth == 0 {
                            found = Some(j);
                            break;
                        }
                        depth -= 1;
                        j += close.len();
                    } else if nests && r.starts_with(open) {
                        depth += 1;
                        j += open.len();
                    } else {
                        j += r.chars().next().unwrap().len_utf8();
                    }
                }
                match found {
                    Some(c) => {
                        if body < c {
                            tokens.push(token(&s[body..c], TokenKind::Text, None));
                        }
                        tokens.push(token(close, TokenKind::Close, Some(p)));
                        i = c + close.len();
                    }
                    None => {
                        if body < s.len() {
                            tokens.push(token(&s[body..], TokenKind::Text, None));
                        }
                        return tokens;
                    }
                }
                continue;
            }

            let delim = rules
                .delims
                .iter()
                .enumerate()
                .find(|(_, d)| !d.is_empty() && rest.starts_with(*d));
            if let Some((d_idx, d)) = delim {
                flush(&mut tokens, &mut text);
                tokens.push(token(d, TokenKind::Delim, Some(d_idx)));
                i += d.len();
                continue;
            }

            let ch: char = rest.chars().next().unwrap();
            if ch.is_ascii() && rules.runs.contains(ch) {
                flush(&mut tokens, &mut text);
                let n: usize = rest.chars().take_while(|&c| c == ch).count();
                tokens.push(token(&rest[..n], TokenKind::Run, None));
                i += n;
                continue;
            }

            text.push(ch);
            i += ch.len_utf8();
        }

        flush(&mut tokens, &mut text);
        tokens
    }

    #[rustfmt::skip]
    #[test]
    fn test_scan_matches_reference() {
        /*
        The table-driven scanner must agree with the reference on every
        input under every rule set: overlapping literals, literals that
        are also run bytes or enclosure openings, multibyte everything,
        nested and unterminated enclosures, and empty strings in every
        rule. Deterministic LCG so a failure is reproducible from the seed.
        */
        const ALPHABET: [&str; 10] = ["a", "b", "c", "é", " ", "(", ")", "\"", "«", "»"];
        const DELIM_POOL: [&str; 10] = ["a", "b", "ab", "ba", " ", "(", ")", "\"", "é", ""];
        const RUNS_POOL: [&str; 7] = ["", " ", "a", " a", "ab", "(", "é"];
        const ENCLOSURE_POOL: [(&str, &str); 7] = [
            ("(", ")"), ("\"", "\""), ("«", "»"), ("a", "b"), ("ab", "a"), ("(", ""), ("", ")"),
        ];
        let mut seed: u64 = 7;
        let mut next = move |modulus: u64| -> usize {
            seed = seed
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            ((seed >> 33) % modulus) as usize
        };

        for case in 0..5000 {
            let delims: Vec<&str> = (0..next(4)).map(|_| DELIM_POOL[next(10)]).collect();
            let runs: &str = RUNS_POOL[next(7)];
            let enclosures: Vec<(&str, &str)> =
                (0..next(3)).map(|_| ENCLOSURE_POOL[next(7)]).collect();
            let rules: ScanRules<'_> = ScanRules::new(&delims).runs(runs).enclosures(&enclosures);
            let s: String = (0..next(14)).map(|_| ALPHABET[next(10)]).collect();

            let expected: Vec<Token> = reference_scan(&rules, &s);
            let actual: Vec<Token> = tokenize_with(&rules, &s);
            assert_eq!(actual, expected, "case {case}: scanner diverges on {s:?} with {rules:?}");
            let joined: String = actual.iter().map(Token::content).collect();
            assert_eq!(joined, s, "case {case}: tokens must concatenate back to the input");
        }
    }
}

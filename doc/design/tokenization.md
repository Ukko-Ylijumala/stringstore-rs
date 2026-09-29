# Tokenization

The crate ships one tokenizer: a byte-class-table linear scanner, `scan_tokens`. Both public entry points (`tokenize`, `tokenize_regex`) and the store-aware `split_and_store_multi` sit on top of it. This doc covers the contract, the scanner's design, and the empty-delimiter footgun.

## `Token`

```rust
pub struct Token {
    content: String,
    is_delim: bool,           // default: false
    delim_idx: Option<usize>, // default: None — index into the delims slice
}
```

Non-delimiter tokens have `is_delim = false` and `delim_idx = None`. Delimiter tokens carry the position of the matched delimiter in the original `delims: &[&str]` slice, which lets callers map back to "which delimiter matched here."

## Matching contract

**Leftmost-first, earliest delimiter wins.** At the leftmost position where any delimiter matches, the delimiter that appears earliest in the `delims` slice is chosen, and scanning resumes immediately after it. Non-delimiter tokens are never empty; consecutive delimiters produce consecutive delimiter tokens with nothing in between.

For overlapping delimiters this means order matters: `["ab", "abc"]` against `"xabcy"` yields `["x", "ab", "cy"]`. If you want the longest delimiter to win, sort the slice longest-first before calling.

## `scan_tokens` (the scanner core)

```rust
fn scan_tokens<'a, F: FnMut(&'a str, Option<usize>)>(s: &'a str, delims: &[&str], f: F)
```

It calls `f` once per token, in order, with a **slice of the input** and the delimiter position. It allocates nothing itself; what the caller does with the slice is the caller's business. `tokenize` copies each slice into an owned `Token`; `split_and_store_multi` interns the slice directly and never materialises a `Token` at all.

The scan works like this:

1. Build a `ScanTable`: a 256-entry `[ByteClass; 256]` where every byte that starts at least one non-empty delimiter is `DelimStart` and everything else is `Plain`. If no delimiter is usable the whole input is one token (or nothing, for empty input).
2. Walk the input **byte by byte**. A `Plain` byte costs one table load and the cursor advances. Only a `DelimStart` byte runs the delimiter comparison loop (`starts_with` for each delimiter in slice order), so the O(n · m) inner loop of the old tokenizer only runs at candidate positions.
3. On a match, emit the pending non-delimiter slice (if any), emit the delimiter slice, and move both the cursor and the pending-token start past the match.

### Why stepping byte-wise is safe on UTF-8

A delimiter is valid UTF-8, so its first byte is never a continuation byte (`0x80..=0xBF`), and the table never marks a continuation byte as `DelimStart`. A match can therefore only start on a char boundary; because a delimiter consists of whole chars it also ends on one. Every slice the scanner takes is on char boundaries, and the bytes inside a multibyte char that are not match starts are simply skipped.

### Cost

Measured on one machine, release build, `tokenize`-style counting without token allocation, 11 single-char delimiters unless noted:

| input | old linear | old regex | this scanner |
|---|---|---|---|
| 35 B | 1.4 µs | 11.8 µs | 0.03 µs |
| 7 KB | 311 µs | 259 µs | 4.9 µs |
| 4 KB, every other byte a delimiter | 240 µs | 300 µs | 4.0 µs |
| 7 KB, one delimiter | 91 µs | – | 7.4 µs |

An `aho-corasick` automaton was also evaluated (leftmost-first, same semantics): its per-call build cost 6–15 µs, which loses to this scanner at every size above, and only beats the old code on kilobyte inputs. The regex crate's compile alone cost ~12 µs per call. Both were dropped; the crate has no regex dependency any more.

## History: `tokenize_regex` and `force_regex`

Until v0.3.11 there were two implementations: the char-by-char linear `tokenize` and a `tokenize_regex` that compiled `escape(d0) | escape(d1) | ...` per call, with `split_and_store_multi(s, delims, force_regex)` choosing between them by a size heuristic. The two always produced identical output (both were leftmost-first), so nothing observable changed when they were unified:

- `tokenize_regex` is now an inline alias of `tokenize`, kept so the name still resolves.
- `force_regex` is accepted and ignored, kept so the signature is stable.

The original linear loop survives verbatim in the test module as `reference_tokenize`, the oracle for `test_scanner_matches_reference`, which checks the scanner against it on thousands of generated inputs over a tiny alphabet with overlapping, duplicated, empty and multibyte delimiters.

## Extension point

`ByteClass` is deliberately an enum rather than a bool. The structured-text scaffolding (`TextElement`, `StructuredLine`) will need splitting rules a literal alternation cannot express — character-class delimiters ("any whitespace"), repeated-character runs (`Character(_, n)`), and enclosures where inner delimiters must not split (`EnclosedElem`). Each of those is another `ByteClass` variant plus a branch in `scan_tokens`; the entry points and `Token` need not change. Per-token *recognition* (is this an IP address, a date, hex) is a separate step that runs on the emitted slices, not part of the scanner.

If a caller ever needs a pattern-defined delimiter (a regex), the right shape is a new entry point that takes a caller-compiled `Regex`, not per-call compilation.

## Empty delimiter handling

Empty entries in the `delims` slice are skipped without renumbering — `Token.delim_idx` continues to reference the original slice position. `starts_with("")` is always true and would advance the cursor by zero, hanging the loop, so the scanner both leaves empty delimiters out of the `ScanTable` and re-checks `!d.is_empty()` in the comparison loop. If every delimiter is empty (or `delims` is empty), the input is emitted as a single non-delimiter token, or nothing for empty input.

The wrapper `split_and_store_multi` also stores `0` in `delim_indices` for empty delimiters, so the storage side of the API remains consistent.

## Relationship to the store

The scanner is pure — it does not depend on a `UniqueStrStore`. The store-aware wrappers (`split_and_store`, `split_and_store_multi`, `store_path`) intern each emitted part and return a `Vec<u32>` of indices. The encoding of these index vectors (especially the use of `0` as a sentinel for "delimiter at boundary") is documented in `splitting-and-paths.md`.

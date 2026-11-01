# Tokenization

The crate ships one tokenizer: a byte-class-table linear scanner, `scan` in `src/scan.rs`. The public entry points (`tokenize`, `tokenize_regex`, `tokenize_with`) and the store-aware `split_and_store_multi` sit on top of it, and so will the structured-text parser (`structured-text.md`). This doc covers the contract, the three kinds of splitting rule, the scanner's design, and the empty-delimiter footgun.

## `Token`

```rust
pub enum TokenKind { Text, Delim, Run, Open, Close }

pub struct Token {
    content: String,
    kind: TokenKind,          // default: Text
    delim_idx: Option<usize>, // Delim: index into the delims slice; Open/Close: into the enclosures slice
}
```

`is_delim()` is true for anything but `Text`, so code written against the literal-only scanner keeps working: literal delimiters are `Delim` tokens carrying the position of the matched delimiter in the original `delims: &[&str]` slice, which lets callers map back to "which delimiter matched here." The other kinds come from the rules below.

## `ScanRules`: the three kinds of splitting rule

```rust
ScanRules::new(&[",", ";"])                  // literal delimiters
    .runs(" \t")                             // ASCII run characters
    .enclosures(&[("\"", "\""), ("(", ")")]) // (open, close) pairs
```

All three are optional and borrowed, so a rule set costs nothing to build; `tokenize(s, delims)` is `tokenize_with(&ScanRules::new(delims), s)`.

- **Literal delimiters.** The original rule. **Leftmost-first, earliest delimiter wins:** at the leftmost position where any delimiter matches, the delimiter that appears earliest in the `delims` slice is chosen, and scanning resumes immediately after it. Consecutive delimiters produce consecutive `Delim` tokens with nothing in between. For overlapping delimiters this means order matters: `["ab", "abc"]` against `"xabcy"` yields `["x", "ab", "cy"]`. If you want the longest delimiter to win, sort the slice longest-first before calling.
- **Run characters.** ASCII characters each maximal run of which is one `Run` token: `"a   b"` with `runs(" ")` gives `["a", "   ", "b"]`, and the token's length is the repeat count the parser turns into `Character(_, n)`. A run is of one byte, so `" \t "` is three runs. Non-ASCII characters in the string are ignored, because a run byte has to be a whole char for the slices to stay valid UTF-8.
- **Enclosures.** `(open, close)` pairs. From an `open` to its matching `close` nothing splits: the enclosed text is one `Text` token whatever delimiters it contains, between an `Open` and a `Close` token that carry the pair's position in the `enclosures` slice. Distinct pairs nest — `(a (b) c)` closes at the last parenthesis, because an inner `(` needs its own `)` first — while a pair whose two strings are equal, such as quotes, cannot. An **unterminated** enclosure runs to the end of the input: `Open`, then the rest as one `Text`, no `Close`; the tokens still concatenate back to the input. A closing string outside any enclosure is plain text. Escapes (`\"` inside quotes) are not understood.

**Precedence** where several rules apply at one position: an enclosure opening beats a literal delimiter, which beats a run. So with `"` both an enclosure and a literal, it encloses; with a space both a literal and a run character, two spaces are two `Delim` tokens, not one `Run`.

Text tokens are never empty, and in every case the tokens concatenate back to the input byte for byte. `test_scan_matches_reference` checks all of this against a char-by-char reference implementation (`reference_scan`) on thousands of generated rule sets and inputs.

## `scan` (the scanner core)

```rust
pub(crate) enum Scanned<'a> {
    Text(&'a str), Delim(&'a str, usize), Run(&'a str), Open(&'a str, usize), Close(&'a str, usize),
}

pub(crate) fn scan<'a, F: FnMut(Scanned<'a>)>(rules: &ScanRules<'_>, s: &'a str, f: F)
```

It calls `f` once per piece, in order, always with a **slice of the input**. It allocates nothing itself; what the caller does with the slice is the caller's business. `tokenize_with` copies each slice into an owned `Token`; `split_and_store_multi` (through the literal-only wrapper `scan_tokens`) interns each text slice directly, reuses the index it already interned for the matched delimiter (the scanner reports which one), and never materialises a `Token` at all.

The scan works like this:

1. Build a `ScanTable`: a 256-entry table of `ByteClass` bit sets — `DELIM_START` for every byte that starts a non-empty literal, `RUN` for every ASCII run character, `OPEN` for every byte that starts a usable enclosure — and the empty set for everything else. If no rule is usable the whole input is one token (or nothing, for empty input).
2. Walk the input **byte by byte**. A byte in no set costs one table load and the cursor advances. Only a byte in some set runs the matching code for that set, in precedence order (`starts_with` for each enclosure opening, then each literal, in slice order; then the run), so the O(n · m) inner loop only runs at candidate positions.
3. On a match, emit the pending text slice (if any), emit the matched piece — for an enclosure, the `Open`, the body found by searching forward for the matching `close` with depth counting, and the `Close` — and move both the cursor and the pending-text start past it.

### Why stepping byte-wise is safe on UTF-8

Every rule string is valid UTF-8, so its first byte is never a continuation byte (`0x80..=0xBF`), and the table never marks a continuation byte; run characters are restricted to ASCII. A match can therefore only start on a char boundary; because a rule string consists of whole chars it also ends on one. Every slice the scanner takes is on char boundaries, and the bytes inside a multibyte char that are not match starts are simply skipped. The search for a closing string compares at every byte, but a closing string's first byte is not a continuation byte either, so it too can only match on a boundary.

### Cost

Measured on one machine, release build, `tokenize`-style counting without token allocation, 11 single-char delimiters unless noted:

| input | old linear | old regex | this scanner |
|---|---|---|---|
| 35 B | 1.4 µs | 11.8 µs | 0.03 µs |
| 7 KB | 311 µs | 259 µs | 4.9 µs |
| 4 KB, every other byte a delimiter | 240 µs | 300 µs | 4.0 µs |
| 7 KB, one delimiter | 91 µs | – | 7.4 µs |

An `aho-corasick` automaton was also evaluated (leftmost-first, same semantics): its per-call build cost 6–15 µs, which loses to this scanner at every size above, and only beats the old code on kilobyte inputs. The regex crate's compile alone cost ~12 µs per call. Both were dropped; the crate has no regex dependency any more.

The figures predate the run and enclosure rules. Those add nothing to the per-byte fast path (still one table load and a compare against the empty set); the extra work runs only at bytes that are candidates for a rule.

## History: `tokenize_regex` and `force_regex`

Until v0.3.11 there were two implementations: the char-by-char linear `tokenize` and a `tokenize_regex` that compiled `escape(d0) | escape(d1) | ...` per call, with `split_and_store_multi(s, delims, force_regex)` choosing between them by a size heuristic. The two always produced identical output (both were leftmost-first), so nothing observable changed when they were unified:

- `tokenize_regex` is now an inline alias of `tokenize`, kept so the name still resolves.
- `force_regex` is accepted and ignored, kept so the signature is stable.

The original linear loop survives verbatim in the test module as `reference_tokenize`, the oracle for `test_scanner_matches_reference`, which checks the literal-only scanner against it on thousands of generated inputs over a tiny alphabet with overlapping, duplicated, empty and multibyte delimiters. `reference_scan` does the same for the full rule set.

## Extension point

`ByteClass` is a bit set rather than a bool so that one byte can be a candidate for several rules. A new splitting rule is another bit plus a branch in `scan`, in its place in the precedence order; the entry points and `Token` need not change (a new `TokenKind` at most). Per-token *recognition* (is this an IP address, a date, hex) is a separate step that runs on the emitted slices, not part of the scanner; see `structured-text.md`.

If a caller ever needs a pattern-defined delimiter (a regex), the right shape is a new entry point that takes a caller-compiled `Regex`, not per-call compilation.

## Empty delimiter handling

Empty entries in the `delims` slice are skipped without renumbering — `Token.delim_idx` continues to reference the original slice position. `starts_with("")` is always true and would advance the cursor by zero, hanging the loop, so the scanner both leaves empty delimiters out of the `ScanTable` and re-checks `!d.is_empty()` in the comparison loop. The same goes for an enclosure pair with an empty string on either side: it never matches. If no rule is usable (every string empty, or none given), the input is emitted as a single text token, or nothing for empty input.

The wrapper `split_and_store_multi` also stores `0` in `delim_indices` for empty delimiters, so the storage side of the API remains consistent.

## Relationship to the store

The scanner is pure — it does not depend on a `UniqueStrStore`, and lives in its own module, `src/scan.rs`. The store-aware wrappers (`split_and_store`, `split_and_store_multi`, `store_path`) intern each emitted part and return a `Vec<u32>` of indices. The encoding of these index vectors (especially the use of `0` as a sentinel for "delimiter at boundary") is documented in `splitting-and-paths.md`.

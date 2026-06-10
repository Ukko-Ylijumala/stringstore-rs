# Tokenization

The crate ships two tokenizer implementations and a dispatcher that picks between them. They are similar in intent — split a string into a `Vec<Token>` where delimiters appear as their own tokens — but the implementations differ enough that they can disagree on pathological input. This doc covers the contract and the known divergence.

## `Token`

```rust
pub struct Token {
    content: String,
    is_delim: bool,           // default: false
    delim_idx: Option<usize>, // default: None — index into the delims slice
}
```

Non-delimiter tokens have `is_delim = false` and `delim_idx = None`. Delimiter tokens carry the position of the matched delimiter in the original `delims: &[&str]` slice, which lets callers map back to "which delimiter matched here."

## `tokenize` (linear scan)

A hand-written O(n · m) scan: for each byte position, walk the delimiter list and check `s[i..].starts_with(d)`. On a match, flush any accumulated non-delimiter content as a `Token`, push the delimiter `Token`, and advance by the delimiter's length. Otherwise, append one `char` to the in-progress non-delimiter and advance by that char's UTF-8 width (advancing by a single byte would land mid-char on multibyte input and panic on the next slice).

**Matching strategy:** first match wins — the delimiter that appears earliest in the `delims` slice is chosen. This matters for overlapping delimiters (see below).

**Cost profile:** no regex compilation overhead, no allocation per match attempt. Best for short strings or small delimiter sets.

## `tokenize_regex` (alternation regex)

Builds a regex pattern of the form `escape(d0) | escape(d1) | ...`, compiles it with `Regex::new`, and iterates `re.find_iter(s)`. Between matches, the slice of `s` from the previous match end to the current match start becomes a non-delimiter token.

**Matching strategy:** the `regex` crate's leftmost-first semantics. For a given starting byte position, the regex engine tries the alternatives in order and takes the first that matches — same idea as `tokenize`, but applied across the full pattern rather than re-evaluated for each byte. After a match, it advances past the match and continues from there.

**Cost profile:** pays a one-time regex compilation cost, then scans in roughly O(n) thereafter. Best for long strings or large delimiter sets where the linear scan's inner loop dominates.

## The dispatcher

`split_and_store_multi` picks between them:

```rust
let complexity: usize = s.len() * delims.len();
let regex: bool = force_regex.unwrap_or_else(|| complexity > 10000 || delims.len() > 10);
```

The thresholds (`10000` for the product, `10` for the delimiter count) are heuristics — the source carries a `TODO: evaluate thresholds for switching between tokenizers`. They have not been benchmarked. Treat them as starting points, not as a tuned operating point.

Callers can override the choice with `force_regex`:

- `None` — auto-detect.
- `Some(true)` — force the regex tokenizer.
- `Some(false)` — force the linear tokenizer.

## Known divergence on overlapping delimiters

The two tokenizers can produce **different output** when delimiters overlap or share prefixes. The library acknowledges this in the doc comment on `split_and_store_multi`:

> NOTE: the two tokenizers are based on different logic and might yield differing results for the same input, especially if there is any overlap between the provided delimiters. YMMV, buyer beware etc. (WIP)

A concrete shape where this can happen: delimiters `["ab", "abc"]` against input `"xabcy"`. The linear tokenizer matches `"ab"` first (it appears earlier in the slice) and continues from `"cy"`. The regex tokenizer's leftmost-first alternation also takes `"ab"` first, but the engine's behavior around overlapping alternatives in the same start position is implementation-defined enough that callers should not rely on equivalence in these cases. The crate's existing tests deliberately use non-overlapping delimiters to avoid the question.

If you need deterministic behavior with overlapping delimiters: sort the delimiter slice longest-first before calling, and prefer `force_regex = Some(false)` so you can reason about the loop.

## Empty delimiter handling

Both tokenizers skip empty entries in the `delims` slice without renumbering — `Token.delim_idx` continues to reference the original slice position. The reasoning:

- `tokenize`: `s[i..].starts_with("")` is always true and would advance the cursor by zero, hanging the loop. The predicate `!d.is_empty() && s[i..].starts_with(d)` short-circuits the empty case.
- `tokenize_regex`: an empty alternative in the alternation pattern matches zero-width everywhere and pollutes the output. The pattern is built from a filtered copy of `delims`; if every delimiter is empty (or `delims` is empty), the function short-circuits to a single non-delim token containing the entire input (or an empty vector for empty input).

The wrapper `split_and_store_multi` also stores `0` in `delim_indices` for empty delimiters, so the storage side of the API remains consistent.

## Relationship to the store

The tokenizers themselves are pure — they do not depend on a `UniqueStrStore`. The store-aware wrappers (`split_and_store`, `split_and_store_multi`, `store_path`) take the resulting tokens and intern each one, returning a `Vec<u32>` of indices. The encoding of these index vectors (especially the use of `0` as a sentinel for "delimiter at boundary") is documented in `splitting-and-paths.md`.

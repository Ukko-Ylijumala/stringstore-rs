# Splitting, paths, and reconstruction

`split_and_store`, `split_and_store_multi`, `store_path`, and `reconstruct` form a small subsystem: take a structured string apart, intern each part, and later put it back together from the indices. The whole subsystem relies on a single piece of encoding — **index `0` (the empty string) as a structural sentinel**.

## The sentinel-zero encoding

Index `0` of every `UniqueStrStore` is the empty string. The splitting APIs reuse this fixed slot to mark "no content here" without allocating a separate marker. In a returned `Vec<u32>` of part indices, an entry of `0` means one of:

- A delimiter was found at the **start** of the string (entry appears at position 0 of the result).
- A delimiter was found at the **end** of the string (entry appears at the last position).
- **Two or more contiguous delimiters** appeared (one `0` per "missing" part).

For absolute paths via `store_path`, the same convention reads as "the path begins with `/`":

| Input | Output | Reading |
|---|---|---|
| `"foo/bar"` | `[foo_idx, bar_idx]` | relative, two parts |
| `"/foo/bar"` | `[0, foo_idx, bar_idx]` | absolute (leading `/`) |
| `"/"` | `[0, 0]` | root |
| `""` | `[0]` | empty |

`reconstruct` reverses this: for each `0` it emits no content, but **still places a delimiter** between adjacent parts unless it is the last part. The result is that `reconstruct(indices, delim_idx)` round-trips the original string for inputs produced by `split_and_store` (modulo path normalization, see below).

## `split_and_store` vs `split_and_store_multi`

`split_and_store(s, delim)` is the single-delimiter form. It uses `str::split` directly — no tokenizer involved — and returns `(parts: Vec<u32>, delim_idx: u32)`.

`split_and_store_multi(s, delims, force_regex)` is the multi-delimiter form. It uses one of the two tokenizers (see `tokenization.md`) and returns `(parts: Vec<u32>, delim_indices: Vec<u32>)`. **Crucially, the `parts` vector includes the delimiters as tokens, interleaved with the non-delimiter parts** — this is different from the single-delimiter form, where the delimiter appears only in the second tuple element.

This asymmetry exists because the multi-delimiter case cannot be reconstructed by a simple `parts.join(delim)`: different delimiters can appear at different positions. The interleaved encoding preserves which delimiter went where.

If you mix up the two encodings when calling `reconstruct`, the output will be wrong but not detectable — `reconstruct` is single-delimiter only.

## `store_path` and normalization

`store_path(p)` runs the input through `miniutils::normalize_path` before splitting on `/`. The normalization:

- Resolves `.` and `..` segments.
- Collapses runs of `/`.
- Replaces non-Unicode byte sequences with `U+FFFD REPLACEMENT CHARACTER`.
- Strips embedded NUL bytes (`\0`) from segments — the `test_store_path` case `"lokas\0juun"` becomes `"lokasjuun"`.

Because of normalization, **the stored part indices may not reconstruct to the byte-identical input string**. The contract is round-trip through the *normalized* form, not the literal input. Callers that need to preserve the original spelling must keep it separately.

The delimiter is always `/` (the constant `PATH_SEP`). `store_path` does not return the delimiter index — callers retrieve it via `store.idx("/")` if needed for `reconstruct`.

## `reconstruct` error surface

`reconstruct(indices, delim)` validates that every index is in bounds *before* concatenation and returns a structured error:

- `IndexOutOfBounds` — `delim` itself is out of range.
- `InvalidReconstruction { idx, pos, max }` — a part index at position `pos` is out of range.
- `ReconstructionTooLarge` — the input vector has more than `u32::MAX` entries (defensive; not reachable from a single `Vec<u32>` on any real machine).

The function takes the `store` **read lock** for the duration of the rebuild and snapshots `len()` under it, so concurrent inserts cannot make a previously-valid index look out-of-range during reconstruction.

## Edge cases worth knowing

- **Empty input string with `split_and_store`**: returns `([0], delim_idx)`. The delimiter is still interned even though nothing was split.
- **Empty input string with `split_and_store_multi`**: returns `([0], delim_indices)` — delimiters still get interned.
- **Empty `delims` slice in `split_and_store_multi`**: returns `([insert(s)], [])` — the whole string is interned as one part, no splitting.
- **Empty delimiter in `delims`**: stored as index `0` in `delim_indices` and skipped by the tokenizer (see `tokenization.md`).
- **Empty `indices` to `reconstruct`**: returns `Ok("")`, regardless of `delim`.
- **`indices == [0]`**: also returns `Ok("")` — single-element zero-sentinel is treated as the empty input case.

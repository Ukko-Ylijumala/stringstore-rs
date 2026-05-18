# Design notes

Per-feature design documentation for the `stringstore` crate. Each file is self-contained — read whichever covers the area you are working on.

| Doc | Covers |
|---|---|
| [storage-architecture.md](storage-architecture.md) | The three-container layout (`ascii` + `store` + `index`) and the `LATIN1_NUM = 256` offset that hides the seam |
| [concurrency.md](concurrency.md) | Lock ordering, the post-lock recheck in `insert_unchecked`, `validate_contents` semantics |
| [unsafe-pointers.md](unsafe-pointers.md) | `borrow_str`, `StoredStrPtr`, and the append-only invariant that makes them sound |
| [tokenization.md](tokenization.md) | `tokenize` vs `tokenize_regex`, the dispatch heuristic, known divergence cases |
| [splitting-and-paths.md](splitting-and-paths.md) | Sentinel-zero encoding shared by `split_and_store`, `store_path`, and `reconstruct` |

The crate is heavily WIP — these docs describe the design as it stands today, including the rough edges. Where a doc calls out a footgun or TODO, that is a real one.

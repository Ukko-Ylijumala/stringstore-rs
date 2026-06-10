# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Commands

Standard Cargo workflow. The crate is `publish = false` and intended for consumption as a git dependency.

- Build: `cargo build` (or `cargo build --release`)
- Run all tests: `cargo test`
- Run a single test: `cargo test test_concurrent_inserts` (the `CONC_S_NUM = 100_000` constant makes the concurrency tests the slow ones)
- Test with the optional memory-accounting feature: `cargo test --features size_of`
- Lint: `cargo clippy --all-targets`

Tests live inline at the bottom of `src/lib.rs` under `mod tests` — there is no `tests/` directory.

## Code map

Single-file library crate. All public and internal types are in `src/lib.rs`. Rough layout, top to bottom:

| Lines (approx.) | Contents |
|---|---|
| `UniqueStrStore` and its impl | The main interner type and every public method |
| `StoredStrPtr` | Raw-pointer handle to an interned string (unsafe lifetime) |
| `StoredStr<'a>` | Safe lifetime-tracked handle |
| `CompactStr`, `Character`, `TextElement`, `StructuredLine`, `Hex`, `HexFormat`, `Integer` | **Dormant scaffolding** for a structured-text feature; not exported, not tested |
| `tokenize`, `tokenize_regex`, `Token` | Tokenizers used by `split_and_store_multi` |
| `return_iso8859_1_cp` | Utility helper |
| `StringStoreError`, `StringStoreResult` | Error types |
| `mod tests` | Inline test module |

## Design documentation

The design of the non-obvious bits lives in [`doc/design/`](doc/design/README.md). Read the relevant doc before changing behavior in that area — each calls out invariants that the type system does not enforce.

- **[Storage architecture](doc/design/storage-architecture.md)** — the three-container split (`ascii` + `store` + `index`) and the load-bearing `LATIN1_NUM = 256` offset between public and internal indices. Required reading before touching anything index-related.
- **[Concurrency model](doc/design/concurrency.md)** — RwLock + DashMap lock ordering and the post-lock recheck in `insert_unchecked`. Required reading before changing the insert path.
- **[Unsafe pointer surface](doc/design/unsafe-pointers.md)** — `borrow_str`, `StoredStrPtr`, and the append-only invariant that keeps them sound. Required reading before adding any removal/mutation API.
- **[Tokenization](doc/design/tokenization.md)** — two tokenizers, the dispatch heuristic, the empty-delimiter footgun, and the known divergence on overlapping delimiters.
- **[Splitting and paths](doc/design/splitting-and-paths.md)** — sentinel-zero encoding shared by `split_and_store`, `store_path`, and `reconstruct`.

## Dependencies

Four dependencies are git-pinned to forks under `Ukko-Ylijumala`:

- `custom_xxh3` — provides `CustomXxh3Hasher` and `hash_bytes`.
- `timesince` — `SecondsSinceEpoch`, used only by the dormant `TextElement` enum.
- `miniutils` — `normalize_path`, used by `store_path`.
- `size-of` (fork) — replaces the upstream crate to work around Rust 1.89+ compiler error E0570. Only pulled in when the `size_of` feature is enabled.

The `size_of` feature is currently *off* by default (the `default = ["size_of"]` line in `Cargo.toml` is commented out). The `SizeOf for UniqueStrStore` impl is hand-rolled because `size_of` does not natively support `RwLock` or `DashMap`.

## Dormant scaffolding — do not assume final

`TextElement`, `StructuredLine`, `Character`, `CompactStr`, `Hex`, `HexFormat`, and the `Integer` trait compile but are not wired up to any public API (only `Hex`/`HexFormat` formatting has tests). They are an in-progress sketch of a structured-text parsing feature, kept compiling via scoped `#[expect(dead_code)]` attributes on the impl blocks, `TextElement`, and `Integer` — not a crate-wide allow. When you wire one of these up, its `expect` attribute will warn as an unfulfilled lint expectation; remove it then. The type definitions themselves carry no attribute (rustc keeps them transitively live via the annotated items). Treat the shapes of these types as unstable — feel free to rework them when implementing the feature for real.

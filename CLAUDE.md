# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Commands

Standard Cargo workflow. The crate is `publish = false` and intended for consumption as a git dependency.

- Build: `cargo build` (or `cargo build --release`)
- Run all tests: `cargo test`
- Run a single test: `cargo test test_concurrent_inserts` (the `CONC_S_NUM = 100_000` constant makes the concurrency tests the slow ones)
- Test with the optional memory-accounting feature: `cargo test --features size_of`
- Test with 128-bit index keys: `cargo test --features xxh128` (the full matrix is `""`, `xxh128`, `size_of`, `xxh128,size_of`, each with and without `--no-default-features`, which drops the `structured` module; clippy the same way)
- Lint: `cargo clippy --all-targets`
- Unsafe check: `cargo +nightly miri test -- --skip test_concurrent_inserts --skip test_competing_inserts` (needs the nightly `miri` component; slow). Required after touching `StrArena`, `SlotTable`, or the insert or read paths; see `doc/design/unsafe-pointers.md`.

Tests live inline at the bottom of each source file under `mod tests` — there is no `tests/` directory.

## Code map

Three source files. `src/lib.rs` holds the store and everything around it; `src/scan.rs` the pure tokenizer; `src/structured.rs` the structured-text data model, compiled only with the `structured` feature (on by default) and re-exported through `lib.rs`. Rough layout of `lib.rs`, top to bottom:

| File / section | Contents |
|---|---|
| `UniqueStrStore` and its impl | The main interner type and every public method |
| `StoreInner`, `PreHashed`, `StrArena`, `SlotTable` | The shared state behind the `Arc`, the identity hasher for the index, the chunked bump arena holding string bytes (`ARENA_CHUNK_SIZE` default, per-store override via `new_with_capacity`), and the lock-free slot table readers use |
| `StoredStrPtr` | Raw-pointer handle to an interned string (unsafe lifetime) |
| `StoredStr<'a>` | Safe lifetime-tracked handle |
| `return_iso8859_1_cp` | Utility helper |
| `StringStoreError`, `StringStoreResult` | Error types |
| `mod tests` | Inline test module |
| `src/scan.rs`: `ScanRules`, `Scanned`, `ByteClass`, `ScanTable`, `scan`, `scan_tokens`, `tokenize`, `tokenize_with`, `tokenize_regex`, `Token`, `TokenKind` | The byte-class scanner (literal delimiters, run characters, enclosures) and its entry points, used by `split_and_store_multi` (`tokenize_regex` is an alias of `tokenize`) |
| `src/structured.rs`: `CompactStr`, `Character`, `TextElement`, `BigElement`, `StructuredLine`, `Hex`, `HexFormat`, `Recognizer`, `Parser` | The structured-text model and parser (all exported from the crate root): a line becomes a `StructuredLine` of 16-byte `TextElement`s, words interned, numbers / IPs / dates / times / hex as values, lossless. See `doc/design/structured-text.md` |

## Design documentation

The design of the non-obvious bits lives in [`doc/design/`](doc/design/README.md). Read the relevant doc before changing behavior in that area — each calls out invariants that the type system does not enforce.

- **[Storage architecture](doc/design/storage-architecture.md)** — the three-container split (`ascii` + `store` + `index`), the arena that holds the string bytes, and the load-bearing `LATIN1_NUM = 256` offset between public and internal indices. Required reading before touching anything index-related.
- **[Concurrency model](doc/design/concurrency.md)** — lock-free reads over the slot table, the writer mutex + DashMap lock ordering, the post-lock recheck in `insert_unchecked`, and the one in-flight index readers may have to wait for. Required reading before changing the insert or read paths.
- **[Unsafe pointer surface](doc/design/unsafe-pointers.md)** — `borrow_str`, `StoredStrPtr`, and the append-only invariant that keeps them sound. Required reading before adding any removal/mutation API.
- **[Tokenization](doc/design/tokenization.md)** — the byte-class scanner, its three splitting rules (literal delimiters, run characters, enclosures) and their precedence, why byte-wise stepping is UTF-8 safe, the empty-delimiter footgun, and where further rules plug in.
- **[Splitting and paths](doc/design/splitting-and-paths.md)** — sentinel-zero encoding shared by `split_and_store`, `store_path`, and `reconstruct`.
- **[Structured text](doc/design/structured-text.md)** — the data model (16-byte `TextElement` with a `Big(Box<BigElement>)` overflow variant, no `Arc` in lines), the lossless-by-re-rendering rule, the scan → recognize → intern pipeline, the recognizer order and what is still open. Required reading before touching `src/structured.rs`.

## Dependencies

Four dependencies are git-pinned to forks under `Ukko-Ylijumala`:

- `custom_xxh3` — provides `hash_bytes` (xxh3-64 with a fixed custom secret), the default index key. The index map itself uses the crate-local identity hasher `PreHashed`, since its keys are already digests.

`xxhash-rust` (crates.io) is pulled in directly, and only with the `xxh128` feature, for `xxh3_128`; `custom_xxh3` has no 128-bit helper yet.
- `timesince` — `SecondsSinceEpoch`, used only by the `TextElement` enum; optional, pulled in by the `structured` feature.
- `miniutils` — `normalize_path`, used by `store_path`.
- `size-of` (fork) — replaces the upstream crate to work around Rust 1.89+ compiler error E0570. Only pulled in when the `size_of` feature is enabled.

The `xxh128` feature (off by default) switches the index key `HashKey` from `u64` to `u128` and compiles out the duplicate-insert content check (`verify_hit`, `collision_panic`); every hashing site goes through `hash_key`, so the two builds differ only there. Doubles the index map's per-entry footprint, which is why it is opt-in. See `doc/design/storage-architecture.md`.

The `structured` feature (on by default) compiles `src/structured.rs`, the structured-text data model, and with it the `timesince` dependency. `--no-default-features` leaves a plain string store.

The `size_of` feature is currently *off* by default (the `default = ["size_of"]` line in `Cargo.toml` is commented out). The `SizeOf for UniqueStrStore` impl is hand-rolled because `size_of` does not natively support `Mutex` or `DashMap`.

## Structured text — unstable surface

`src/structured.rs` (feature `structured`, default on) is the newest part of the crate and its shapes are not settled: `TextElement`, `BigElement` and `Recognizer` are `#[non_exhaustive]`, and `doc/design/structured-text.md` lists what is still open. The invariants that matter:

- `TextElement` is 16 bytes, enforced by a `const` assertion against `TEXT_ELEMENT_SIZE`. A payload that does not fit goes into `BigElement` behind the `Big` variant, which the constructors (`ip`, `range`, `sentence`, `url`, `url_params`) and accessors (`as_ip`, `as_range`, `as_big`) hide; `Debug` through `with_store` never prints `Big(..)`.
- Nothing in the model holds a store reference; formatting goes through `with_store`, like `StoredStr`.
- `Parser::parse` is lossless: a recognized value is kept only if it renders back to the exact piece it came from (`007`, `+5`, `2.50`, `1e5` stay words), so `line.with_store(&store).to_string()` is the input byte for byte. `test_parse_round_trips` checks this on generated lines; keep it passing when adding a recognizer, and keep the recognizer-order table in the design doc in step with `Recognizer::DEFAULT`.
- `Recognizer::DEFAULT` is the unambiguous set; `Recognizer::ALL` adds the heuristic recognizers (`Url`, `Email`, `Username`, `Hostname`) and the two passes over the parsed line (`Timestamp` merges `Day` + space + `Time`, `Sentence` merges word runs). Only `Range` has no recognizer. Keep the recognizer-order table in the design doc in step with both slices.

# Structured text

**Status: data model done, no parser.** `src/structured.rs` (behind the `structured` feature, on by default) carries the data model (`TextElement`, `BigElement`, `StructuredLine`, `CompactStr`, `Character`, `Hex`, `HexFormat`) reworked from a 2024 sketch to the decisions below — step 1 of the implementation order. It compiles under scoped `#[expect(dead_code)]` and has tests only for its layout and `Display`/`Debug` output; nothing produces a `StructuredLine` from text yet. The shapes are still unstable: the open questions at the end may move variants around.

## Goal

Parse a line of log-like text into a compact sequence of *elements*: words become interned `u32` indices into a `UniqueStrStore`, and high-cardinality tokens — numbers, IP addresses, dates, times, hex — become native values instead of strings. Two things are gained:

1. **Memory.** A parsed line should be smaller than its text. Words repeat across lines and are stored once; numbers cost 8 bytes instead of their digits plus a store entry.
2. **Store hygiene.** The store is append-only (see `unsafe-pointers.md`). A pipeline that interns every token of every line would fill it with request ids, counters and timestamps that never repeat. The recognizers are what keep those out of the store.

The feature is therefore two cooperating halves: the data model (compact elements) and the parser (scanner, recognizers, interning). The store itself does not change.

## Decisions

### 1. Round trip is lossless

`Display` of a parsed line must reproduce the input bytes exactly. The sketch renders a *canonical* form (`KeyVal` always prints `=`, `Hex` drops leading zeros, `Day` has one spelling), which is not good enough for log storage, where the original line is the thing people want back.

The mechanism is **verify by re-rendering**, not recording spellings: after a recognizer fires on a slice, render the value back and compare with the slice. On mismatch, the slice is kept as an interned `Word`. This handles `+42`, `007`, `1e5`, `0x00ff`, odd date spellings and anything else for free, costs one small formatting pass per recognized token, and makes every recognizer safe to add — a wrong guess degrades to a word instead of corrupting output. Recognizers may still make *some* spellings exact (hex keeps its width, see below); the rule is what guarantees the rest.

### 2. An element is 16 bytes

`TextElement` is `#[repr(Rust)]` with a tag and a payload. Two layout facts (measured, stable `rustc`) decide what fits in 16 bytes:

- An **align-8 payload of up to 8 bytes** fits, and so does `(u64, u8)` — the tag goes in the padding after the `u8`.
- An **align-4 payload of up to 12 bytes** fits (`(u32, u32, u32)`), the tag taking the first 4 bytes.
- Anything larger pushes the whole enum to 24, and the sketch today is **32 bytes** (`Vec<CompactStr>`, `String`, `Range(i64, i64, _)`, `IpAddr` at 17 bytes). A 20-element line costs ~650 bytes against ~120 bytes of text, the opposite of the goal.

Everything that does not fit goes behind **one overflow variant**, `Big(Box<BigElement>)`, where `BigElement` is a second enum (32 bytes, boxed, so its size is irrelevant). A `const` assertion pins `size_of::<TextElement>() == TEXT_ELEMENT_SIZE` (16) so a new variant that silently bumps it to 24 fails to compile. The tag has spare values, so `Option<TextElement>` is 16 bytes too.

Inline variants (target):

| Variant | Payload | Bytes |
|---|---|---|
| `Delimiter(Character)`, `Char(Character)`, `Open(Character)`, `Close(Character)` | index + repeat count | 8 |
| `Word(CompactStr)`, `Hostname(CompactStr)`, `RawText(CompactStr)` | index | 4 |
| `KeyVal`, `Username`, `Email` | two indices | 8 |
| `Integer(i64)`, `Float(f64)`, `Timestamp(u64)` | native | 8 |
| `Day(i16, u8, u8)`, `Time(u8, u8, u8)` | native | 4 / 3 |
| `IPv4(Ipv4Addr)` | native | 4 |
| `HexStr(Hex, HexFormat)` | up to 16 digits + format | 9 |
| `Big(Box<Big>)` | pointer | 8 |

Boxed variants (`BigElement`): `IPv6(Ipv6Addr)`, `Range(i64, i64, Character)`, `Sentence(Vec<CompactStr>)`, `URLStr(Vec<CompactStr>)`, `URLParams(Vec<CompactStr>)`. Hex wider than 16 digits (hashes, UUIDs) has no variant yet and will fall back to `Word` under the re-render rule; add a boxed `u128` or interned variant with the hex recognizer if interning them turns out to hurt. Ranges are rare in log text; IPv6 and URLs are not, see "Open questions".

The exact split between inline and boxed is an implementation detail: `TextElement` has constructors (`ip`, `range`, `sentence`, `url`, `url_params`) and accessors (`as_ip`, more as needed) that hide the `Big` variant, and `WithStore<TextElement>`'s `Debug` prints a boxed element as itself (`IPv6(fe80::1)`, never `Big(..)`). Keep `BigElement` out of the public API so variants can move across the boundary without breaking callers.

### 3. Enclosures are brackets, not a tree

The sketch's `EnclosedElem(Box<TextElement>, open, close)` nests. Instead, the scanner emits `Open(Character)` and `Close(Character)` elements into the flat stream and the enclosed content follows as ordinary elements. No box, no recursion in `Display`, and an unterminated enclosure is simply an `Open` with no matching `Close` — the round trip still holds.

### 4. Lines do not carry a store

The sketch's `StructuredLine` holds an `Arc<UniqueStrStore>`. `UniqueStrStore` is already a newtype around an `Arc`, so that is a double indirection plus refcount traffic per line, 8 bytes each over millions of lines. A `StructuredLine` is just the elements (`Vec<TextElement>` or `Box<[TextElement]>`); the caller pairs it with a store for formatting through the `with_store` pattern the sketch already uses for the individual element types, exactly like `StoredStr<'a>(u32, &'a UniqueStrStore)` does for single strings.

### 5. No generic integer type

`TextElement<I: Integer = i64>` is generic in the sketch, but `StructuredLine` fixes `I = i64` and the `Integer` trait exists only to support the parameter. Drop both; `Integer(i64)` is the variant, and a `Unsigned(u64)` variant can be added if values above `i64::MAX` turn out to matter. Fewer monomorphizations, simpler signatures.

### 6. Raw text is interned

`RawText(String)` is an unbounded heap allocation inside the compact structure. It becomes `RawText(CompactStr)`. If a caller needs to keep high-entropy text *out* of the store, that is a parser setting (skip interning above a length or entropy threshold and emit the slice through a different channel), not a variant.

### 7. Hex keeps its width

`HexFormat` is a `u8` bitmap using three bits (`UPPER`, `PREFIX`, `COLUMNS`). The spare five bits hold the digit width (`with_width`, 0 = natural, clamped to 31), which zero-pads the digits so `0x00ff` renders back as written, making most hex round-trip exactly without going through the fallback. Values longer than 16 digits do not fit `Hex(u64)`; see decision 2. The current `COLUMNS` grouping (by four digits from the right) matches neither MAC addresses (by two) nor anything common in logs; revisit or drop it when the recognizer is written.

### 8. Timestamps are opt-in

Any ten-digit integer is a plausible Unix timestamp; nothing in the token says so. `Timestamp` recognition must be enabled explicitly in the parser config, optionally restricted to a context (a field position, an enclosing bracket, a key name). `Day` and `Time` have unambiguous shapes and are always on. A combined `DateTime` recognizer (`2026-11-01T07:05:00Z`, syslog `Nov  1 07:05:00`) producing a `Timestamp` is the natural way to make the variant useful; it belongs in the heuristic phase (step 6 below).

## Pipeline

```text
&str line ──► scanner ──► slices ──► recognizers ──► Recognized<'a> ──► intern ──► StructuredLine
            (pure,          (pure, borrow the         (one insert_many
             no store)       input, no store)          per line)
```

- **Scanner.** `scan_tokens` extended with the `ByteClass` variants planned in `tokenization.md`: a whitespace (or any caller-defined) character class whose runs collapse into one `Character(_, n)` delimiter, and enclosure pairs that suppress splitting between `Open` and `Close`. The entry points `tokenize` / `split_and_store_multi` are unchanged by this.
- **Recognizers.** Pure functions `fn(&str) -> Option<Recognized>` where `Recognized<'a>` is a small enum over borrowed slices and native values (no interning yet). They run in a configured order on each non-delimiter slice, first match wins, then the re-render check of decision 1. Being store-free, they are tested on bare strings.
- **Interning.** One pass maps `Recognized<'a>` to `TextElement` by interning every slice it still holds. Collect the slices and use `insert_many` so the writer mutex is taken once per line (see `concurrency.md`).
- **Config.** A `Parser` built with a builder: delimiters, whitespace class, enclosure pairs, the recognizer set and its order, timestamp policy. `parser.parse(&store, line) -> StructuredLine`.

### Recognizer order and the ambiguities it resolves

| Input | Wanted | Loses to | Rule |
|---|---|---|---|
| `10.0.0.1` | `IPv4` | `Float` | IP before float |
| `2026-11-01` | `Day` | `Range` | date before range |
| `07:05:00` | `Time` | hex with `COLUMNS` | time before hex |
| `deadbeef` | `Word` | `Hex` | hex requires `0x` unless configured otherwise |
| `1730000000` | `Integer` | `Timestamp` | timestamp opt-in (decision 8) |
| `foo=bar` | `KeyVal` | `Word` | key-value before word, but after URL params |

The exact list grows with the recognizers; keep it in this table.

## Implementation order

Each step is independently testable and leaves the crate building with no new dead code.

1. **Data model** (decisions 2–7) — *done.* Slimmed the enum, added `BigElement`, flattened enclosures, dropped the generic and the `Arc`, interned raw text, split IPv4 from IPv6, pinned the size with a `const` assertion, gave `HexFormat` its width. No unsafe, no concurrency.
2. **Scanner extensions**: the whitespace-class delimiter with run counting, then enclosure pairs. Pure code, checked against the `reference_tokenize` oracle in the test module extended with the new rules.
3. **Unambiguous recognizers**: integer, float, prefixed hex, IPv4/IPv6 via `std::net` parsing, `YYYY-MM-DD`, `HH:MM:SS`, `key=value`. Plus the re-render check.
4. **`Parser` and `parse`**: wire the three stages, with a property test that random log-like lines round-trip through `Display` byte for byte.
5. **Heuristic recognizers**: hostname, email, URL, sentence, date-time to `Timestamp`. Each needs a dictionary, a config or context; each lands without touching steps 1–4.
6. **Housekeeping** — *done early, so the parser lands in its final home.* The `structured` cargo feature (default on) gates `src/structured.rs` and makes `timesince` optional (it serves only `Timestamp`); the scanner moved to `src/scan.rs`. `lib.rs` keeps the store.

## Open questions

- **Box vs side table for fat elements.** `Big(Box<Big>)` keeps elements self-contained and renderable from the store alone. The alternative, `Big(u32)` into a per-line `Vec<Big>`, saves one allocation per fat element but adds a Vec header to every line and forces `with_line` instead of `with_store`. The box is chosen; access logs (a URL per line) are the case that could overturn it. If it does, the cheaper fix is to flatten URL and sentence parts into one per-line `Vec<CompactStr>` and store `(start: u32, len: u16)` inline — 6 bytes — which keeps URLs out of the box entirely.
- **Interning IPv6.** Addresses repeat (they are hosts), so `IPv6(CompactStr)` interning the canonical text may beat 16 raw bytes per occurrence and would make the variant inline. Decide with data once the parser exists.
- **Sentence boundaries.** What makes a `Sentence` rather than a run of `Word`s is undefined in the sketch. Probably: a parser setting that merges word runs between configured punctuation. Low priority.

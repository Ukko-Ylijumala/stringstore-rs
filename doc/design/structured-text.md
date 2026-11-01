# Structured text

**Status: parser with the unambiguous recognizers; heuristic recognizers pending.** `src/structured.rs` (behind the `structured` feature, on by default) holds the data model (`TextElement`, `BigElement`, `StructuredLine`, `CompactStr`, `Character`, `Hex`, `HexFormat`), the `Recognizer`s and the `Parser` — steps 1–4 and 6 of the implementation order. Step 5 (hostnames, emails, URLs, sentences, timestamps) is not done: those variants render but nothing produces them from text. The shapes are still unstable (`#[non_exhaustive]`); the open questions at the end may move variants around.

## Goal

Parse a line of log-like text into a compact sequence of *elements*: words become interned `u32` indices into a `UniqueStrStore`, and high-cardinality tokens — numbers, IP addresses, dates, times, hex — become native values instead of strings. Two things are gained:

1. **Memory.** A parsed line should be smaller than its text. Words repeat across lines and are stored once; numbers cost 8 bytes instead of their digits plus a store entry.
2. **Store hygiene.** The store is append-only (see `unsafe-pointers.md`). A pipeline that interns every token of every line would fill it with request ids, counters and timestamps that never repeat. The recognizers are what keep those out of the store.

The feature is therefore two cooperating halves: the data model (compact elements) and the parser (scanner, recognizers, interning). The store itself does not change.

## Decisions

### 1. Round trip is lossless

`Display` of a parsed line must reproduce the input bytes exactly. The 2024 sketch rendered a *canonical* form (`KeyVal` always prints `=`, `Hex` drops leading zeros, `Day` has one spelling), which is not good enough for log storage, where the original line is the thing people want back.

The mechanism is **verify by re-rendering**, not recording spellings: after a recognizer claims a piece, the parser renders the value back into a scratch buffer and compares it with the piece. On mismatch the next recognizer gets a go, and if none fits the piece is interned as a `Word` (or as `RawText` when it is the body of an enclosure). This handles `+42`, `007`, `1e5`, `2.50`, `0X1F`, `fe80:0:0:0:0:0:0:1` and anything else for free, costs one small formatting pass per recognized piece, and makes every recognizer safe to add — a wrong guess degrades to a word instead of corrupting output. Recognizers still make the common spellings exact (hex keeps its width, see below); the rule is what guarantees the rest. Note the corollary: a value's `Display` *is* the set of spellings it can represent, so `100.0` and `2.50` stay words because `f64` prints them as `100` and `2.5`.

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

### 8. Timestamps come from a date and a time, not from an integer

`SecondsSinceEpoch` (from `timesince`) displays as `YYYY-MM-DD HH:MM:SS`, so under the re-render rule a `Timestamp` can never come from a bare integer: `1730000000` renders as a date and would not match. That settles decision 8 the easy way — no opt-in integer heuristic. The way to produce a `Timestamp` is a post-pass that merges `Day`, a single-space `Delimiter` and `Time` into one element (UTC assumed), which is both compact (16 bytes for what was 48) and exact. It is heuristic only in the time-zone assumption, so it belongs to step 5 and is off unless asked for. `Day` and `Time` have unambiguous shapes and are in the default set; ISO `T` / `Z` spellings are not recognized and stay words.

## Pipeline

```text
&str line ──► scan ──► Scanned ──► Recognizer::apply ──► Recognized ──► Pending ──► insert_many ──► StructuredLine
            (pure,                 (pure, borrows the      (re-render       (one writer-mutex
             no store)              piece, no store)        check)           hold per line)
```

- **Scanner.** `scan` with a `ScanRules`: literal delimiters, run characters (a maximal run of one ASCII byte is one piece, which the parser turns into `Character(_, n)` — a `Delimiter` for whitespace, a `Char` otherwise, split above 255 repeats) and enclosure pairs that suppress splitting between `Open` and `Close`. See `tokenization.md`. The entry points `tokenize` / `split_and_store_multi` are unchanged by this.
- **Recognizers.** `Recognizer::apply(&str) -> Option<Recognized>`, pure and store-free, so they are tested on bare strings. `Recognized` is either a `Scalar(TextElement)` — a finished value with no string parts, which then has to pass the re-render check — or a shape whose strings still need interning (`KeyVal(&str, &str)`); the latter are exact by construction. The parser runs them in order on each text piece, enclosure bodies included, and takes the first that claims it *and* renders back.
- **Interning.** Every text piece that is still a string (`Word`, `RawText`, delimiters, brackets, both halves of a `KeyVal`) is collected as a `Pending` and interned with one `insert_many` per line, so the writer mutex is taken once per line at most and single characters resolve to the Latin-1 table without touching it. Then the `TextElement`s are built, consuming the indices in order.
- **Config.** `Parser::new(rules).recognizers(&[..])`; `Parser::default()` is whitespace runs with quotes, brackets and parentheses as enclosures and `Recognizer::DEFAULT`. `parser.parse(&store, line) -> StructuredLine`.

### Recognizer order and the ambiguities it resolves

`Recognizer::DEFAULT` is `[Ip, Day, Time, Integer, Float, Hex, KeyVal]`. Keep this table in step with it.

| Input | Wanted | Would otherwise be | Rule |
|---|---|---|---|
| `10.0.0.1` | `IPv4` | `Float` (fails to parse anyway) | IP first |
| `2026-11-01` | `Day` | a `Range`, once ranges are recognized | date before anything with dashes |
| `07:05:00` | `Time` | hex with `COLUMNS`, if ever recognized | time before anything with colons |
| `42` | `Integer` | `Float(42.0)`, which also renders as `42` | integer before float |
| `0x1f` | `HexStr` | — | hex requires the `0x` prefix; `deadbeef` is a word |
| `1730000000` | `Integer` | `Timestamp` | never (decision 8) |
| `k=v` | `KeyVal` | `Word` | key-value last, and only without whitespace in the piece |
| `len=0xff, t=1` | `RawText` | `KeyVal` | a piece with whitespace (an enclosure body) is not a key-value pair |

Known lossy spellings that stay `Word`, by design: `007`, `+5`, `2.50`, `100.0`, `1e5`, `.5`, `inf`/`NaN` (rejected before parsing, so a word in a log is never a float), `0X1F`, `0xdEaD` (mixed case), hex over 16 digits, `fe80:0:0:0:0:0:0:1` (Rust compresses it), `010.0.0.1` (Rust rejects it), `2026-13-01`, `24:00:00`, `7:05:00`.

## Implementation order

Each step is independently testable and leaves the crate building with no new dead code.

1. **Data model** (decisions 2–7) — *done.* Slimmed the enum, added `BigElement`, flattened enclosures, dropped the generic and the `Arc`, interned raw text, split IPv4 from IPv6, pinned the size with a `const` assertion, gave `HexFormat` its width. No unsafe, no concurrency.
2. **Scanner extensions** — *done.* `ScanRules` with run characters (a run is one `Run` piece, its length the count) and enclosure pairs (nesting for distinct pairs, unterminated runs to the end), `tokenize_with`, `TokenKind`; checked against a char-by-char `reference_scan` oracle. See `tokenization.md`.
3. **Unambiguous recognizers** — *done.* Integer, float, prefixed hex, IPv4/IPv6 via `std::net` parsing, `YYYY-MM-DD`, `HH:MM:SS`, `key=value`, plus the re-render check.
4. **`Parser` and `parse`** — *done.* The three stages wired, with `test_parse_round_trips` parsing 3000 generated log-like lines and checking they come back byte for byte.
5. **Heuristic recognizers**: hostname, email, URL, sentence, and the `Day` + `Time` merge into `Timestamp`. Each needs a dictionary, a config or context, so none is in `Recognizer::DEFAULT`; each lands without touching steps 1–4.
6. **Housekeeping** — *done early, so the parser lands in its final home.* The `structured` cargo feature (default on) gates `src/structured.rs` and makes `timesince` optional (it serves only `Timestamp`); the scanner moved to `src/scan.rs`. `lib.rs` keeps the store.

## Open questions

- **Box vs side table for fat elements.** `Big(Box<Big>)` keeps elements self-contained and renderable from the store alone. The alternative, `Big(u32)` into a per-line `Vec<Big>`, saves one allocation per fat element but adds a Vec header to every line and forces `with_line` instead of `with_store`. The box is chosen; access logs (a URL per line) are the case that could overturn it. If it does, the cheaper fix is to flatten URL and sentence parts into one per-line `Vec<CompactStr>` and store `(start: u32, len: u16)` inline — 6 bytes — which keeps URLs out of the box entirely.
- **Interning IPv6.** Addresses repeat (they are hosts), so `IPv6(CompactStr)` interning the canonical text may beat 16 raw bytes per occurrence and would make the variant inline. Decide with data once the parser exists.
- **Sentence boundaries.** What makes a `Sentence` rather than a run of `Word`s is undefined in the sketch. Probably: a parser setting that merges word runs between configured punctuation. Low priority.

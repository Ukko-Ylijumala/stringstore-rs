// Copyright (c) 2024-2026 Mikko Tanner. All rights reserved.

/*!
Structured text: a line of log-like text as a compact sequence of
elements, with words interned in a [UniqueStrStore] and numbers, IP
addresses, dates and hex as native values.

[Parser] turns a line into a [StructuredLine] in three stages: the
scanner (`src/scan.rs`) splits it into pieces, the [Recognizer]s turn
text pieces into values, and one `insert_many` interns whatever is still
a string. A recognized value is only kept when it renders back to the
exact piece it came from, so `line.with_store(&store).to_string()` is
the input, byte for byte. The design is `doc/design/structured-text.md`.

```
use stringstore::{Parser, ScanRules, TextElement, UniqueStrStore};

let store = UniqueStrStore::new();
let parser = Parser::new(ScanRules::new(&[]).runs(" ").enclosures(&[("[", "]")]));
let line = parser.parse(&store, "[2026-11-01] host=db1 10.0.0.1 took 42");
assert_eq!(line.with_store(&store).to_string(), "[2026-11-01] host=db1 10.0.0.1 took 42");
assert!(matches!(line.elements()[1], TextElement::Day(2026, 11, 1)));
assert!(matches!(line.elements()[10], TextElement::Integer(42)));
```
*/

use crate::{
    scan::{scan, ScanRules, Scanned},
    UniqueStrStore,
};
use std::{
    fmt::{self, Debug, Display, Formatter, Write as FmtWrite},
    net::{IpAddr, Ipv4Addr, Ipv6Addr},
};
use timesince::SecondsSinceEpoch;

/**
Size in bytes of one [TextElement]: a tag plus an 8-byte payload. Every
inline variant is laid out to fit, and a `const` assertion next to the
enum enforces it; a payload that does not fit goes into [BigElement]. See
`doc/design/structured-text.md`.
*/
const TEXT_ELEMENT_SIZE: usize = 16;
/// The scanning rules of [Parser::default]: whitespace runs, and the
/// usual brackets and quotes as enclosures.
const DEFAULT_RULES: ScanRules<'static> =
    ScanRules::new(&[])
        .runs(" \t")
        .enclosures(&[("\"", "\""), ("[", "]"), ("(", ")")]);

/* ############################## DATA MODEL ############################### */

/**
A value from the structured-text model paired with the [UniqueStrStore]
its indices refer to, so that it can be formatted.

`Display` recreates the text; `Debug` prints the index *and* the text
(`CompactStr(256: "foo")`), which is what makes a dumped [StructuredLine]
legible. Obtained through the `with_store` method of [CompactStr],
[Character], [TextElement], [BigElement] and [StructuredLine]; none of
them holds a store reference of its own.
*/
pub struct WithStore<'a, T: ?Sized>(&'a T, &'a UniqueStrStore);

/**
A reference (index) to a stored string slice in a [UniqueStrStore].

This is a compact version which lacks a reference back to the containing store,
hence it is only usable as a part of a larger structure with a reference.
*/
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CompactStr(u32);

impl CompactStr {
    /// Pair with `store` for `Display` / `Debug`; see [WithStore].
    pub fn with_store<'a>(&'a self, store: &'a UniqueStrStore) -> WithStore<'a, Self> {
        WithStore(self, store)
    }

    /// The index in the store.
    #[inline]
    pub fn idx(&self) -> u32 {
        self.0
    }

    /// The string, from the store it was interned in.
    pub fn get<'a>(&self, store: &'a UniqueStrStore) -> &'a str {
        // SAFETY: the index was handed out by this store; the store never
        // removes strings, so it is still in bounds (unsafe-pointers.md).
        // Pairing an index with another store is the caller's error, and
        // `borrow_str` panics on an out-of-bounds index rather than
        // reading past the table.
        unsafe { store.borrow_str(self.0) }
    }

    /// The string, owned.
    pub fn to_string(self, store: &UniqueStrStore) -> String {
        self.get(store).to_string()
    }
}

impl Display for WithStore<'_, CompactStr> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.write_str(self.0.get(self.1))
    }
}

impl Debug for WithStore<'_, CompactStr> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "CompactStr({}: {:?})", self.0.idx(), self.0.get(self.1))
    }
}

/// A list of stored strings, each resolved: `[CompactStr(256: "a"), ..]`.
impl Debug for WithStore<'_, [CompactStr]> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.debug_list()
            .entries(self.0.iter().map(|p| p.with_store(self.1)))
            .finish()
    }
}

/// This is a single or repeated character (or short string) stored in a
/// [UniqueStrStore]: the string and how many times it repeats.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Character(CompactStr, u8);

impl Character {
    /// Pair with `store` for `Display` / `Debug`; see [WithStore].
    pub fn with_store<'a>(&'a self, store: &'a UniqueStrStore) -> WithStore<'a, Self> {
        WithStore(self, store)
    }

    /// The index of the repeated string in the store.
    #[inline]
    pub fn idx(&self) -> u32 {
        self.0 .0
    }

    /// How many times the string repeats.
    #[inline]
    pub fn num(&self) -> u8 {
        self.1
    }

    /// The repeated string (once), from the store it was interned in.
    pub fn get<'a>(&self, store: &'a UniqueStrStore) -> &'a str {
        self.0.get(store)
    }

    /// The full text, repeats included, owned.
    pub fn to_string(self, store: &UniqueStrStore) -> String {
        self.get(store).repeat(self.num() as usize)
    }
}

impl Display for WithStore<'_, Character> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        let s: &str = self.0.get(self.1);
        for _ in 0..self.0.num() {
            f.write_str(s)?;
        }
        Ok(())
    }
}

impl Debug for WithStore<'_, Character> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Character({}: {:?} x{})",
            self.0.idx(),
            self.0.get(self.1),
            self.0.num()
        )
    }
}

/* --------------------------------- */

/**
One element of a [StructuredLine]: a piece of the text as the [Parser]
understood it.

Every inline payload is at most 8 bytes (or 12 at alignment 4), so the
enum is `TEXT_ELEMENT_SIZE` (16) bytes; the assertion below pins that. Any
payload that does not fit lives in [BigElement] behind the single `Big`
variant, which the constructors and accessors in the impl hide. See
`doc/design/structured-text.md`.
*/
#[non_exhaustive]
#[derive(Debug, PartialEq)]
pub enum TextElement {
    /// An element which is explicitly a delimiter, f.ex. space (`" "`):
    /// a literal delimiter of the [ScanRules], or a run of whitespace.
    Delimiter(Character),
    /// A single (or repeated) character, f.ex. `*` or `***`: a run of a
    /// non-whitespace run character.
    Char(Character),
    /// The opening bracket of an enclosure, f.ex. `(` or `"`. The enclosed
    /// elements follow in the stream, up to the matching `Close`.
    Open(Character),
    /// The closing bracket of an enclosure; see `Open`.
    Close(Character),
    /// A piece of text no recognizer claimed.
    Word(CompactStr),
    /// A key-value pair, f.ex. `foo="bar"`.
    KeyVal(CompactStr, CompactStr),
    Integer(i64),
    Float(f64),
    /// A date in the format `YYYY-MM-DD`.
    Day(i16, u8, u8),
    /// A time in the format `HH:MM:SS`.
    Time(u8, u8, u8),
    /// A timestamp as seconds since the Unix epoch.
    Timestamp(SecondsSinceEpoch),
    /// An IPv4 address. IPv6 does not fit inline: [BigElement::IPv6].
    IPv4(Ipv4Addr),
    /// A host name, f.ex. `www.example.org`. Usually a FQDN.
    Hostname(CompactStr),
    /// An username, f.ex. `john@workstation`.
    Username(CompactStr, CompactStr),
    /// An email address, f.ex. `john.doe@example.org`.
    Email(CompactStr, CompactStr),
    /// A hexadecimal number of up to 16 digits, f.ex. `0xdeadbeef` or `feedf00d`.
    HexStr(Hex, HexFormat),
    /// Unprocessed text, interned like a word: the body of an enclosure
    /// that no recognizer claimed.
    RawText(CompactStr),
    /// Every element whose payload does not fit inline.
    Big(Box<BigElement>),
    // Maybe for future...?
    //UuidStr(Uuid),
    //PhoneNumber,
    //GeoCoordinate,
    //Duration,
}

const _: () = assert!(
    size_of::<TextElement>() == TEXT_ELEMENT_SIZE,
    "TextElement outgrew TEXT_ELEMENT_SIZE: move the new payload into BigElement"
);

/**
The payloads too large for an inline [TextElement] variant. Boxed, so its
own size does not matter. Reached through `TextElement::Big`, which the
constructors and accessors on [TextElement] hide, so that a variant can
move across the inline/boxed boundary without touching callers; prefer
those to matching on this enum.
*/
#[non_exhaustive]
#[derive(Debug, PartialEq)]
pub enum BigElement {
    /// An IPv6 address (16 bytes).
    IPv6(Ipv6Addr),
    /// A range of integers with "range" marker, f.ex. `-15..10` or `0-100`.
    Range(i64, i64, Character),
    /// A sentence as a single element, f.ex. `Mary had a little lamb.`.
    Sentence(Vec<CompactStr>),
    /// A URL, f.ex. `https://www.example.org:8080/path/to/file.html`.
    URLStr(Vec<CompactStr>),
    /// URL query params, f.ex. `?foo=bar&baz=qux`. Question mark is implicit.
    URLParams(Vec<CompactStr>),
}

impl TextElement {
    /// Pair with `store` for `Display` / `Debug`; see [WithStore].
    pub fn with_store<'a>(&'a self, store: &'a UniqueStrStore) -> WithStore<'a, Self> {
        WithStore(self, store)
    }

    #[inline]
    fn big(elem: BigElement) -> Self {
        Self::Big(Box::new(elem))
    }

    /// An IP address of either family; only IPv6 is boxed.
    pub fn ip(addr: IpAddr) -> Self {
        match addr {
            IpAddr::V4(v4) => Self::IPv4(v4),
            IpAddr::V6(v6) => Self::big(BigElement::IPv6(v6)),
        }
    }

    /// A range of integers with its "range" marker; see [BigElement::Range].
    pub fn range(from: i64, to: i64, marker: Character) -> Self {
        Self::big(BigElement::Range(from, to, marker))
    }

    /// A sentence of stored words; see [BigElement::Sentence].
    pub fn sentence(words: Vec<CompactStr>) -> Self {
        Self::big(BigElement::Sentence(words))
    }

    /// A URL in stored parts; see [BigElement::URLStr].
    pub fn url(parts: Vec<CompactStr>) -> Self {
        Self::big(BigElement::URLStr(parts))
    }

    /// URL query params; see [BigElement::URLParams].
    pub fn url_params(params: Vec<CompactStr>) -> Self {
        Self::big(BigElement::URLParams(params))
    }

    /// The boxed payload, if this element has one.
    pub fn as_big(&self) -> Option<&BigElement> {
        match self {
            Self::Big(big) => Some(big),
            _ => None,
        }
    }

    /// The IP address, whichever family and wherever it is stored.
    pub fn as_ip(&self) -> Option<IpAddr> {
        match self {
            Self::IPv4(v4) => Some(IpAddr::V4(*v4)),
            _ => match self.as_big()? {
                BigElement::IPv6(v6) => Some(IpAddr::V6(*v6)),
                _ => None,
            },
        }
    }

    /// The range's bounds and marker, if this is a range.
    pub fn as_range(&self) -> Option<(i64, i64, Character)> {
        match self.as_big()? {
            BigElement::Range(from, to, marker) => Some((*from, *to, *marker)),
            _ => None,
        }
    }

    /// Whether this element split the text rather than being part of it:
    /// a `Delimiter`, `Open` or `Close`.
    pub fn is_delim(&self) -> bool {
        matches!(self, Self::Delimiter(_) | Self::Open(_) | Self::Close(_))
    }
}

impl BigElement {
    /// Pair with `store` for `Display` / `Debug`; see [WithStore].
    pub fn with_store<'a>(&'a self, store: &'a UniqueStrStore) -> WithStore<'a, Self> {
        WithStore(self, store)
    }
}

/**
Recreates the element's text. This is the *canonical* rendering of the
value: the `=` of a `KeyVal`, zero-padded date fields, the shortest
float. The [Parser] only keeps a recognized value when this rendering
equals the piece it came from, and interns the piece as a `Word`
otherwise, so a parsed line concatenates back to its input byte for
byte. A hand-built line has no such guarantee.
*/
impl Display for WithStore<'_, TextElement> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        let store: &UniqueStrStore = self.1;
        match self.0 {
            TextElement::Delimiter(c)
            | TextElement::Char(c)
            | TextElement::Open(c)
            | TextElement::Close(c) => write!(f, "{}", c.with_store(store)),
            TextElement::Word(w) | TextElement::Hostname(w) | TextElement::RawText(w) => {
                f.write_str(w.get(store))
            }
            TextElement::KeyVal(k, v) => write!(f, "{}={}", k.get(store), v.get(store)),
            TextElement::Integer(i) => write!(f, "{i}"),
            TextElement::Float(x) => write!(f, "{x}"),
            TextElement::Day(y, m, d) => write!(f, "{y:04}-{m:02}-{d:02}"),
            TextElement::Time(h, m, sec) => write!(f, "{h:02}:{m:02}:{sec:02}"),
            TextElement::Timestamp(t) => write!(f, "{t}"),
            TextElement::IPv4(ip) => write!(f, "{ip}"),
            TextElement::Username(u, host) => write!(f, "{}@{}", u.get(store), host.get(store)),
            TextElement::Email(u, domain) => write!(f, "{}@{}", u.get(store), domain.get(store)),
            TextElement::HexStr(hex, fmt) => f.write_str(&hex.format(*fmt)),
            TextElement::Big(big) => write!(f, "{}", big.with_store(store)),
        }
    }
}

/// The boxed half of [TextElement]'s `Display`; same canonical rendering.
impl Display for WithStore<'_, BigElement> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        let store: &UniqueStrStore = self.1;
        // write a list of stored strings with `sep` between them
        let join = |f: &mut Formatter<'_>, parts: &[CompactStr], sep: &str| -> fmt::Result {
            for (i, part) in parts.iter().enumerate() {
                if i > 0 {
                    f.write_str(sep)?;
                }
                f.write_str(part.get(store))?;
            }
            Ok(())
        };

        match self.0 {
            BigElement::IPv6(ip) => write!(f, "{ip}"),
            BigElement::Range(a, b, marker) => write!(f, "{a}{}{b}", marker.with_store(store)),
            BigElement::Sentence(words) => join(f, words, " "),
            BigElement::URLStr(parts) => join(f, parts, ""),
            BigElement::URLParams(params) => {
                f.write_str("?")?;
                join(f, params, "&")
            }
        }
    }
}

/**
Like the derived `Debug`, but every stored-string field shows its text
next to its index (`Word(CompactStr(256: "foo"))`), and a boxed element
prints as itself (`IPv6(fe80::1)`, not `Big(IPv6(..))`): where a payload
lives is an implementation detail.
*/
impl Debug for WithStore<'_, TextElement> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        let store: &UniqueStrStore = self.1;
        match self.0 {
            TextElement::Delimiter(c) => f
                .debug_tuple("Delimiter")
                .field(&c.with_store(store))
                .finish(),
            TextElement::Char(c) => f.debug_tuple("Char").field(&c.with_store(store)).finish(),
            TextElement::Open(c) => f.debug_tuple("Open").field(&c.with_store(store)).finish(),
            TextElement::Close(c) => f.debug_tuple("Close").field(&c.with_store(store)).finish(),
            TextElement::Word(w) => f.debug_tuple("Word").field(&w.with_store(store)).finish(),
            TextElement::KeyVal(k, v) => f
                .debug_tuple("KeyVal")
                .field(&k.with_store(store))
                .field(&v.with_store(store))
                .finish(),
            TextElement::Integer(i) => f.debug_tuple("Integer").field(i).finish(),
            TextElement::Float(x) => f.debug_tuple("Float").field(x).finish(),
            TextElement::Day(y, m, d) => f.debug_tuple("Day").field(y).field(m).field(d).finish(),
            TextElement::Time(h, m, sec) => {
                f.debug_tuple("Time").field(h).field(m).field(sec).finish()
            }
            TextElement::Timestamp(t) => f
                .debug_tuple("Timestamp")
                .field(&format_args!("{t}"))
                .finish(),
            TextElement::IPv4(ip) => f.debug_tuple("IPv4").field(ip).finish(),
            TextElement::Hostname(h) => f
                .debug_tuple("Hostname")
                .field(&h.with_store(store))
                .finish(),
            TextElement::Username(u, host) => f
                .debug_tuple("Username")
                .field(&u.with_store(store))
                .field(&host.with_store(store))
                .finish(),
            TextElement::Email(u, domain) => f
                .debug_tuple("Email")
                .field(&u.with_store(store))
                .field(&domain.with_store(store))
                .finish(),
            TextElement::HexStr(hex, fmt) => f.debug_tuple("HexStr").field(hex).field(fmt).finish(),
            TextElement::RawText(s) => f
                .debug_tuple("RawText")
                .field(&s.with_store(store))
                .finish(),
            TextElement::Big(big) => Debug::fmt(&big.with_store(store), f),
        }
    }
}

/// The boxed half of [TextElement]'s `Debug`.
impl Debug for WithStore<'_, BigElement> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        let store: &UniqueStrStore = self.1;
        // a Vec<CompactStr> field, resolved: `WithStore(&parts[..], store)`
        match self.0 {
            BigElement::IPv6(ip) => f.debug_tuple("IPv6").field(ip).finish(),
            BigElement::Range(a, b, marker) => f
                .debug_tuple("Range")
                .field(a)
                .field(b)
                .field(&marker.with_store(store))
                .finish(),
            BigElement::Sentence(words) => f
                .debug_tuple("Sentence")
                .field(&WithStore(&words[..], store))
                .finish(),
            BigElement::URLStr(parts) => f
                .debug_tuple("URLStr")
                .field(&WithStore(&parts[..], store))
                .finish(),
            BigElement::URLParams(params) => f
                .debug_tuple("URLParams")
                .field(&WithStore(&params[..], store))
                .finish(),
        }
    }
}

/* --------------------------------- */

/**
A unit of structured text, which can be a line or a block: the elements,
and nothing else. It holds no store reference (that would cost a pointer
per line over millions of lines); pair it with the store through
`with_store` for formatting, the way [StoredStr](crate::StoredStr) pairs an
index with its store. The derived `Debug` shows raw indices; `with_store`
resolves them.
*/
#[derive(Debug, Default, PartialEq)]
pub struct StructuredLine {
    elems: Vec<TextElement>,
}

impl StructuredLine {
    /// An empty line; [Parser::parse] makes the useful ones.
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of elements.
    pub fn len(&self) -> usize {
        self.elems.len()
    }

    /// Whether there are no elements.
    pub fn is_empty(&self) -> bool {
        self.elems.is_empty()
    }

    /// Append an element.
    pub fn push(&mut self, elem: TextElement) {
        self.elems.push(elem);
    }

    /// The elements, in text order.
    pub fn elements(&self) -> &[TextElement] {
        &self.elems
    }

    /// Pair with `store` for `Display` / `Debug`; see [WithStore].
    pub fn with_store<'a>(&'a self, store: &'a UniqueStrStore) -> WithStore<'a, Self> {
        WithStore(self, store)
    }
}

/// Recreates the line by concatenating its elements (canonical rendering,
/// see [TextElement]'s `Display`; byte-exact for a parsed line).
impl Display for WithStore<'_, StructuredLine> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        for elem in &self.0.elems {
            write!(f, "{}", elem.with_store(self.1))?;
        }
        Ok(())
    }
}

/// Lists the elements with their strings resolved; the store itself is
/// not printed (it may hold millions of strings).
impl Debug for WithStore<'_, StructuredLine> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.write_str("StructuredLine ")?;
        f.debug_list()
            .entries(self.0.elems.iter().map(|e| e.with_store(self.1)))
            .finish()
    }
}

/* --------------------------------- */

/// A representation of a hexadecimal number.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Hex(u64);

impl Hex {
    /// The value.
    pub fn get(&self) -> u64 {
        self.0
    }

    /// The hex string in the given format.
    pub fn format(self, fmt: HexFormat) -> String {
        /*
        Order matters: the width pads first so that the column grouping
        counts the leading zeros, and the "0x" prefix is applied last so
        that it is neither uppercased ("0X...") nor counted into the
        grouping.
        */
        let mut result: String = format!("{:0w$x}", self.0, w = fmt.width() as usize);
        if fmt.is_upper() {
            result = result.to_uppercase();
        }
        if fmt.is_columns() {
            // group by 4 digits counting from the *right*, like a numeric
            // separator: 0xfeedf00d5 -> "f:eedf:00d5"
            let n: usize = result.len();
            result = result
                .chars()
                .enumerate()
                .map(|(i, c)| {
                    if i != 0 && (n - i).is_multiple_of(4) {
                        format!(":{c}")
                    } else {
                        c.to_string()
                    }
                })
                .collect();
        }
        if fmt.is_prefix() {
            result = format!("0x{}", result);
        }
        result
    }
}

impl Debug for Hex {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "Hex({:x})", self.0)
    }
}

impl Display for Hex {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "{:x}", self.0)
    }
}

/* --------------------------------- */

/**
Options for hex string display: the low `FLAG_BITS` (three) bits are
the flags below, the high five the digit width (`with_width`), which
zero-pads the digits so that a parsed `0x00ff` renders back as written.
Width 0 means the natural width.
*/
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct HexFormat(u8);

impl HexFormat {
    pub const PLAIN: Self = Self(0b0);
    pub const UPPER: Self = Self(0b1);
    pub const PREFIX: Self = Self(0b10);
    pub const COLUMNS: Self = Self(0b100);
    /// Number of flag bits; the width occupies the rest of the byte.
    const FLAG_BITS: u8 = 3;
    const FLAG_MASK: u8 = (1 << Self::FLAG_BITS) - 1;
    /// The widest width the five bits hold; also the widest `u64` needs.
    pub const MAX_WIDTH: u8 = u8::MAX >> Self::FLAG_BITS;

    /// Whether the hex string is uppercase.
    pub fn is_upper(&self) -> bool {
        self.0 & Self::UPPER.0 != 0
    }
    /// Whether the "0x" prefix should be shown.
    pub fn is_prefix(&self) -> bool {
        self.0 & Self::PREFIX.0 != 0
    }
    /// Whether the parts are divided by columns (":").
    pub fn is_columns(&self) -> bool {
        self.0 & Self::COLUMNS.0 != 0
    }
    /// The fixed digit width (zero-padded), or 0 for the natural width.
    pub fn width(&self) -> u8 {
        self.0 >> Self::FLAG_BITS
    }
    /// The same flags with a fixed digit width, clamped to [Self::MAX_WIDTH].
    pub fn with_width(self, width: u8) -> Self {
        Self((self.0 & Self::FLAG_MASK) | (width.min(Self::MAX_WIDTH) << Self::FLAG_BITS))
    }
    /// Both sets of flags, keeping `self`'s width.
    pub fn with(self, other: Self) -> Self {
        Self(self.0 | (other.0 & Self::FLAG_MASK))
    }
}

#[rustfmt::skip]
impl Display for HexFormat {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        if *self == Self::PLAIN {
            return write!(f, "Plain");
        }

        let mut parts: Vec<String> = Vec::new();
        if self.is_upper() { parts.push("Upper".to_string()); }
        if self.is_prefix() { parts.push("Prefix".to_string()); }
        if self.is_columns() { parts.push("Columns".to_string()); }
        if self.width() > 0 { parts.push(format!("Width({})", self.width())); }
        write!(f, "{}", parts.join("|"))
    }
}

impl Debug for HexFormat {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "HexFmt({}: {})", self.0, self)
    }
}

/* ############################## RECOGNIZERS ############################## */

/**
A rule that turns a piece of text into a value. The [Parser] runs its
recognizers in order on every text piece and takes the first that
claims it *and* renders back to the exact piece (see [Parser]); the
order therefore settles the overlaps, which is what [Recognizer::DEFAULT]
does for the unambiguous set.
*/
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Recognizer {
    /// An IPv4 or IPv6 address, as `std::net` parses it: `10.0.0.1`, `fe80::1`.
    Ip,
    /// `YYYY-MM-DD`, with the month in 1–12 and the day in 1–31 (no calendar).
    Day,
    /// `HH:MM:SS`, 24-hour.
    Time,
    /// A decimal `i64`, as Rust parses it; `+5`, `007` and `-0` stay words.
    Integer,
    /// A decimal `f64` starting with a digit, `-` or `.`; only spellings
    /// Rust prints back the same way (`3.14`, not `2.50` or `1e5`).
    Float,
    /// `0x` followed by 1–16 hex digits, all lowercase or all uppercase.
    Hex,
    /// `key=value`: a non-empty key up to the first `=`, the rest the
    /// value (which may be empty); no whitespace on either side, so the
    /// body of an enclosure with spaces in it stays raw text.
    KeyVal,
}

impl Recognizer {
    /// The unambiguous recognizers, in the order that resolves their
    /// overlaps: an IP before a float, a date before anything with
    /// dashes, a time before anything with colons, integers before
    /// floats and hex, key-value pairs last.
    pub const DEFAULT: &'static [Recognizer] = &[
        Recognizer::Ip,
        Recognizer::Day,
        Recognizer::Time,
        Recognizer::Integer,
        Recognizer::Float,
        Recognizer::Hex,
        Recognizer::KeyVal,
    ];

    /// Run this recognizer on `text`; `None` if it does not claim it.
    fn apply<'a>(self, text: &'a str) -> Option<Recognized<'a>> {
        let scalar = |elem: TextElement| Some(Recognized::Scalar(elem));
        match self {
            Self::Ip => scalar(TextElement::ip(text.parse().ok()?)),
            Self::Day => {
                let b: &[u8] = text.as_bytes();
                if b.len() != 10 || b[4] != b'-' || b[7] != b'-' {
                    return None;
                }
                let (y, m, d) = (digits(&b[0..4])?, digits(&b[5..7])?, digits(&b[8..10])?);
                if !(1..=12).contains(&m) || !(1..=31).contains(&d) {
                    return None;
                }
                scalar(TextElement::Day(y as i16, m as u8, d as u8))
            }
            Self::Time => {
                let b: &[u8] = text.as_bytes();
                if b.len() != 8 || b[2] != b':' || b[5] != b':' {
                    return None;
                }
                let (h, m, s) = (digits(&b[0..2])?, digits(&b[3..5])?, digits(&b[6..8])?);
                if h > 23 || m > 59 || s > 59 {
                    return None;
                }
                scalar(TextElement::Time(h as u8, m as u8, s as u8))
            }
            Self::Integer => scalar(TextElement::Integer(text.parse().ok()?)),
            Self::Float => {
                // not "inf", "NaN" or "infinity", which Rust would parse
                let first: u8 = *text.as_bytes().first()?;
                if !(first.is_ascii_digit() || first == b'-' || first == b'.') {
                    return None;
                }
                scalar(TextElement::Float(text.parse().ok()?))
            }
            Self::Hex => {
                let hex: &str = text.strip_prefix("0x")?;
                let valid: bool = !hex.is_empty() && hex.len() <= 16;
                if !valid || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
                    return None;
                }
                let upper: bool = hex.bytes().any(|b| b.is_ascii_uppercase());
                if upper && hex.bytes().any(|b| b.is_ascii_lowercase()) {
                    return None; // mixed case would not render back
                }
                let flags: HexFormat = if upper {
                    HexFormat::UPPER
                } else {
                    HexFormat::PLAIN
                };
                let fmt: HexFormat = HexFormat::PREFIX.with(flags).with_width(hex.len() as u8);
                scalar(TextElement::HexStr(
                    Hex(u64::from_str_radix(hex, 16).ok()?),
                    fmt,
                ))
            }
            Self::KeyVal => {
                let (key, value) = text.split_once('=')?;
                let plain: bool = !key.is_empty() && !text.bytes().any(|b| b.is_ascii_whitespace());
                plain.then_some(Recognized::KeyVal(key, value))
            }
        }
    }
}

/// The value of an all-digit byte string, `None` if any byte is not a digit.
fn digits(b: &[u8]) -> Option<u32> {
    b.iter().try_fold(0u32, |acc, &c| {
        c.is_ascii_digit().then(|| acc * 10 + u32::from(c - b'0'))
    })
}

/// What a [Recognizer] made of a text piece: either a finished value with
/// no string parts, or a shape whose strings still have to be interned.
enum Recognized<'a> {
    /// A value with no string parts; final, once it passes the re-render check.
    Scalar(TextElement),
    /// `key=value`, both still to be interned.
    KeyVal(&'a str, &'a str),
}

/* ################################ PARSER ################################# */

/**
Turns a line of text into a [StructuredLine]: the scanner splits it by
the [ScanRules], every text piece goes through the [Recognizer]s in
order, and what is still a string is interned in one `insert_many`
(one writer-mutex hold per line at most).

**Lossless.** A recognized value is kept only when rendering it back
gives the exact piece it came from; otherwise the next recognizer gets a
go, and if none fits the piece is interned as a `Word` (or `RawText`,
for the body of an enclosure). `007`, `+5`, `2.50`, `1e5` and `0xDeAd`
therefore stay words, and `line.with_store(&store).to_string()` is
always the input, byte for byte.

**Runs and enclosures.** A run of a whitespace run character becomes a
`Delimiter(Character(_, n))`, any other run a `Char`; a run longer than
255 is split. An enclosure's opening and closing strings become `Open`
and `Close`, and its body goes through the recognizers like any text.
*/
#[derive(Debug, Clone)]
pub struct Parser<'r> {
    rules: ScanRules<'r>,
    recognizers: &'r [Recognizer],
}

impl<'r> Parser<'r> {
    /// A parser splitting by `rules`, with the [Recognizer::DEFAULT] set.
    pub const fn new(rules: ScanRules<'r>) -> Self {
        Self {
            rules,
            recognizers: Recognizer::DEFAULT,
        }
    }

    /// The recognizers to run, in this order; an empty slice interns
    /// every text piece as a word.
    pub const fn recognizers(self, recognizers: &'r [Recognizer]) -> Self {
        Self {
            recognizers,
            ..self
        }
    }

    /// The scanning rules.
    pub fn rules(&self) -> &ScanRules<'r> {
        &self.rules
    }

    /// Parse one line, interning its strings in `store`.
    pub fn parse(&self, store: &UniqueStrStore, line: &str) -> StructuredLine {
        let mut pending: Vec<Pending<'_>> = Vec::new();
        let mut buf: String = String::new(); // re-render scratch
        let mut enclosed: bool = false; // the next text piece is an enclosure body
        scan(&self.rules, line, |piece: Scanned<'_>| match piece {
            Scanned::Text(text) => {
                pending.push(self.recognize(store, text, enclosed, &mut buf));
                enclosed = false;
            }
            Scanned::Delim(d, _) => pending.push(Pending::Delimiter(d, 1)),
            Scanned::Run(run) => Self::push_run(&mut pending, run),
            Scanned::Open(open, _) => {
                pending.push(Pending::Open(open));
                enclosed = true;
            }
            Scanned::Close(close, _) => {
                pending.push(Pending::Close(close));
                enclosed = false;
            }
        });

        // intern every string of the line in one go, then build the elements
        let mut strs: Vec<&str> = Vec::with_capacity(pending.len());
        for p in &pending {
            p.strs(&mut strs);
        }
        let indices: Vec<u32> = store.insert_many(&strs);
        let mut next = indices.into_iter().map(CompactStr);
        let elems: Vec<TextElement> = pending
            .into_iter()
            .map(|p| p.into_element(&mut next))
            .collect();
        debug_assert!(next.next().is_none(), "every interned string must be used");
        StructuredLine { elems }
    }

    /// Run the recognizers on a text piece, with the re-render check for
    /// scalars; a piece nobody claims is a `Word`, or `RawText` inside an
    /// enclosure.
    fn recognize<'a>(
        &self,
        store: &UniqueStrStore,
        text: &'a str,
        enclosed: bool,
        buf: &mut String,
    ) -> Pending<'a> {
        for recognizer in self.recognizers {
            match recognizer.apply(text) {
                None => continue,
                Some(Recognized::KeyVal(k, v)) => return Pending::KeyVal(k, v),
                Some(Recognized::Scalar(elem)) => {
                    // a scalar reads nothing from the store; it just needs the type
                    buf.clear();
                    write!(buf, "{}", elem.with_store(store)).expect("String never fails");
                    if buf == text {
                        return Pending::Done(elem);
                    }
                    // lossy spelling: let the next recognizer try, else a word
                }
            }
        }
        if enclosed {
            Pending::RawText(text)
        } else {
            Pending::Word(text)
        }
    }

    /// A run of one byte as `Character`s of at most 255 repeats each:
    /// delimiters for whitespace, plain characters otherwise.
    fn push_run<'a>(pending: &mut Vec<Pending<'a>>, run: &'a str) {
        let byte: &'a str = &run[..1];
        let whitespace: bool = run.as_bytes()[0].is_ascii_whitespace();
        let mut left: usize = run.len();
        while left > 0 {
            let n: u8 = left.min(u8::MAX as usize) as u8;
            pending.push(if whitespace {
                Pending::Delimiter(byte, n)
            } else {
                Pending::Char(byte, n)
            });
            left -= n as usize;
        }
    }
}

/// `Parser::new(DEFAULT_RULES)`: whitespace runs, and the usual quotes
/// and brackets as enclosures.
impl Default for Parser<'static> {
    fn default() -> Self {
        Self::new(DEFAULT_RULES)
    }
}

/**
An element of a line being parsed, before its strings are interned.
`strs` lists the strings in the order `into_element` consumes their
indices; keep the two in step.
*/
enum Pending<'a> {
    Word(&'a str),
    RawText(&'a str),
    /// The delimiter text and its repeat count.
    Delimiter(&'a str, u8),
    Char(&'a str, u8),
    Open(&'a str),
    Close(&'a str),
    KeyVal(&'a str, &'a str),
    /// A finished scalar element.
    Done(TextElement),
}

impl<'a> Pending<'a> {
    /// Push the strings this element needs interned, in order.
    fn strs(&self, out: &mut Vec<&'a str>) {
        match self {
            Self::Word(s)
            | Self::RawText(s)
            | Self::Delimiter(s, _)
            | Self::Char(s, _)
            | Self::Open(s)
            | Self::Close(s) => out.push(s),
            Self::KeyVal(k, v) => out.extend([k, v]),
            Self::Done(_) => {}
        }
    }

    /// Build the element, taking its interned indices from `idx` in the
    /// order `strs` listed the strings.
    fn into_element(self, idx: &mut impl Iterator<Item = CompactStr>) -> TextElement {
        let mut next = || idx.next().expect("one index per string listed by `strs`");
        match self {
            Self::Word(_) => TextElement::Word(next()),
            Self::RawText(_) => TextElement::RawText(next()),
            Self::Delimiter(_, n) => TextElement::Delimiter(Character(next(), n)),
            Self::Char(_, n) => TextElement::Char(Character(next(), n)),
            Self::Open(_) => TextElement::Open(Character(next(), 1)),
            Self::Close(_) => TextElement::Close(Character(next(), 1)),
            Self::KeyVal(_, _) => TextElement::KeyVal(next(), next()),
            Self::Done(elem) => elem,
        }
    }
}

/* ################################ TESTS ################################## */

#[cfg(test)]
mod tests {
    use super::*;

    /// The line's elements as their legible `Debug` strings.
    fn dbg(line: &StructuredLine, store: &UniqueStrStore) -> Vec<String> {
        line.elements()
            .iter()
            .map(|e| format!("{:?}", e.with_store(store)))
            .collect()
    }

    /// Parse `text` as a whole piece (no splitting) and return the one element.
    fn one(store: &UniqueStrStore, text: &str) -> TextElement {
        let mut line: StructuredLine = Parser::new(ScanRules::new(&[])).parse(store, text);
        assert_eq!(
            line.len(),
            1,
            "{text:?} must be one element: {:?}",
            dbg(&line, store)
        );
        assert_eq!(
            line.with_store(store).to_string(),
            text,
            "round trip of {text:?}"
        );
        line.elems.pop().unwrap()
    }

    #[rustfmt::skip]
    #[test]
    fn test_structured_line_display_and_debug() {
        let store: UniqueStrStore = UniqueStrStore::new();
        let word = |s: &str| CompactStr(store.insert(s));
        let ch = |s: &str, n: u8| Character(CompactStr(store.insert(s)), n);
        let mut line: StructuredLine = StructuredLine::new();
        assert!(line.is_empty());

        line.push(TextElement::Word(word("Mary")));
        line.push(TextElement::Delimiter(ch(" ", 1)));
        line.push(TextElement::KeyVal(word("had"), word("lamb")));
        line.push(TextElement::Delimiter(ch(" ", 2)));
        line.push(TextElement::Integer(-42));
        line.push(TextElement::Char(ch("*", 3)));
        line.push(TextElement::range(0, 100, ch("-", 1)));
        line.push(TextElement::Day(2026, 9, 29));
        line.push(TextElement::Time(7, 5, 0));
        line.push(TextElement::ip("10.0.0.1".parse().unwrap()));
        line.push(TextElement::ip("fe80::1".parse().unwrap()));
        line.push(TextElement::Email(word("john"), word("example.org")));
        line.push(TextElement::HexStr(Hex(0xfeedf00d), HexFormat::PREFIX));
        line.push(TextElement::sentence(vec![word("a"), word("little")]));
        line.push(TextElement::url(vec![word("https://"), word("example.org")]));
        line.push(TextElement::url_params(vec![word("x=1"), word("y=2")]));
        line.push(TextElement::Open(ch("(", 1)));
        line.push(TextElement::Word(word("inner")));
        line.push(TextElement::Close(ch(")", 1)));
        line.push(TextElement::RawText(word("raw")));

        assert_eq!(
            line.with_store(&store).to_string(),
            "Mary had=lamb  -42***0-1002026-09-2907:05:0010.0.0.1fe80::1john@example.org0xfeedf00d\
             a littlehttps://example.org?x=1&y=2(inner)raw"
        );
        assert_eq!(line.len(), 20);
        assert!(!line.is_empty());

        // the boxed payloads are reached through the accessors
        let elems: &[TextElement] = line.elements();
        assert_eq!(elems[9].as_ip(), Some("10.0.0.1".parse().unwrap()));
        assert_eq!(elems[10].as_ip(), Some("fe80::1".parse().unwrap()));
        assert_eq!(elems[0].as_ip(), None);
        assert_eq!(elems[6].as_range(), Some((0, 100, ch("-", 1))));
        assert_eq!(elems[4].as_range(), None);
        assert!(matches!(elems[9], TextElement::IPv4(_)));
        assert!(matches!(elems[10], TextElement::Big(_)));
        assert!(matches!(elems[13].as_big(), Some(BigElement::Sentence(words)) if words.len() == 2));
        assert!(elems[1].is_delim() && elems[16].is_delim() && elems[18].is_delim());
        assert!(!elems[0].is_delim() && !elems[5].is_delim());

        let dbg: String = format!("{:?}", line.with_store(&store));
        let mary: u32 = store.idx("Mary").unwrap();
        assert!(dbg.starts_with("StructuredLine ["), "{dbg}");
        assert!(dbg.contains(&format!("Word(CompactStr({mary}: \"Mary\"))")), "{dbg}");
        assert!(dbg.contains("Delimiter(Character(32: \" \" x2))"), "{dbg}");
        assert!(dbg.contains("Integer(-42)"), "{dbg}");
        assert!(dbg.contains("IPv4(10.0.0.1)"), "{dbg}");
        assert!(dbg.contains("IPv6(fe80::1)"), "{dbg}");
        assert!(dbg.contains("Open(Character(40: \"(\" x1))"), "{dbg}");
        assert!(dbg.contains("Sentence([CompactStr("), "{dbg}");
        assert!(!dbg.contains("Big("), "boxing is an implementation detail: {dbg}");
        assert!(!dbg.contains("UniqueStrStore"), "store must not be dumped: {dbg}");
    }

    #[test]
    fn test_text_element_is_compact() {
        assert_eq!(size_of::<TextElement>(), TEXT_ELEMENT_SIZE);
        assert_eq!(size_of::<Box<BigElement>>(), size_of::<usize>());
        // the tag has spare values, so an Option costs nothing extra
        assert_eq!(size_of::<Option<TextElement>>(), TEXT_ELEMENT_SIZE);
    }

    #[test]
    fn test_hex_format() {
        let h: Hex = Hex(0xdeadbeef);
        assert_eq!(h.format(HexFormat::PLAIN), "deadbeef");
        assert_eq!(h.format(HexFormat::UPPER), "DEADBEEF");
        assert_eq!(h.format(HexFormat::PREFIX), "0xdeadbeef");
        assert_eq!(h.format(HexFormat::COLUMNS), "dead:beef");
        assert_eq!(h.to_string(), "deadbeef", "Display is the plain form");

        // groups count from the right when the digit count isn't a multiple of 4
        assert_eq!(Hex(0xfeedf00d5).format(HexFormat::COLUMNS), "f:eedf:00d5");
        assert_eq!(Hex(0x5).format(HexFormat::COLUMNS), "5");

        // the "0x" prefix is applied last: neither uppercased nor grouped
        let all: HexFormat = HexFormat::UPPER
            .with(HexFormat::PREFIX)
            .with(HexFormat::COLUMNS);
        assert_eq!(h.format(all), "0xDEAD:BEEF");
        assert_eq!(all.to_string(), "Upper|Prefix|Columns");
        assert_eq!(HexFormat::PLAIN.to_string(), "Plain");

        // a fixed width restores leading zeros; the columns group the padded digits
        assert_eq!(Hex(0xff).format(HexFormat::PREFIX.with_width(4)), "0x00ff");
        assert_eq!(
            Hex(0xff).format(HexFormat::COLUMNS.with_width(8)),
            "0000:00ff"
        );
        assert_eq!(
            Hex(0xdeadbeef).format(HexFormat::PLAIN.with_width(2)),
            "deadbeef"
        );
        let wide: HexFormat = HexFormat::UPPER.with_width(4);
        assert_eq!(wide.width(), 4);
        assert!(wide.is_upper() && !wide.is_prefix());
        assert_eq!(wide.to_string(), "Upper|Width(4)");
        assert_eq!(HexFormat::PLAIN.with_width(4).to_string(), "Width(4)");
        assert_eq!(HexFormat::PLAIN.with_width(1), HexFormat(0b1000));
        assert_eq!(
            HexFormat::PLAIN.with_width(u8::MAX).width(),
            HexFormat::MAX_WIDTH
        );
        assert_eq!(wide.with_width(0), HexFormat::UPPER);
        // `with` adds flags and keeps the width
        assert_eq!(
            wide.with(HexFormat::PREFIX.with_width(9)),
            HexFormat::UPPER.with(HexFormat::PREFIX).with_width(4)
        );
    }

    #[rustfmt::skip]
    #[test]
    fn test_recognizers() {
        let store: UniqueStrStore = UniqueStrStore::new();
        let kind = |text: &str| format!("{:?}", one(&store, text).with_store(&store));

        // the unambiguous set
        assert_eq!(kind("10.0.0.1"), "IPv4(10.0.0.1)");
        assert_eq!(kind("fe80::1"), "IPv6(fe80::1)");
        assert_eq!(kind("::1"), "IPv6(::1)");
        assert_eq!(kind("2026-11-01"), "Day(2026, 11, 1)");
        assert_eq!(kind("0001-01-01"), "Day(1, 1, 1)");
        assert_eq!(kind("07:05:00"), "Time(7, 5, 0)");
        assert_eq!(kind("23:59:59"), "Time(23, 59, 59)");
        assert_eq!(kind("42"), "Integer(42)");
        assert_eq!(kind("-42"), "Integer(-42)");
        assert_eq!(kind("0"), "Integer(0)");
        assert_eq!(kind("9223372036854775807"), "Integer(9223372036854775807)");
        assert_eq!(kind("3.14"), "Float(3.14)");
        assert_eq!(kind("-0.5"), "Float(-0.5)");
        assert_eq!(kind("-0"), "Float(-0.0)", "Rust prints -0.0 as \"-0\", so it is exact");
        assert_eq!(kind("0xdeadbeef"), "HexStr(Hex(deadbeef), HexFmt(66: Prefix|Width(8)))");
        assert_eq!(kind("0x00ff"), "HexStr(Hex(ff), HexFmt(34: Prefix|Width(4)))");
        assert_eq!(kind("0xDEAD"), "HexStr(Hex(dead), HexFmt(35: Upper|Prefix|Width(4)))");
        assert_eq!(kind("0x0123456789abcdef"), "HexStr(Hex(123456789abcdef), HexFmt(130: Prefix|Width(16)))");
        assert!(kind("host=db1").starts_with("KeyVal(CompactStr("), "{}", kind("host=db1"));
        assert!(kind("a=b=c").contains("\"b=c\""), "value runs to the end: {}", kind("a=b=c"));
        assert!(kind("k=").contains("CompactStr(0: \"\")"), "empty value is index 0: {}", kind("k="));
        assert!(kind("k=a b").starts_with("Word("), "whitespace: {}", kind("k=a b"));

        // spellings that would not render back stay words
        for text in ["007", "+5", "2.50", "1e5", ".5", "100.0", "0xdEaD", "0x", "0x0123456789abcdef0",
                     "0xgg", "fe80:0:0:0:0:0:0:1", "010.0.0.1", "2026-13-01", "2026-11-00", "2026-11-1",
                     "24:00:00", "7:05:00", "07:05", "=v", "inf", "NaN", "infinity", "1e999", "-", "x"] {
            assert!(kind(text).starts_with("Word("), "{text:?} must stay a word: {}", kind(text));
        }
        // ... including ones a later recognizer would have claimed lossily
        assert_eq!(kind("99999999999999999999"), format!("Word(CompactStr({}: \"99999999999999999999\"))",
                   store.idx("99999999999999999999").unwrap()));

        // no calendar: the shape is what is recognized
        assert_eq!(kind("2026-02-31"), "Day(2026, 2, 31)");
    }

    #[test]
    fn test_recognizer_order_and_selection() {
        let store: UniqueStrStore = UniqueStrStore::new();
        let rules: ScanRules<'_> = ScanRules::new(&[]);

        // only integers: everything else is a word
        let ints: Parser<'_> = Parser::new(rules).recognizers(&[Recognizer::Integer]);
        let line: StructuredLine = ints.parse(&store, "10.0.0.1");
        assert!(matches!(line.elements()[0], TextElement::Word(_)));
        let line: StructuredLine = ints.parse(&store, "42");
        assert!(matches!(line.elements()[0], TextElement::Integer(42)));

        // no recognizers: words only, and the round trip still holds
        let words: Parser<'_> = Parser::new(rules).recognizers(&[]);
        let line: StructuredLine = words.parse(&store, "42");
        assert!(matches!(line.elements()[0], TextElement::Word(_)));
        assert_eq!(line.with_store(&store).to_string(), "42");

        // KeyVal before Integer makes no difference to "42", but a lossy
        // earlier recognizer hands over to the next: Float first, then Integer
        let float_first: Parser<'_> =
            Parser::new(rules).recognizers(&[Recognizer::Float, Recognizer::Integer]);
        let line: StructuredLine = float_first.parse(&store, "42");
        assert!(matches!(line.elements()[0], TextElement::Float(x) if x == 42.0));
        let line: StructuredLine = float_first.parse(&store, "007");
        assert!(matches!(line.elements()[0], TextElement::Word(_)));
    }

    #[rustfmt::skip]
    #[test]
    fn test_parse_log_line() {
        let store: UniqueStrStore = UniqueStrStore::new();
        let pairs: [(&str, &str); 3] = [("\"", "\""), ("[", "]"), ("(", ")")];
        let rules: ScanRules<'_> = ScanRules::new(&[",", ";"]).runs(" \t*").enclosures(&pairs);
        let parser: Parser<'_> = Parser::new(rules);

        let text: &str = "2026-11-01 07:05:00 db1 sshd[4242]: Accepted key for root from 10.0.0.1 port 22 \
                          (len=0x00ff, t=0.25); \"quoted, text\" *** --- [fe80::1] [] \"";
        let line: StructuredLine = parser.parse(&store, text);
        assert_eq!(line.with_store(&store).to_string(), text, "round trip");

        let got: Vec<String> = dbg(&line, &store);
        let want: Vec<&str> = vec![
            "Day(2026, 11, 1)", "Delimiter(Character(32: \" \" x1))", "Time(7, 5, 0)",
            "Delimiter(Character(32: \" \" x1))", "Word(CompactStr(", "Delimiter(Character(32: \" \" x1))",
            "Word(CompactStr(", "Open(Character(91: \"[\" x1))", "Integer(4242)", "Close(Character(93: \"]\" x1))",
            "Word(CompactStr(", // the ":" after the bracket: its own word, as it is no delimiter here
            "Delimiter(Character(32: \" \" x1))", "Word(CompactStr(", "Delimiter(Character(32: \" \" x1))",
            "Word(CompactStr(", "Delimiter(Character(32: \" \" x1))", "Word(CompactStr(",
            "Delimiter(Character(32: \" \" x1))", "Word(CompactStr(", "Delimiter(Character(32: \" \" x1))",
            "Word(CompactStr(", "Delimiter(Character(32: \" \" x1))", "IPv4(10.0.0.1)",
            "Delimiter(Character(32: \" \" x1))", "Word(CompactStr(", "Delimiter(Character(32: \" \" x1))",
            "Integer(22)", "Delimiter(Character(32: \" \" x1))", "Open(Character(40: \"(\" x1))",
            "RawText(CompactStr(", "Close(Character(41: \")\" x1))", "Delimiter(Character(59: \";\" x1))",
            "Delimiter(Character(32: \" \" x1))", "Open(Character(34: \"\\\"\" x1))", "RawText(CompactStr(",
            "Close(Character(34: \"\\\"\" x1))", "Delimiter(Character(32: \" \" x1))", "Char(Character(42: \"*\" x3))",
            "Delimiter(Character(32: \" \" x1))", "Word(CompactStr(", "Delimiter(Character(32: \" \" x1))",
            "Open(Character(91: \"[\" x1))", "IPv6(fe80::1)", "Close(Character(93: \"]\" x1))",
            "Delimiter(Character(32: \" \" x1))", "Open(Character(91: \"[\" x1))", "Close(Character(93: \"]\" x1))",
            "Delimiter(Character(32: \" \" x1))", "Open(Character(34: \"\\\"\" x1))",
        ];
        assert_eq!(got.len(), want.len(), "{got:#?}");
        for (i, (g, w)) in got.iter().zip(&want).enumerate() {
            assert!(g.starts_with(w), "element {i}: got {g}, want {w}…");
        }
        // the enclosure body "len=0x00ff, t=0.25" did not split on the comma and is raw text
        assert!(got[29].contains("\"len=0x00ff, t=0.25\""), "{}", got[29]);
        // "sshd[4242]:" — the word before the bracket and the colon after it
        assert!(got[6].contains("\"sshd\"") && got[10].contains("\":\""), "{} / {}", got[6], got[10]);
    }

    #[test]
    fn test_runs_become_characters() {
        let store: UniqueStrStore = UniqueStrStore::new();
        let parser: Parser<'_> = Parser::new(ScanRules::new(&[]).runs(" *"));

        let line: StructuredLine = parser.parse(&store, "a   b");
        assert_eq!(dbg(&line, &store)[1], "Delimiter(Character(32: \" \" x3))");
        let line: StructuredLine = parser.parse(&store, "***");
        assert_eq!(dbg(&line, &store), ["Char(Character(42: \"*\" x3))"]);

        // a run longer than 255 is split, and still renders back
        let long: String = " ".repeat(300);
        let line: StructuredLine = parser.parse(&store, &long);
        assert_eq!(
            dbg(&line, &store),
            [
                "Delimiter(Character(32: \" \" x255))",
                "Delimiter(Character(32: \" \" x45))"
            ]
        );
        assert_eq!(line.with_store(&store).to_string(), long);
        let exact: String = "*".repeat(510);
        assert_eq!(parser.parse(&store, &exact).len(), 2);

        // single characters cost no new store entry: they are the Latin-1 table
        let before: usize = store.len();
        parser.parse(&store, "  **  ");
        assert_eq!(store.len(), before);
    }

    #[test]
    fn test_enclosures_become_brackets() {
        let store: UniqueStrStore = UniqueStrStore::new();
        let parser: Parser<'static> = Parser::default();

        // the body goes through the recognizers
        let line: StructuredLine = parser.parse(&store, "[2026-11-01] (42) \"fe80::1\"");
        let got: Vec<String> = dbg(&line, &store);
        assert_eq!(got[1], "Day(2026, 11, 1)");
        assert_eq!(got[5], "Integer(42)");
        assert_eq!(got[9], "IPv6(fe80::1)");
        // an unclaimed body is raw text; an unclaimed free piece a word
        let line: StructuredLine = parser.parse(&store, "\"a b\" c");
        let got: Vec<String> = dbg(&line, &store);
        assert!(
            got[1].starts_with("RawText(") && got[1].contains("\"a b\""),
            "{}",
            got[1]
        );
        assert!(got[4].starts_with("Word("), "{}", got[4]);
        // unterminated: Open, body, no Close; empty: Open, Close
        let line: StructuredLine = parser.parse(&store, "x \"y z");
        assert_eq!(line.len(), 4);
        assert!(matches!(line.elements()[2], TextElement::Open(_)));
        assert!(matches!(line.elements()[3], TextElement::RawText(_)));
        let line: StructuredLine = parser.parse(&store, "[]");
        assert_eq!(line.len(), 2);
        assert!(line.elements().iter().all(TextElement::is_delim));
    }

    #[test]
    fn test_parse_edge_cases() {
        let store: UniqueStrStore = UniqueStrStore::new();
        let parser: Parser<'static> = Parser::default();
        assert!(parser.parse(&store, "").is_empty());
        let line: StructuredLine = parser.parse(&store, "   ");
        assert_eq!(line.len(), 1);
        assert_eq!(line.with_store(&store).to_string(), "   ");

        // parsing the same line twice adds nothing to the store
        parser.parse(&store, "the quick brown fox 10.0.0.1 jumps=over 42");
        let before: usize = store.len();
        parser.parse(&store, "the quick brown fox 10.0.0.1 jumps=over 42");
        assert_eq!(store.len(), before);
        // recognized values add nothing either
        parser.parse(&store, "10.0.0.2 43 2026-11-02 0xbeef 1.5");
        assert_eq!(store.len(), before, "scalars must not be interned");
    }

    #[rustfmt::skip]
    #[test]
    fn test_parse_round_trips() {
        /*
        Random log-like lines must come back byte for byte, whatever the
        recognizers make of their pieces. The pool mixes exact and lossy
        spellings of every recognized shape with delimiters, runs and
        enclosures (nested, empty, unterminated). Deterministic LCG so a
        failure is reproducible from the seed.
        */
        const POOL: [&str; 48] = [
            "GET", "error", "user", "a", "ümlaut", "日本",
            "42", "-7", "007", "+3", "3.14", "2.50", "1e5", "-0", "99999999999999999999",
            "10.0.0.1", "fe80::1", "::1", "256.1.1.1", "fe80:0:0:0:0:0:0:1",
            "2026-11-01", "2026-13-01", "26-11-01", "07:05:00", "24:00:00", "7:05:00",
            "0xdeadbeef", "0xDEAD", "0x", "deadbeef", "0xdEaD", "0x00ff",
            "k=v", "k=", "=v", "a=b=c",
            " ", "  ", "\t", ", ", ";", "***", "---",
            "\"quoted text\"", "[2026-11-01]", "(a (b) c)", "\"unterminated", "[]",
        ];
        let store: UniqueStrStore = UniqueStrStore::new();
        let pairs: [(&str, &str); 3] = [("\"", "\""), ("[", "]"), ("(", ")")];
        let rules: ScanRules<'_> = ScanRules::new(&[",", ";"]).runs(" \t*-").enclosures(&pairs);
        let parser: Parser<'_> = Parser::new(rules);
        let mut seed: u64 = 11;
        let mut next = move |modulus: u64| -> usize {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            ((seed >> 33) % modulus) as usize
        };

        for case in 0..3000 {
            let text: String = (0..next(12)).map(|_| POOL[next(POOL.len() as u64)]).collect();
            let line: StructuredLine = parser.parse(&store, &text);
            assert_eq!(line.with_store(&store).to_string(), text, "case {case}");
            // and a reparse is the same line
            assert_eq!(parser.parse(&store, &text), line, "case {case}: parsing must be deterministic");
        }
        store.validate_contents().expect("store consistent after parsing");
    }
}

// Copyright (c) 2024-2026 Mikko Tanner. All rights reserved.

/*!
Structured text: a line of log-like text as a compact sequence of
elements, with words interned in a [UniqueStrStore] and numbers, IP
addresses, dates and hex as native values.

Only the data model exists so far; nothing produces a [StructuredLine]
from text yet. The design is `doc/design/structured-text.md`.
*/

use crate::UniqueStrStore;
use std::{
    fmt::{self, Debug, Display, Formatter},
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

/*
NOTE: the dormant structured-text scaffolding below carries scoped
`#[expect(dead_code)]` attributes instead of a crate-wide allow. The
moment an item gets wired up, its attribute reports itself as an
"unfulfilled lint expectation" — remove it then. Items that the tests
already exercise (`StructuredLine`, `BigElement` and the `with_store`
formatting helpers) use `#[cfg_attr(not(test), expect(dead_code))]` so
the expectation holds in both build configurations. The type definitions
themselves need no attribute: rustc treats the `expect`-annotated impls
and `TextElement` as live roots, which keeps the types they reference
transitively live.

The target design is `doc/design/structured-text.md`; the data model
below follows it (step 1 of the implementation order). The parser that
produces a [StructuredLine] from text does not exist yet.
*/

/**
A value from the structured-text scaffolding paired with the
[UniqueStrStore] its indices refer to, so that it can be formatted.

`Display` recreates the text; `Debug` prints the index *and* the text
(`CompactStr(256: "foo")`), which is what makes a dumped [StructuredLine]
legible. Obtained through the `with_store` method of [CompactStr],
[Character], [TextElement], [BigElement] and [StructuredLine]; none of
them holds a store reference of its own.
*/
#[cfg_attr(not(test), expect(dead_code))]
struct WithStore<'a, T: ?Sized>(&'a T, &'a UniqueStrStore);

/**
A reference (index) to a stored string slice in a [UniqueStrStore].

This is a compact version which lacks a reference back to the containing store,
hence it is only usable as a part of a larger structure with a reference.
*/
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CompactStr(u32);

#[cfg_attr(not(test), expect(dead_code))]
impl CompactStr {
    /// Pair with `store` for `Display` / `Debug`; see [WithStore].
    fn with_store<'a>(&'a self, store: &'a UniqueStrStore) -> WithStore<'a, Self> {
        WithStore(self, store)
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

#[expect(dead_code)]
impl CompactStr {
    #[inline]
    fn idx(&self) -> u32 {
        self.0
    }

    fn get<'a>(&self, store: &'a UniqueStrStore) -> &'a str {
        unsafe { store.borrow_str(self.0) }
    }

    fn to_string(self, store: &UniqueStrStore) -> String {
        self.get(store).to_string()
    }
}

/// This is a single or repeated character stored in a [UniqueStrStore].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Character(CompactStr, u8);

#[cfg_attr(not(test), expect(dead_code))]
impl Character {
    /// Pair with `store` for `Display` / `Debug`; see [WithStore].
    fn with_store<'a>(&'a self, store: &'a UniqueStrStore) -> WithStore<'a, Self> {
        WithStore(self, store)
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
        write!(f, "Character({}: {:?} x{})", self.0.idx(), self.0.get(self.1), self.0.num())
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

#[expect(dead_code)]
impl Character {
    #[inline]
    fn idx(&self) -> u32 {
        self.0 .0
    }

    #[inline]
    fn num(&self) -> u8 {
        self.1
    }

    fn get<'a>(&self, store: &'a UniqueStrStore) -> &'a str {
        unsafe { store.borrow_str(self.idx()) }
    }

    fn to_string(self, store: &UniqueStrStore) -> String {
        self.get(store).repeat(self.num() as usize)
    }
}

/* --------------------------------- */

/**
Possible text elements in a structured line.

Every inline payload is at most 8 bytes (or 12 at alignment 4), so the
enum is [TEXT_ELEMENT_SIZE] bytes; the assertion below pins that. Any
payload that does not fit lives in [BigElement] behind the single `Big`
variant, which the constructors and accessors in the impl hide. See
`doc/design/structured-text.md`.
*/
#[expect(dead_code)]
#[derive(Debug, PartialEq)]
enum TextElement {
    /// An element which is explicitly a delimiter, f.ex. space (`" "`).
    Delimiter(Character),
    /// A single (or repeated) character, f.ex. `*` or `***`.
    Char(Character),
    /// The opening bracket of an enclosure, f.ex. `(` or `"`. The enclosed
    /// elements follow in the stream, up to the matching `Close`.
    Open(Character),
    /// The closing bracket of an enclosure; see `Open`.
    Close(Character),
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
    /// Unprocessed text, interned like a word.
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
move across the inline/boxed boundary without touching callers.
*/
#[cfg_attr(not(test), expect(dead_code))]
#[derive(Debug, PartialEq)]
enum BigElement {
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

#[cfg_attr(not(test), expect(dead_code))]
impl TextElement {
    /// Pair with `store` for `Display` / `Debug`; see [WithStore].
    fn with_store<'a>(&'a self, store: &'a UniqueStrStore) -> WithStore<'a, Self> {
        WithStore(self, store)
    }

    #[inline]
    fn big(elem: BigElement) -> Self {
        Self::Big(Box::new(elem))
    }

    /// An IP address of either family; only IPv6 is boxed.
    fn ip(addr: IpAddr) -> Self {
        match addr {
            IpAddr::V4(v4) => Self::IPv4(v4),
            IpAddr::V6(v6) => Self::big(BigElement::IPv6(v6)),
        }
    }

    /// A range of integers with its "range" marker; see [BigElement::Range].
    fn range(from: i64, to: i64, marker: Character) -> Self {
        Self::big(BigElement::Range(from, to, marker))
    }

    /// A sentence of stored words; see [BigElement::Sentence].
    fn sentence(words: Vec<CompactStr>) -> Self {
        Self::big(BigElement::Sentence(words))
    }

    /// A URL in stored parts; see [BigElement::URLStr].
    fn url(parts: Vec<CompactStr>) -> Self {
        Self::big(BigElement::URLStr(parts))
    }

    /// URL query params; see [BigElement::URLParams].
    fn url_params(params: Vec<CompactStr>) -> Self {
        Self::big(BigElement::URLParams(params))
    }

    /// The IP address, whichever family and wherever it is stored.
    fn as_ip(&self) -> Option<IpAddr> {
        match self {
            Self::IPv4(v4) => Some(IpAddr::V4(*v4)),
            Self::Big(big) => match **big {
                BigElement::IPv6(v6) => Some(IpAddr::V6(v6)),
                _ => None,
            },
            _ => None,
        }
    }
}

#[cfg_attr(not(test), expect(dead_code))]
impl BigElement {
    /// Pair with `store` for `Display` / `Debug`; see [WithStore].
    fn with_store<'a>(&'a self, store: &'a UniqueStrStore) -> WithStore<'a, Self> {
        WithStore(self, store)
    }
}

/**
Recreates the element's text. This is the *canonical* rendering of the
value: the `=` of a `KeyVal`, zero-padded date fields, the shortest
float. The planned parser only emits a recognized value when this
rendering equals the slice it came from and keeps the slice as a `Word`
otherwise (see `doc/design/structured-text.md`), so a parsed line
concatenates back to its input byte for byte. A hand-built line has no
such guarantee.
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
            TextElement::HexStr(hex, fmt) => f.write_str(&Hex::to_string(*hex, *fmt)),
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
            TextElement::Delimiter(c) => f.debug_tuple("Delimiter").field(&c.with_store(store)).finish(),
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
            TextElement::Time(h, m, sec) => f.debug_tuple("Time").field(h).field(m).field(sec).finish(),
            TextElement::Timestamp(t) => f.debug_tuple("Timestamp").field(&format_args!("{t}")).finish(),
            TextElement::IPv4(ip) => f.debug_tuple("IPv4").field(ip).finish(),
            TextElement::Hostname(h) => f.debug_tuple("Hostname").field(&h.with_store(store)).finish(),
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
            TextElement::RawText(s) => f.debug_tuple("RawText").field(&s.with_store(store)).finish(),
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
index with its store. The derived `Debug` shows raw indices; `with_store` resolves them.
*/
#[derive(Debug, Default, PartialEq)]
struct StructuredLine {
    elems: Vec<TextElement>,
}

#[cfg_attr(not(test), expect(dead_code))]
impl StructuredLine {
    fn new() -> Self {
        Self::default()
    }

    fn len(&self) -> usize {
        self.elems.len()
    }

    fn is_empty(&self) -> bool {
        self.elems.is_empty()
    }

    fn push(&mut self, elem: TextElement) {
        self.elems.push(elem);
    }

    /// Pair with `store` for `Display` / `Debug`; see [WithStore].
    fn with_store<'a>(&'a self, store: &'a UniqueStrStore) -> WithStore<'a, Self> {
        WithStore(self, store)
    }
}

/// Recreates the line by concatenating its elements (canonical rendering,
/// see [TextElement]'s `Display`).
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
#[derive(Clone, Copy, PartialEq)]
struct Hex(u64);

#[expect(dead_code)]
impl Hex {
    fn get(&self) -> u64 {
        self.0
    }

    fn to_string(self, fmt: HexFormat) -> String {
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
#[derive(Clone, Copy, PartialEq)]
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

/* ################################ TESTS ################################## */

#[cfg(test)]
mod tests {
    use super::*;

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
        assert_eq!(line.elems[9].as_ip(), Some("10.0.0.1".parse().unwrap()));
        assert_eq!(line.elems[10].as_ip(), Some("fe80::1".parse().unwrap()));
        assert_eq!(line.elems[0].as_ip(), None);
        assert!(matches!(line.elems[9], TextElement::IPv4(_)));
        assert!(matches!(line.elems[10], TextElement::Big(_)));

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
    fn test_hex_to_string() {
        let h: Hex = Hex(0xdeadbeef);
        assert_eq!(h.to_string(HexFormat::PLAIN), "deadbeef");
        assert_eq!(h.to_string(HexFormat::UPPER), "DEADBEEF");
        assert_eq!(h.to_string(HexFormat::PREFIX), "0xdeadbeef");
        assert_eq!(h.to_string(HexFormat::COLUMNS), "dead:beef");

        // groups count from the right when the digit count isn't a multiple of 4
        assert_eq!(Hex(0xfeedf00d5).to_string(HexFormat::COLUMNS), "f:eedf:00d5");
        assert_eq!(Hex(0x5).to_string(HexFormat::COLUMNS), "5");

        // the "0x" prefix is applied last: neither uppercased nor grouped
        let all: HexFormat =
            HexFormat(HexFormat::UPPER.0 | HexFormat::PREFIX.0 | HexFormat::COLUMNS.0);
        assert_eq!(h.to_string(all), "0xDEAD:BEEF");

        assert_eq!(all.to_string(), "Upper|Prefix|Columns");
        assert_eq!(HexFormat::PLAIN.to_string(), "Plain");

        // a fixed width restores leading zeros; the columns group the padded digits
        assert_eq!(Hex(0xff).to_string(HexFormat::PREFIX.with_width(4)), "0x00ff");
        assert_eq!(Hex(0xff).to_string(HexFormat::COLUMNS.with_width(8)), "0000:00ff");
        assert_eq!(Hex(0xdeadbeef).to_string(HexFormat::PLAIN.with_width(2)), "deadbeef");
        let wide: HexFormat = HexFormat::UPPER.with_width(4);
        assert_eq!(wide.width(), 4);
        assert!(wide.is_upper() && !wide.is_prefix());
        assert_eq!(wide.to_string(), "Upper|Width(4)");
        assert_eq!(HexFormat::PLAIN.with_width(4).to_string(), "Width(4)");
        assert_eq!(HexFormat::PLAIN.with_width(1), HexFormat(0b1000));
        assert_eq!(HexFormat::PLAIN.with_width(u8::MAX).width(), HexFormat::MAX_WIDTH);
        assert_eq!(wide.with_width(0), HexFormat::UPPER);
    }
}

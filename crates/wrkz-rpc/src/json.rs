// Copyright (c) 2026, The WrkzCoin developers
//
// Please see the included LICENSE file for more information

//! A strict JSON value, parser and writer, sized for a daemon that listens on
//! a socket.
//!
//! Why not a JSON crate: two of this crate's requirements are unusual enough
//! that owning the code is cheaper than bending a library to them.
//!
//! 1. **Key order.** The C++ builds every response with `nlohmann::json`, whose
//!    default `basic_json` stores objects in a `std::map<std::string, ...>`.
//!    `dump()` therefore emits keys in ascending byte order, and every captured
//!    sample in `spec/vectors` is alphabetical. [`Json::to_string`] sorts keys
//!    the same way, so our bodies are byte-identical to the C++ for the same
//!    values — see the crate docs, "Field order".
//! 2. **Integers.** Amounts, difficulties and global indexes are `u64` and must
//!    never round-trip through `f64`. [`Json::U64`] and [`Json::I64`] are
//!    separate from [`Json::F64`], and the parser only produces `F64` for a
//!    literal that actually carries a fraction or an exponent.
//!
//! The parser also carries the limits a public endpoint needs: a nesting depth
//! cap, a byte cap on the input, and no allocation sized from anything the
//! client declared (there is no length prefix in JSON, and every `Vec` here
//! grows from bytes actually read).

use std::fmt::Write as _;

/// Deepest nesting a request body may use (`RpcLimits::max_json_depth`
/// defaults to this). The C++ uses `nlohmann::json::parse`, whose recursion is
/// bounded only by the stack; ours is bounded by a number.
pub const DEFAULT_MAX_DEPTH: usize = 64;

/// A JSON value.
///
/// `Object` keeps insertion order for the convenience of the code that builds
/// it; [`Json::to_string`] ignores that order and emits keys sorted, which is
/// what the C++ does.
#[derive(Clone, Debug, PartialEq)]
pub enum Json {
    Null,
    Bool(bool),
    /// A non-negative integer literal that fits in 64 bits.
    U64(u64),
    /// A negative integer literal that fits in 64 bits.
    I64(i64),
    /// A literal with a fraction or an exponent, or an integer too large for
    /// 64 bits. No handler in this crate emits one.
    F64(f64),
    Str(String),
    Array(Vec<Json>),
    Object(Vec<(String, Json)>),
}

/// Why a body was not JSON this server would accept.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum JsonError {
    /// A syntax fault, with the byte offset it was found at.
    Syntax(&'static str, usize),
    /// More than `max_depth` nested arrays or objects.
    TooDeep(usize),
    /// The body was longer than `max_bytes`.
    TooLarge(usize),
    /// Not valid UTF-8.
    NotUtf8,
}

impl std::fmt::Display for JsonError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            JsonError::Syntax(what, at) => write!(f, "{what} at byte {at}"),
            JsonError::TooDeep(d) => write!(f, "JSON nested deeper than {d}"),
            JsonError::TooLarge(n) => write!(f, "JSON body over {n} bytes"),
            JsonError::NotUtf8 => write!(f, "body is not valid UTF-8"),
        }
    }
}

impl std::error::Error for JsonError {}

/// What [`parse`] will accept.
#[derive(Clone, Copy, Debug)]
pub struct ParseLimits {
    pub max_bytes: usize,
    pub max_depth: usize,
}

impl Default for ParseLimits {
    fn default() -> Self {
        Self { max_bytes: 2 * 1024 * 1024, max_depth: DEFAULT_MAX_DEPTH }
    }
}

/// Parse a whole document. Trailing bytes other than whitespace are a syntax
/// error, as they are for `nlohmann::json::parse`.
pub fn parse(bytes: &[u8], limits: ParseLimits) -> Result<Json, JsonError> {
    if bytes.len() > limits.max_bytes {
        return Err(JsonError::TooLarge(limits.max_bytes));
    }
    let text = std::str::from_utf8(bytes).map_err(|_| JsonError::NotUtf8)?;
    let mut p = Parser { b: text.as_bytes(), at: 0, depth: 0, max_depth: limits.max_depth };
    p.skip_ws();
    let v = p.value()?;
    p.skip_ws();
    if p.at != p.b.len() {
        return Err(JsonError::Syntax("trailing data", p.at));
    }
    Ok(v)
}

struct Parser<'a> {
    b: &'a [u8],
    at: usize,
    depth: usize,
    max_depth: usize,
}

impl Parser<'_> {
    fn skip_ws(&mut self) {
        while let Some(c) = self.b.get(self.at) {
            match c {
                b' ' | b'\t' | b'\n' | b'\r' => self.at += 1,
                _ => break,
            }
        }
    }

    fn peek(&self) -> Option<u8> {
        self.b.get(self.at).copied()
    }

    fn eat(&mut self, c: u8, what: &'static str) -> Result<(), JsonError> {
        if self.peek() == Some(c) {
            self.at += 1;
            Ok(())
        } else {
            Err(JsonError::Syntax(what, self.at))
        }
    }

    fn literal(&mut self, word: &[u8], v: Json) -> Result<Json, JsonError> {
        if self.b[self.at..].starts_with(word) {
            self.at += word.len();
            Ok(v)
        } else {
            Err(JsonError::Syntax("unexpected literal", self.at))
        }
    }

    fn value(&mut self) -> Result<Json, JsonError> {
        match self.peek() {
            None => Err(JsonError::Syntax("unexpected end of input", self.at)),
            Some(b'n') => self.literal(b"null", Json::Null),
            Some(b't') => self.literal(b"true", Json::Bool(true)),
            Some(b'f') => self.literal(b"false", Json::Bool(false)),
            Some(b'"') => Ok(Json::Str(self.string()?)),
            Some(b'[') => self.array(),
            Some(b'{') => self.object(),
            Some(c) if c == b'-' || c.is_ascii_digit() => self.number(),
            Some(_) => Err(JsonError::Syntax("unexpected character", self.at)),
        }
    }

    fn enter(&mut self) -> Result<(), JsonError> {
        self.depth += 1;
        if self.depth > self.max_depth {
            return Err(JsonError::TooDeep(self.max_depth));
        }
        Ok(())
    }

    fn array(&mut self) -> Result<Json, JsonError> {
        self.enter()?;
        self.at += 1; // '['
        let mut items = Vec::new();
        self.skip_ws();
        if self.peek() == Some(b']') {
            self.at += 1;
            self.depth -= 1;
            return Ok(Json::Array(items));
        }
        loop {
            self.skip_ws();
            items.push(self.value()?);
            self.skip_ws();
            match self.peek() {
                Some(b',') => self.at += 1,
                Some(b']') => {
                    self.at += 1;
                    break;
                }
                _ => return Err(JsonError::Syntax("expected ',' or ']'", self.at)),
            }
        }
        self.depth -= 1;
        Ok(Json::Array(items))
    }

    fn object(&mut self) -> Result<Json, JsonError> {
        self.enter()?;
        self.at += 1; // '{'
        let mut items: Vec<(String, Json)> = Vec::new();
        self.skip_ws();
        if self.peek() == Some(b'}') {
            self.at += 1;
            self.depth -= 1;
            return Ok(Json::Object(items));
        }
        loop {
            self.skip_ws();
            let key = self.string()?;
            self.skip_ws();
            self.eat(b':', "expected ':'")?;
            self.skip_ws();
            let value = self.value()?;
            // `nlohmann` inserts with `operator[]`, which only adds when the key
            // is absent, so a duplicate key keeps the *first* value.
            if !items.iter().any(|(k, _)| *k == key) {
                items.push((key, value));
            }
            self.skip_ws();
            match self.peek() {
                Some(b',') => self.at += 1,
                Some(b'}') => {
                    self.at += 1;
                    break;
                }
                _ => return Err(JsonError::Syntax("expected ',' or '}'", self.at)),
            }
        }
        self.depth -= 1;
        Ok(Json::Object(items))
    }

    fn string(&mut self) -> Result<String, JsonError> {
        self.eat(b'"', "expected a string")?;
        let mut out = String::new();
        loop {
            let c = self.peek().ok_or(JsonError::Syntax("unterminated string", self.at))?;
            self.at += 1;
            match c {
                b'"' => return Ok(out),
                b'\\' => {
                    let e = self.peek().ok_or(JsonError::Syntax("unterminated escape", self.at))?;
                    self.at += 1;
                    match e {
                        b'"' => out.push('"'),
                        b'\\' => out.push('\\'),
                        b'/' => out.push('/'),
                        b'b' => out.push('\u{8}'),
                        b'f' => out.push('\u{c}'),
                        b'n' => out.push('\n'),
                        b'r' => out.push('\r'),
                        b't' => out.push('\t'),
                        b'u' => out.push(self.unicode_escape()?),
                        _ => return Err(JsonError::Syntax("bad escape", self.at)),
                    }
                }
                // Unescaped control characters are invalid JSON.
                0x00..=0x1f => return Err(JsonError::Syntax("control character in string", self.at)),
                _ => {
                    // The input is known-valid UTF-8, so re-assemble the code
                    // point from the bytes the lexer walked past.
                    let start = self.at - 1;
                    let end = start + utf8_len(c);
                    let s = self.b.get(start..end).ok_or(JsonError::Syntax("truncated UTF-8", start))?;
                    out.push_str(std::str::from_utf8(s).map_err(|_| JsonError::NotUtf8)?);
                    self.at = end;
                }
            }
        }
    }

    fn unicode_escape(&mut self) -> Result<char, JsonError> {
        let first = self.hex4()?;
        // A high surrogate must be followed by `\uDC00..\uDFFF`.
        if (0xd800..0xdc00).contains(&first) {
            if self.b.get(self.at) == Some(&b'\\') && self.b.get(self.at + 1) == Some(&b'u') {
                self.at += 2;
                let second = self.hex4()?;
                if (0xdc00..0xe000).contains(&second) {
                    let c = 0x10000 + (((first - 0xd800) as u32) << 10) + (second - 0xdc00) as u32;
                    return char::from_u32(c).ok_or(JsonError::Syntax("bad surrogate pair", self.at));
                }
            }
            return Err(JsonError::Syntax("lone high surrogate", self.at));
        }
        char::from_u32(first as u32).ok_or(JsonError::Syntax("bad unicode escape", self.at))
    }

    fn hex4(&mut self) -> Result<u16, JsonError> {
        let s = self.b.get(self.at..self.at + 4).ok_or(JsonError::Syntax("truncated unicode escape", self.at))?;
        let mut v: u16 = 0;
        for &c in s {
            let d = match c {
                b'0'..=b'9' => c - b'0',
                b'a'..=b'f' => c - b'a' + 10,
                b'A'..=b'F' => c - b'A' + 10,
                _ => return Err(JsonError::Syntax("bad hex in a unicode escape", self.at)),
            };
            v = (v << 4) | d as u16;
        }
        self.at += 4;
        Ok(v)
    }

    fn number(&mut self) -> Result<Json, JsonError> {
        let start = self.at;
        if self.peek() == Some(b'-') {
            self.at += 1;
        }
        let int_start = self.at;
        while matches!(self.peek(), Some(c) if c.is_ascii_digit()) {
            self.at += 1;
        }
        if self.at == int_start {
            return Err(JsonError::Syntax("expected a digit", self.at));
        }
        // Leading zeros are invalid JSON ("0" alone is fine).
        if self.b[int_start] == b'0' && self.at - int_start > 1 {
            return Err(JsonError::Syntax("leading zero", int_start));
        }
        let mut floating = false;
        if self.peek() == Some(b'.') {
            floating = true;
            self.at += 1;
            let frac_start = self.at;
            while matches!(self.peek(), Some(c) if c.is_ascii_digit()) {
                self.at += 1;
            }
            if self.at == frac_start {
                return Err(JsonError::Syntax("expected a digit after the point", self.at));
            }
        }
        if matches!(self.peek(), Some(b'e') | Some(b'E')) {
            floating = true;
            self.at += 1;
            if matches!(self.peek(), Some(b'+') | Some(b'-')) {
                self.at += 1;
            }
            let exp_start = self.at;
            while matches!(self.peek(), Some(c) if c.is_ascii_digit()) {
                self.at += 1;
            }
            if self.at == exp_start {
                return Err(JsonError::Syntax("expected a digit in the exponent", self.at));
            }
        }
        let text = std::str::from_utf8(&self.b[start..self.at]).map_err(|_| JsonError::NotUtf8)?;
        if !floating {
            if let Ok(v) = text.parse::<u64>() {
                return Ok(Json::U64(v));
            }
            if let Ok(v) = text.parse::<i64>() {
                return Ok(Json::I64(v));
            }
        }
        // Either a real float, or an integer past 64 bits. Both land here, and
        // no handler reads one as an amount.
        text.parse::<f64>().map(Json::F64).map_err(|_| JsonError::Syntax("bad number", start))
    }
}

fn utf8_len(first: u8) -> usize {
    match first {
        0x00..=0x7f => 1,
        0xc0..=0xdf => 2,
        0xe0..=0xef => 3,
        _ => 4,
    }
}

// ---------------------------------------------------------------------------
// reading
// ---------------------------------------------------------------------------

impl Json {
    /// An object member, or `None` when this is not an object or has no such key.
    pub fn get(&self, key: &str) -> Option<&Json> {
        match self {
            Json::Object(items) => items.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    /// `hasMember(j, key)` (`include/JsonHelper.h:18`).
    pub fn has(&self, key: &str) -> bool {
        self.get(key).is_some()
    }

    pub fn is_object(&self) -> bool {
        matches!(self, Json::Object(_))
    }

    /// `get<uint64_t>()` restricted to what it can answer without surprise.
    ///
    /// `nlohmann` would turn a bool into 0/1 and wrap a negative integer; both
    /// are a type error here, which the middleware reports the way the C++
    /// reports a `type_error` (a refused request rather than a wrong answer).
    pub fn as_u64(&self) -> Option<u64> {
        match self {
            Json::U64(v) => Some(*v),
            _ => None,
        }
    }

    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Json::Bool(v) => Some(*v),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Json::Str(s) => Some(s.as_str()),
            _ => None,
        }
    }

    pub fn as_array(&self) -> Option<&[Json]> {
        match self {
            Json::Array(v) => Some(v.as_slice()),
            _ => None,
        }
    }

    /// The name of this value's type, for a diff report.
    pub fn type_name(&self) -> &'static str {
        match self {
            Json::Null => "null",
            Json::Bool(_) => "bool",
            Json::U64(_) | Json::I64(_) | Json::F64(_) => "number",
            Json::Str(_) => "string",
            Json::Array(_) => "array",
            Json::Object(_) => "object",
        }
    }
}

// ---------------------------------------------------------------------------
// building
// ---------------------------------------------------------------------------

/// An object under construction. Insertion order is irrelevant to the output
/// (keys are sorted on write); this exists so a handler reads like the C++ it
/// mirrors, line for line.
#[derive(Clone, Debug, Default)]
pub struct Obj(Vec<(String, Json)>);

impl Obj {
    pub fn new() -> Self {
        Self(Vec::new())
    }

    pub fn set(&mut self, key: &str, value: impl Into<Json>) -> &mut Self {
        let value = value.into();
        match self.0.iter_mut().find(|(k, _)| k == key) {
            Some(slot) => slot.1 = value,
            None => self.0.push((key.to_string(), value)),
        }
        self
    }

    pub fn build(self) -> Json {
        Json::Object(self.0)
    }
}

impl From<Obj> for Json {
    fn from(o: Obj) -> Self {
        o.build()
    }
}

macro_rules! from_unsigned {
    ($($t:ty),*) => {
        $(impl From<$t> for Json {
            fn from(v: $t) -> Self {
                Json::U64(v as u64)
            }
        })*
    };
}

from_unsigned!(u8, u16, u32, u64, usize);

impl From<i64> for Json {
    fn from(v: i64) -> Self {
        Json::I64(v)
    }
}
impl From<bool> for Json {
    fn from(v: bool) -> Self {
        Json::Bool(v)
    }
}
impl From<String> for Json {
    fn from(v: String) -> Self {
        Json::Str(v)
    }
}
impl From<&str> for Json {
    fn from(v: &str) -> Self {
        Json::Str(v.to_string())
    }
}
impl From<Vec<Json>> for Json {
    fn from(v: Vec<Json>) -> Self {
        Json::Array(v)
    }
}

/// An array of hex-encoded hashes.
pub fn hash_array<'a, I: IntoIterator<Item = &'a [u8; 32]>>(hashes: I) -> Json {
    Json::Array(hashes.into_iter().map(|h| Json::Str(hex::encode(h))).collect())
}

/// An array of `u64`s.
pub fn u64_array<I: IntoIterator<Item = u64>>(values: I) -> Json {
    Json::Array(values.into_iter().map(Json::U64).collect())
}

// ---------------------------------------------------------------------------
// writing
// ---------------------------------------------------------------------------

impl Json {
    /// `nlohmann::json::dump()`: compact, no spaces, keys in ascending byte
    /// order, `/` unescaped, non-ASCII emitted as UTF-8.
    #[allow(clippy::inherent_to_string)]
    pub fn to_string(&self) -> String {
        let mut s = String::new();
        self.write(&mut s);
        s
    }

    fn write(&self, out: &mut String) {
        match self {
            Json::Null => out.push_str("null"),
            Json::Bool(true) => out.push_str("true"),
            Json::Bool(false) => out.push_str("false"),
            Json::U64(v) => {
                let _ = write!(out, "{v}");
            }
            Json::I64(v) => {
                let _ = write!(out, "{v}");
            }
            Json::F64(v) => {
                if v.is_finite() {
                    let _ = write!(out, "{v}");
                } else {
                    // `nlohmann` dumps a non-finite number as `null`.
                    out.push_str("null");
                }
            }
            Json::Str(s) => write_string(s, out),
            Json::Array(items) => {
                out.push('[');
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    item.write(out);
                }
                out.push(']');
            }
            Json::Object(items) => {
                let mut order: Vec<&(String, Json)> = items.iter().collect();
                order.sort_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));
                out.push('{');
                for (i, (key, value)) in order.into_iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    write_string(key, out);
                    out.push(':');
                    value.write(out);
                }
                out.push('}');
            }
        }
    }
}

fn write_string(s: &str, out: &mut String) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> Json {
        parse(s.as_bytes(), ParseLimits::default()).expect("parses")
    }

    #[test]
    fn keys_come_out_sorted_like_nlohmann() {
        let mut o = Obj::new();
        o.set("status", "OK").set("height", 4_213_000u64).set("alt_blocks_count", 0u64);
        assert_eq!(o.build().to_string(), r#"{"alt_blocks_count":0,"height":4213000,"status":"OK"}"#);
    }

    #[test]
    fn u64_survives_a_round_trip() {
        let v = p(r#"{"a":18446744073709551615,"b":-9223372036854775808,"c":1.5,"d":1e3}"#);
        assert_eq!(v.get("a").unwrap(), &Json::U64(u64::MAX));
        assert_eq!(v.get("b").unwrap(), &Json::I64(i64::MIN));
        assert!(matches!(v.get("c").unwrap(), Json::F64(_)));
        assert!(matches!(v.get("d").unwrap(), Json::F64(_)));
        assert_eq!(Json::U64(u64::MAX).to_string(), "18446744073709551615");
        // An integer past 64 bits does not silently become a small number.
        assert!(matches!(p("123456789012345678901234567890"), Json::F64(_)));
    }

    #[test]
    fn nesting_is_capped() {
        let deep = format!("{}{}", "[".repeat(200), "]".repeat(200));
        let e = parse(deep.as_bytes(), ParseLimits { max_bytes: 4096, max_depth: 64 }).unwrap_err();
        assert_eq!(e, JsonError::TooDeep(64));
        // A body at the cap still parses.
        let ok = format!("{}{}", "[".repeat(64), "]".repeat(64));
        assert!(parse(ok.as_bytes(), ParseLimits { max_bytes: 4096, max_depth: 64 }).is_ok());
    }

    #[test]
    fn oversized_and_invalid_bodies_are_refused() {
        assert_eq!(parse(&[b'1'; 10], ParseLimits { max_bytes: 4, max_depth: 8 }).unwrap_err(), JsonError::TooLarge(4));
        assert_eq!(parse(&[0xff, 0xfe], ParseLimits::default()).unwrap_err(), JsonError::NotUtf8);
        for bad in ["", "{", "[1,]", "{\"a\"}", "01", "1.", "\"\\x\"", "tru", "{} {}", "\"a"] {
            assert!(parse(bad.as_bytes(), ParseLimits::default()).is_err(), "{bad} should not parse");
        }
    }

    #[test]
    fn strings_escape_the_way_nlohmann_dumps_them() {
        assert_eq!(Json::Str("a/b".into()).to_string(), r#""a/b""#, "nlohmann does not escape a slash");
        assert_eq!(Json::Str("\n\t\"\\".into()).to_string(), r#""\n\t\"\\""#);
        assert_eq!(Json::Str("\u{1}".into()).to_string(), r#""\u0001""#);
        assert_eq!(Json::Str("é".into()).to_string(), "\"é\"", "ensure_ascii is false");
        assert_eq!(p(r#""\u00e9""#), Json::Str("é".into()));
        assert_eq!(p(r#""\ud83d\ude00""#), Json::Str("😀".into()));
        assert!(parse(br#""\ud83d""#, ParseLimits::default()).is_err(), "a lone surrogate is refused");
    }

    #[test]
    fn a_duplicate_key_keeps_the_first_value_like_nlohmann() {
        assert_eq!(p(r#"{"a":1,"a":2}"#).get("a").unwrap(), &Json::U64(1));
    }

    #[test]
    fn typed_reads_refuse_the_wrong_type() {
        let v = p(r#"{"n":1,"s":"x","b":true,"neg":-1}"#);
        assert_eq!(v.get("n").unwrap().as_u64(), Some(1));
        assert_eq!(v.get("b").unwrap().as_u64(), None);
        assert_eq!(v.get("neg").unwrap().as_u64(), None);
        assert_eq!(v.get("s").unwrap().as_str(), Some("x"));
        assert_eq!(v.get("n").unwrap().as_str(), None);
        assert!(!v.has("missing"));
    }
}

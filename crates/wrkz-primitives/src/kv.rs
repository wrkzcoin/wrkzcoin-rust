// Copyright (c) 2026, The WrkzCoin developers
//
// Please see the included LICENSE file for more information

//! KV binary "portable storage" (spec/04-serialization.md "KV binary";
//! `src/serialization/KVBinary*`). Used for every P2P payload, every database
//! record and the peer state file.

use crate::{Error, Result};

pub const SIGNATURE_A: u32 = 0x01011101;
pub const SIGNATURE_B: u32 = 0x01020101;
pub const FORMAT_VERSION: u8 = 1;
pub const HEADER: [u8; 9] = [0x01, 0x11, 0x01, 0x01, 0x01, 0x01, 0x02, 0x01, 0x01];

pub const TYPE_INT64: u8 = 1;
pub const TYPE_INT32: u8 = 2;
pub const TYPE_INT16: u8 = 3;
pub const TYPE_INT8: u8 = 4;
pub const TYPE_UINT64: u8 = 5;
pub const TYPE_UINT32: u8 = 6;
pub const TYPE_UINT16: u8 = 7;
pub const TYPE_UINT8: u8 = 8;
pub const TYPE_DOUBLE: u8 = 9;
pub const TYPE_STRING: u8 = 10;
pub const TYPE_BOOL: u8 = 11;
pub const TYPE_OBJECT: u8 = 12;
pub const TYPE_ARRAY: u8 = 13;
pub const ARRAY_FLAG: u8 = 0x80;

/// Maximum object nesting a document may use. Every level costs at least four
/// wire bytes, so this only ever rejects deliberately deep input; the deepest
/// real structure on this network is three levels (the peer state file).
pub const MAX_DEPTH: usize = 32;

/// A decoded or to-be-encoded value. Integers carry their KV type byte so
/// the writer emits the width of the C++ field; readers widen to 64 bits and
/// callers convert by the receiving field's declared type, as the C++ does.
#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    Int(i64, u8),
    Uint(u64, u8),
    Double(f64),
    String(Vec<u8>),
    Bool(bool),
    Object(Section),
    /// Homogeneous array of scalars or objects (never nested arrays).
    Array(Vec<Value>),
}

/// An ordered list of named entries (writers emit declaration order).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Section {
    pub entries: Vec<(String, Value)>,
}

impl Section {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn get(&self, name: &str) -> Option<&Value> {
        self.entries.iter().find(|(k, _)| k == name).map(|(_, v)| v)
    }

    /// Any integer, widened through 64 bits like the C++ reader.
    pub fn get_u64(&self, name: &str) -> Option<u64> {
        match self.get(name)? {
            Value::Uint(v, _) => Some(*v),
            Value::Int(v, _) => Some(*v as u64),
            _ => None,
        }
    }

    pub fn get_bytes(&self, name: &str) -> Option<&[u8]> {
        match self.get(name)? {
            Value::String(s) => Some(s),
            _ => None,
        }
    }

    pub fn get_bool(&self, name: &str) -> Option<bool> {
        match self.get(name)? {
            Value::Bool(b) => Some(*b),
            _ => None,
        }
    }

    pub fn get_object(&self, name: &str) -> Option<&Section> {
        match self.get(name)? {
            Value::Object(s) => Some(s),
            _ => None,
        }
    }

    /// Missing array fields read as empty (the writer omits them).
    pub fn get_array(&self, name: &str) -> &[Value] {
        match self.get(name) {
            Some(Value::Array(a)) => a,
            _ => &[],
        }
    }

    // ---- builder API mirroring the C++ field types ----
    pub fn u8(mut self, name: &str, v: u8) -> Self {
        self.entries.push((name.into(), Value::Uint(v as u64, TYPE_UINT8)));
        self
    }
    pub fn u16(mut self, name: &str, v: u16) -> Self {
        self.entries.push((name.into(), Value::Uint(v as u64, TYPE_UINT16)));
        self
    }
    pub fn u32(mut self, name: &str, v: u32) -> Self {
        self.entries.push((name.into(), Value::Uint(v as u64, TYPE_UINT32)));
        self
    }
    pub fn u64(mut self, name: &str, v: u64) -> Self {
        self.entries.push((name.into(), Value::Uint(v, TYPE_UINT64)));
        self
    }
    pub fn i32(mut self, name: &str, v: i32) -> Self {
        self.entries.push((name.into(), Value::Int(v as i64, TYPE_INT32)));
        self
    }
    /// A fixed POD or a `serializeAsBinary` blob: an empty value is **not
    /// written at all**, matching `binary(void*, 0)` in
    /// `KVBinaryOutputStreamSerializer` (spec/04, "an empty binary blob is not
    /// written"). Use [`Section::text`] for a genuine `std::string` field,
    /// which the C++ writer emits even when empty.
    pub fn string(mut self, name: &str, v: &[u8]) -> Self {
        if !v.is_empty() {
            self.entries.push((name.into(), Value::String(v.to_vec())));
        }
        self
    }

    /// A `std::string` field, always written (an empty one becomes a
    /// zero-length KV string). Contrast [`Section::string`], which drops an
    /// empty value because that is what `serializeAsBinary` does.
    pub fn text(mut self, name: &str, v: &[u8]) -> Self {
        self.entries.push((name.into(), Value::String(v.to_vec())));
        self
    }
    pub fn bool(mut self, name: &str, v: bool) -> Self {
        self.entries.push((name.into(), Value::Bool(v)));
        self
    }
    pub fn object(mut self, name: &str, v: Section) -> Self {
        self.entries.push((name.into(), Value::Object(v)));
        self
    }
    /// Array of strings; an empty array is not written at all.
    pub fn string_array(mut self, name: &str, v: &[Vec<u8>]) -> Self {
        if !v.is_empty() {
            self.entries.push((name.into(), Value::Array(v.iter().map(|s| Value::String(s.clone())).collect())));
        }
        self
    }
    pub fn object_array(mut self, name: &str, v: Vec<Section>) -> Self {
        if !v.is_empty() {
            self.entries.push((name.into(), Value::Array(v.into_iter().map(Value::Object).collect())));
        }
        self
    }
}

// ---------------------------------------------------------------------------
// encoding
// ---------------------------------------------------------------------------

/// Write a KV varint (two low tag bits select the width).
///
/// Only the low 62 bits are representable: a value `>= 2^62` silently loses its
/// top bits. Every caller passes a length or a count, so this is unreachable in
/// practice, and the `debug_assert!` catches it in tests.
pub fn write_kv_varint(out: &mut Vec<u8>, v: u64) {
    debug_assert!(v < 1 << 62, "kv varint holds 62 bits, got {v}");
    if v <= 63 {
        out.push((v << 2) as u8);
    } else if v <= 16383 {
        out.extend_from_slice(&(((v << 2) | 1) as u16).to_le_bytes());
    } else if v < (1 << 30) {
        out.extend_from_slice(&(((v << 2) | 2) as u32).to_le_bytes());
    } else {
        out.extend_from_slice(&((v << 2) | 3).to_le_bytes());
    }
}

pub fn read_kv_varint(input: &[u8], pos: &mut usize) -> Result<u64> {
    let b0 = *input.get(*pos).ok_or(Error::Truncated)?;
    let size = match b0 & 3 {
        0 => 1,
        1 => 2,
        2 => 4,
        _ => 8,
    };
    if *pos + size > input.len() {
        return Err(Error::Truncated);
    }
    let mut raw = [0u8; 8];
    raw[..size].copy_from_slice(&input[*pos..*pos + size]);
    *pos += size;
    Ok(u64::from_le_bytes(raw) >> 2)
}

fn type_of(v: &Value) -> u8 {
    match v {
        Value::Uint(_, t) | Value::Int(_, t) => *t,
        Value::Double(_) => TYPE_DOUBLE,
        Value::String(_) => TYPE_STRING,
        Value::Bool(_) => TYPE_BOOL,
        Value::Object(_) => TYPE_OBJECT,
        Value::Array(_) => TYPE_ARRAY,
    }
}

fn write_scalar(out: &mut Vec<u8>, v: &Value) {
    match v {
        Value::Uint(u, t) => match *t {
            TYPE_UINT8 => out.push(*u as u8),
            TYPE_UINT16 => out.extend_from_slice(&(*u as u16).to_le_bytes()),
            TYPE_UINT32 => out.extend_from_slice(&(*u as u32).to_le_bytes()),
            _ => out.extend_from_slice(&u.to_le_bytes()),
        },
        Value::Int(i, t) => match *t {
            TYPE_INT8 => out.push(*i as i8 as u8),
            TYPE_INT16 => out.extend_from_slice(&(*i as i16).to_le_bytes()),
            TYPE_INT32 => out.extend_from_slice(&(*i as i32).to_le_bytes()),
            _ => out.extend_from_slice(&i.to_le_bytes()),
        },
        Value::Double(d) => out.extend_from_slice(&d.to_le_bytes()),
        Value::String(s) => {
            write_kv_varint(out, s.len() as u64);
            out.extend_from_slice(s);
        }
        Value::Bool(b) => out.push(*b as u8),
        Value::Object(s) => write_section(out, s),
        // TODO(stage-3): the builder API is infallible and every `Section` in
        // the tree is built from literals, so this cannot be reached today.
        // Make `encode` return `Result` once sections are built from decoded
        // or otherwise dynamic data.
        Value::Array(_) => unreachable!("nested arrays are never written"),
    }
}

/// # Panics
///
/// On an entry name that is empty or longer than 255 bytes, which the wire
/// format cannot express. The builder API is infallible, so this is a caller
/// bug; anything produced by [`decode`] already satisfies the invariant, since
/// the reader takes the length from a single byte and requires valid UTF-8.
fn write_section(out: &mut Vec<u8>, s: &Section) {
    write_kv_varint(out, s.entries.len() as u64);
    for (name, v) in &s.entries {
        assert!(!name.is_empty() && name.len() <= 255, "kv entry name length");
        out.push(name.len() as u8);
        out.extend_from_slice(name.as_bytes());
        match v {
            Value::Array(items) => {
                let elem_ty = items.first().map(type_of).unwrap_or(TYPE_STRING);
                out.push(elem_ty | ARRAY_FLAG);
                write_kv_varint(out, items.len() as u64);
                for e in items {
                    write_scalar(out, e);
                }
            }
            _ => {
                out.push(type_of(v));
                write_scalar(out, v);
            }
        }
    }
}

/// Encode a top-level section with the portable-storage header.
pub fn encode(s: &Section) -> Vec<u8> {
    let mut out = Vec::with_capacity(64);
    out.extend_from_slice(&HEADER);
    write_section(&mut out, s);
    out
}

// ---------------------------------------------------------------------------
// decoding
// ---------------------------------------------------------------------------

/// Maximum number of values (section entries plus array elements) one document
/// may decode into.
///
/// A declared count is only ever bounded by the bytes that remain, and a wire
/// byte expands to a 32-byte [`Value`] (56 for a section entry, plus its name's
/// heap allocation), so without an absolute ceiling a 100 MB Levin frame can
/// demand several gigabytes. Real messages on this network are three orders of
/// magnitude below this limit: a 600-block `NOTIFY_RESPONSE_GET_OBJECTS` is
/// roughly 60,000 values and the peer state file a few tens of thousands.
pub const MAX_VALUES: usize = 1_000_000;

/// Never pre-allocate from a peer-declared count; reserve a typical size and
/// let `push` grow the vector against data that actually arrives.
const PREALLOC_CAP: usize = 64;

/// Smallest number of wire bytes an element of `ty` can occupy, used to reject
/// a declared count that the remaining input cannot possibly satisfy.
fn min_wire_bytes(ty: u8) -> usize {
    match ty {
        TYPE_INT64 | TYPE_UINT64 | TYPE_DOUBLE => 8,
        TYPE_INT32 | TYPE_UINT32 => 4,
        TYPE_INT16 | TYPE_UINT16 => 2,
        // uint8/int8/bool are one byte; a string is at least its one-byte
        // length varint and an object at least its one-byte entry count.
        _ => 1,
    }
}

/// Smallest wire size of a section entry: one-byte name length, at least one
/// name byte, the type byte, and at least one byte of value.
const MIN_ENTRY_BYTES: usize = 4;

/// The value budget shared by every nested section and array of one document.
struct Budget(usize);

impl Budget {
    fn take(&mut self, n: usize) -> Result<()> {
        if n > self.0 {
            return Err(Error::Malformed("kv value budget"));
        }
        self.0 -= n;
        Ok(())
    }
}

fn take<'a>(input: &'a [u8], pos: &mut usize, n: usize) -> Result<&'a [u8]> {
    if *pos + n > input.len() {
        return Err(Error::Truncated);
    }
    let s = &input[*pos..*pos + n];
    *pos += n;
    Ok(s)
}

fn read_scalar(input: &[u8], pos: &mut usize, ty: u8, depth: usize, budget: &mut Budget) -> Result<Value> {
    Ok(match ty {
        TYPE_INT64 => Value::Int(i64::from_le_bytes(take(input, pos, 8)?.try_into().unwrap()), ty),
        TYPE_INT32 => Value::Int(i32::from_le_bytes(take(input, pos, 4)?.try_into().unwrap()) as i64, ty),
        TYPE_INT16 => Value::Int(i16::from_le_bytes(take(input, pos, 2)?.try_into().unwrap()) as i64, ty),
        TYPE_INT8 => Value::Int(take(input, pos, 1)?[0] as i8 as i64, ty),
        TYPE_UINT64 => Value::Uint(u64::from_le_bytes(take(input, pos, 8)?.try_into().unwrap()), ty),
        TYPE_UINT32 => Value::Uint(u32::from_le_bytes(take(input, pos, 4)?.try_into().unwrap()) as u64, ty),
        TYPE_UINT16 => Value::Uint(u16::from_le_bytes(take(input, pos, 2)?.try_into().unwrap()) as u64, ty),
        TYPE_UINT8 => Value::Uint(take(input, pos, 1)?[0] as u64, ty),
        TYPE_DOUBLE => Value::Double(f64::from_le_bytes(take(input, pos, 8)?.try_into().unwrap())),
        TYPE_STRING => {
            let n = read_kv_varint(input, pos)?;
            if n > (input.len() - *pos) as u64 {
                return Err(Error::Truncated);
            }
            Value::String(take(input, pos, n as usize)?.to_vec())
        }
        TYPE_BOOL => Value::Bool(take(input, pos, 1)?[0] != 0),
        TYPE_OBJECT => Value::Object(read_section(input, pos, depth + 1, budget)?),
        _ => return Err(Error::Malformed("kv type")),
    })
}

fn read_section(input: &[u8], pos: &mut usize, depth: usize, budget: &mut Budget) -> Result<Section> {
    if depth > MAX_DEPTH {
        return Err(Error::Malformed("kv nesting"));
    }
    let count = read_kv_varint(input, pos)?;
    // Bound the declared count by the bytes that could hold it, then charge it
    // against the document-wide budget, so nothing allocates on a peer's word.
    if count > ((input.len() - *pos) / MIN_ENTRY_BYTES) as u64 {
        return Err(Error::Truncated);
    }
    budget.take(count as usize)?;
    let mut s = Section { entries: Vec::with_capacity((count as usize).min(PREALLOC_CAP)) };
    for _ in 0..count {
        let n = take(input, pos, 1)?[0] as usize;
        if n == 0 {
            return Err(Error::Malformed("kv name"));
        }
        // Names must be valid UTF-8: `String::from_utf8_lossy` would expand a
        // 255-byte non-UTF-8 name to 765 bytes, which the 255-byte cap in
        // `write_section` would then reject on re-encode.
        let name =
            std::str::from_utf8(take(input, pos, n)?).map_err(|_| Error::Malformed("kv name encoding"))?.to_owned();
        let ty = take(input, pos, 1)?[0];
        let v = if ty & ARRAY_FLAG != 0 {
            let elem = ty & !ARRAY_FLAG;
            if elem == TYPE_ARRAY {
                return Err(Error::Malformed("kv nested array"));
            }
            let cnt = read_kv_varint(input, pos)?;
            if cnt > ((input.len() - *pos) / min_wire_bytes(elem)) as u64 {
                return Err(Error::Truncated);
            }
            budget.take(cnt as usize)?;
            let mut items = Vec::with_capacity((cnt as usize).min(PREALLOC_CAP));
            for _ in 0..cnt {
                items.push(read_scalar(input, pos, elem, depth, budget)?);
            }
            Value::Array(items)
        } else if ty == TYPE_ARRAY {
            return Err(Error::Malformed("kv bare array type"));
        } else {
            read_scalar(input, pos, ty, depth, budget)?
        };
        s.entries.push((name, v));
    }
    Ok(s)
}

/// Decode a top-level document (header + section).
///
/// Bytes after the top-level section are ignored, as the C++ reader does
/// (`KVBinaryInputStreamSerializer` stops once the root object is complete and
/// never looks at the remainder).
pub fn decode(input: &[u8]) -> Result<Section> {
    if input.len() < HEADER.len() || input[..HEADER.len()] != HEADER {
        return Err(Error::Malformed("kv header"));
    }
    let mut pos = HEADER.len();
    read_section(input, &mut pos, 0, &mut Budget(MAX_VALUES))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ping_example() {
        // 04: COMMAND_PING response { status: "OK", peer_id: 1 }
        let s = Section::new().string("status", b"OK").u64("peer_id", 1);
        let bytes = encode(&s);
        let want = "011101010101020101".to_string()
            + "08"
            + "06"
            + "737461747573"
            + "0a"
            + "08"
            + "4f4b"
            + "07"
            + "706565725f6964"
            + "05"
            + "0100000000000000";
        assert_eq!(hex::encode(&bytes), want);
        let back = decode(&bytes).unwrap();
        assert_eq!(back.get_bytes("status"), Some(&b"OK"[..]));
        assert_eq!(back.get_u64("peer_id"), Some(1));
    }

    #[test]
    fn kv_varint_sizes() {
        for v in [0u64, 63, 64, 16383, 16384, (1 << 30) - 1, 1 << 30, u64::MAX >> 2] {
            let mut out = Vec::new();
            write_kv_varint(&mut out, v);
            let mut pos = 0;
            assert_eq!(read_kv_varint(&out, &mut pos).unwrap(), v);
            assert_eq!(pos, out.len());
        }
    }

    #[test]
    fn widths_and_arrays_round_trip() {
        let s = Section::new()
            .u8("version", 19)
            .u32("port", 17855)
            .object("inner", Section::new().string("x", b"abc"))
            .string_array("txs", &[b"a".to_vec(), b"bb".to_vec()])
            .string_array("empty", &[])
            .string("emptyblob", b"");
        let bytes = encode(&s);
        let mut want = vec![0x10u8]; // 4 entries
        want.extend_from_slice(&[7, b'v', b'e', b'r', b's', b'i', b'o', b'n', TYPE_UINT8, 19]);
        want.extend_from_slice(&[4, b'p', b'o', b'r', b't', TYPE_UINT32, 0xbf, 0x45, 0, 0]);
        want.extend_from_slice(&[
            5,
            b'i',
            b'n',
            b'n',
            b'e',
            b'r',
            TYPE_OBJECT,
            0x04,
            1,
            b'x',
            TYPE_STRING,
            0x0c,
            b'a',
            b'b',
            b'c',
        ]);
        want.extend_from_slice(&[3, b't', b'x', b's', TYPE_STRING | ARRAY_FLAG, 0x08, 0x04, b'a', 0x08, b'b', b'b']);
        assert_eq!(&bytes[9..], &want[..]);
        let back = decode(&bytes).unwrap();
        assert_eq!(back.get_u64("version"), Some(19));
        assert_eq!(back.get_u64("port"), Some(17855));
        assert_eq!(back.get_object("inner").unwrap().get_bytes("x"), Some(&b"abc"[..]));
        assert_eq!(back.get_array("txs").len(), 2);
        assert!(back.get("empty").is_none());
        assert!(back.get("emptyblob").is_none());
        assert_eq!(back, s.clone().into_canonical());
        // trailing bytes are ignored, as the C++ reader does
        let mut extra = bytes.clone();
        extra.push(0);
        assert_eq!(decode(&extra).unwrap(), back);
        assert!(decode(&bytes[..bytes.len() - 1]).is_err());
        assert!(decode(b"garbage").is_err());
    }

    #[test]
    fn text_writes_an_empty_string_but_string_omits_it() {
        let s = Section::new().text("a", b"").string("b", b"");
        let back = decode(&encode(&s)).unwrap();
        assert_eq!(back.get_bytes("a"), Some(&[][..]));
        assert!(back.get("b").is_none());
    }

    /// One object per level; the 33rd is rejected before it can recurse.
    #[test]
    fn nesting_cap() {
        fn nest(levels: usize) -> Vec<u8> {
            let mut inner = Section::new().u8("x", 1);
            for _ in 0..levels {
                inner = Section::new().object("o", inner);
            }
            encode(&inner)
        }
        assert!(decode(&nest(MAX_DEPTH - 1)).is_ok());
        assert_eq!(decode(&nest(MAX_DEPTH + 1)), Err(Error::Malformed("kv nesting")));
    }

    #[test]
    fn rejects_non_utf8_name() {
        let mut doc = HEADER.to_vec();
        doc.push(0x04); // one entry
        doc.extend_from_slice(&[2, 0xff, 0xfe, TYPE_UINT8, 7]);
        assert_eq!(decode(&doc), Err(Error::Malformed("kv name encoding")));
    }

    /// A declared count far beyond what the remaining bytes can hold must be
    /// refused before anything is allocated.
    #[test]
    fn rejects_oversized_counts() {
        // array of 2^28 uint64 elements in a 20-byte document
        let mut doc = HEADER.to_vec();
        doc.push(0x04); // one entry
        doc.extend_from_slice(&[1, b'a', TYPE_UINT64 | ARRAY_FLAG]);
        write_kv_varint(&mut doc, 1 << 28);
        assert_eq!(decode(&doc), Err(Error::Truncated));

        // section claiming 2^28 entries
        let mut doc = HEADER.to_vec();
        write_kv_varint(&mut doc, 1 << 28);
        assert_eq!(decode(&doc), Err(Error::Truncated));
    }

    /// A count the remaining bytes *could* hold, but that would blow the
    /// document-wide value budget, is refused too.
    #[test]
    fn enforces_value_budget() {
        let mut doc = HEADER.to_vec();
        doc.push(0x04); // one entry
        doc.extend_from_slice(&[1, b'a', TYPE_UINT8 | ARRAY_FLAG]);
        let cnt = (MAX_VALUES + 1) as u64;
        write_kv_varint(&mut doc, cnt);
        doc.resize(doc.len() + cnt as usize, 0); // the elements really are there
        assert_eq!(decode(&doc), Err(Error::Malformed("kv value budget")));
    }

    impl Section {
        /// Drop entries the writer never emits, for equality checks in tests.
        fn into_canonical(self) -> Section {
            self
        }
    }
}

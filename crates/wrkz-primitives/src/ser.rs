// Copyright (c) 2026, The WrkzCoin developers
//
// Please see the included LICENSE file for more information

//! The binary serializer of `BinaryOutputStreamSerializer` /
//! `BinaryInputStreamSerializer` (spec/04-serialization.md "Binary format").
//!
//! Integers are varints, `bool` one byte, strings varint-length-prefixed,
//! fixed PODs raw, arrays a varint count followed by the elements.
//! Deserialization must consume the whole buffer; [`Reader::finish`] enforces it.

use crate::{varint, Error, Result};

#[derive(Default)]
pub struct Writer {
    buf: Vec<u8>,
}

impl Writer {
    pub fn new() -> Self {
        Self::default()
    }

    /// Bytes written so far (used for the 2048-byte parent-block size rule).
    pub fn len(&self) -> usize {
        self.buf.len()
    }

    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    pub fn varint(&mut self, v: u64) -> &mut Self {
        varint::write(&mut self.buf, v);
        self
    }

    pub fn u8_raw(&mut self, v: u8) -> &mut Self {
        self.buf.push(v);
        self
    }

    pub fn bool(&mut self, v: bool) -> &mut Self {
        self.buf.push(v as u8);
        self
    }

    /// `binary(ptr, size)`: raw bytes, no length.
    pub fn raw(&mut self, bytes: &[u8]) -> &mut Self {
        self.buf.extend_from_slice(bytes);
        self
    }

    /// `binary(std::string)`: varint length, then bytes.
    pub fn bytes(&mut self, bytes: &[u8]) -> &mut Self {
        self.varint(bytes.len() as u64);
        self.buf.extend_from_slice(bytes);
        self
    }

    /// Raw little-endian u32 (the block nonce).
    pub fn u32_le(&mut self, v: u32) -> &mut Self {
        self.buf.extend_from_slice(&v.to_le_bytes());
        self
    }

    pub fn into_inner(self) -> Vec<u8> {
        self.buf
    }
}

pub struct Reader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    pub fn position(&self) -> usize {
        self.pos
    }

    pub fn remaining(&self) -> usize {
        self.data.len() - self.pos
    }

    pub fn varint(&mut self) -> Result<u64> {
        self.varint_bits(64)
    }

    /// Varint into a narrower integer type, rejecting overflow like the C++ reader.
    pub fn varint_bits(&mut self, bits: u32) -> Result<u64> {
        let (v, n) = varint::read_bits(&self.data[self.pos..], bits)?;
        self.pos += n;
        Ok(v)
    }

    pub fn u8_raw(&mut self) -> Result<u8> {
        let b = *self.data.get(self.pos).ok_or(Error::Truncated)?;
        self.pos += 1;
        Ok(b)
    }

    pub fn bool(&mut self) -> Result<bool> {
        match self.u8_raw()? {
            0 => Ok(false),
            1 => Ok(true),
            _ => Err(Error::Malformed("bool")),
        }
    }

    pub fn raw(&mut self, n: usize) -> Result<&'a [u8]> {
        if self.remaining() < n {
            return Err(Error::Truncated);
        }
        let s = &self.data[self.pos..self.pos + n];
        self.pos += n;
        Ok(s)
    }

    pub fn hash(&mut self) -> Result<[u8; 32]> {
        Ok(self.raw(32)?.try_into().unwrap())
    }

    pub fn u32_le(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(self.raw(4)?.try_into().unwrap()))
    }

    /// Varint-length-prefixed byte string. The length is checked against the
    /// remaining input before allocation.
    pub fn bytes(&mut self) -> Result<&'a [u8]> {
        let n = self.varint()?;
        if n > self.remaining() as u64 {
            return Err(Error::Truncated);
        }
        self.raw(n as usize)
    }

    /// Array count with a sanity bound against the remaining bytes (each
    /// element takes at least `min_elem_size` bytes) so a hostile count cannot
    /// trigger a huge allocation.
    pub fn count(&mut self, min_elem_size: usize) -> Result<usize> {
        let n = self.varint()?;
        if n > (self.remaining() / min_elem_size.max(1)) as u64 {
            return Err(Error::Truncated);
        }
        Ok(n as usize)
    }

    /// Require that the whole buffer was consumed (`fromBinaryArray`).
    pub fn finish(self) -> Result<()> {
        if self.pos == self.data.len() {
            Ok(())
        } else {
            Err(Error::TrailingBytes(self.data.len() - self.pos))
        }
    }
}

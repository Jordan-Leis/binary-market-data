//! Byte-level primitives: bounds-checked reader and LEB128 varints.
//!
//! The reader never panics on hostile input. Every read returns `Err` on
//! truncation, and varints reject both overflow and non-minimal encodings so
//! that a value has exactly one wire form.

use crate::error::Error;

pub struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    pub fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    pub fn remaining(&self) -> usize {
        self.buf.len() - self.pos
    }

    pub fn is_empty(&self) -> bool {
        self.remaining() == 0
    }

    pub fn u8(&mut self) -> Result<u8, Error> {
        let b = *self.buf.get(self.pos).ok_or(Error::Truncated)?;
        self.pos += 1;
        Ok(b)
    }

    pub fn bytes(&mut self, n: usize) -> Result<&'a [u8], Error> {
        if n > self.remaining() {
            return Err(Error::Truncated);
        }
        let s = &self.buf[self.pos..self.pos + n];
        self.pos += n;
        Ok(s)
    }

    pub fn array<const N: usize>(&mut self) -> Result<[u8; N], Error> {
        let s = self.bytes(N)?;
        let mut a = [0u8; N];
        a.copy_from_slice(s);
        Ok(a)
    }

    pub fn varint(&mut self) -> Result<u64, Error> {
        let v = self.varint128()?;
        u64::try_from(v).map_err(|_| Error::VarintOverflow)
    }

    pub fn varint128(&mut self) -> Result<u128, Error> {
        let mut v: u128 = 0;
        let mut shift = 0u32;
        loop {
            let b = self.u8()?;
            let low = (b & 0x7f) as u128;
            if shift >= 128 || (shift > 121 && low >> (128 - shift) != 0) {
                return Err(Error::VarintOverflow);
            }
            v |= low << shift;
            if b & 0x80 == 0 {
                if b == 0 && shift != 0 {
                    return Err(Error::NonMinimalVarint);
                }
                return Ok(v);
            }
            shift += 7;
        }
    }

    pub fn zigzag(&mut self) -> Result<i64, Error> {
        let v = self.varint()?;
        Ok(unzig64(v))
    }

    pub fn zigzag128(&mut self) -> Result<i128, Error> {
        let v = self.varint128()?;
        Ok(unzig128(v))
    }

    /// A count that is about to size an allocation. Each element needs at
    /// least `min_bytes` on the wire, so a claimed count that could not fit in
    /// the remaining input is rejected before anything is allocated.
    pub fn count(&mut self, max: usize, min_bytes: usize) -> Result<usize, Error> {
        let n = self.varint()?;
        if n > max as u64 {
            return Err(Error::CountTooLarge);
        }
        let n = n as usize;
        if n.saturating_mul(min_bytes) > self.remaining() {
            return Err(Error::Truncated);
        }
        Ok(n)
    }
}

pub fn put_varint(out: &mut Vec<u8>, mut v: u64) {
    while v >= 0x80 {
        out.push((v as u8) | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

pub fn put_varint128(out: &mut Vec<u8>, mut v: u128) {
    while v >= 0x80 {
        out.push((v as u8) | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

pub fn put_zigzag(out: &mut Vec<u8>, v: i64) {
    put_varint(out, zig64(v));
}

pub fn put_zigzag128(out: &mut Vec<u8>, v: i128) {
    put_varint128(out, zig128(v));
}

pub fn varint_len(mut v: u64) -> usize {
    let mut n = 1;
    while v >= 0x80 {
        v >>= 7;
        n += 1;
    }
    n
}

#[inline]
pub fn zig64(v: i64) -> u64 {
    ((v << 1) ^ (v >> 63)) as u64
}

#[inline]
pub fn unzig64(v: u64) -> i64 {
    ((v >> 1) as i64) ^ -((v & 1) as i64)
}

#[inline]
pub fn zig128(v: i128) -> u128 {
    ((v << 1) ^ (v >> 127)) as u128
}

#[inline]
pub fn unzig128(v: u128) -> i128 {
    ((v >> 1) as i128) ^ -((v & 1) as i128)
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    proptest! {
        #[test]
        fn varint_round_trip(v: u64) {
            let mut b = vec![];
            put_varint(&mut b, v);
            prop_assert_eq!(b.len(), varint_len(v));
            let mut r = Reader::new(&b);
            prop_assert_eq!(r.varint().unwrap(), v);
            prop_assert!(r.is_empty());
        }

        #[test]
        fn varint128_round_trip(v: u128) {
            let mut b = vec![];
            put_varint128(&mut b, v);
            let mut r = Reader::new(&b);
            prop_assert_eq!(r.varint128().unwrap(), v);
        }

        #[test]
        fn zigzag_round_trip(v: i64, w: i128) {
            prop_assert_eq!(unzig64(zig64(v)), v);
            prop_assert_eq!(unzig128(zig128(w)), w);
        }
    }

    #[test]
    fn rejects_overflow_and_non_minimal() {
        // 2^64 does not fit in u64.
        let mut b = vec![];
        put_varint128(&mut b, 1u128 << 64);
        assert_eq!(Reader::new(&b).varint(), Err(Error::VarintOverflow));
        // 19 bytes of continuation overflows u128.
        assert_eq!(Reader::new(&[0xff; 20]).varint128(), Err(Error::VarintOverflow));
        // 0x80 0x00 is a non-minimal encoding of zero.
        assert_eq!(Reader::new(&[0x80, 0x00]).varint(), Err(Error::NonMinimalVarint));
        // Top byte of a 19-byte varint may only carry two more bits.
        let mut over = vec![0xffu8; 18];
        over.push(0x7f);
        assert_eq!(Reader::new(&over).varint128(), Err(Error::VarintOverflow));
    }

    #[test]
    fn count_is_checked_against_remaining_input() {
        let mut b = vec![];
        put_varint(&mut b, 1000);
        assert_eq!(Reader::new(&b).count(4096, 2), Err(Error::Truncated));
        assert_eq!(Reader::new(&b).count(10, 2), Err(Error::CountTooLarge));
    }
}

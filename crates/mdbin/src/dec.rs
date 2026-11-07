//! Exact fixed-point decimals. No floats anywhere in this crate.
//!
//! A `Dec` keeps the mantissa and the number of fractional digits exactly as
//! they appeared in the JSON string, so `"1.50"` and `"1.5"` stay different
//! values on the wire and format back to the same text they came from.

use crate::error::Error;
use crate::wire::{put_varint, put_varint128, unzig128, zig128, Reader};

/// 4 bits carry the scale. The public API documents up to 12 fractional
/// digits, so 15 leaves headroom without widening the encoding.
pub const MAX_SCALE: u8 = 15;

/// 36 digits keeps `zigzag(m) << 4 | s` inside a u128, so `put_dec` has no
/// failure path. Longer values are forwarded as raw JSON instead.
pub const MAX_DIGITS: usize = 36;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Dec {
    pub m: i128,
    pub s: u8,
}

const POW10: [i128; 16] = {
    let mut t = [1i128; 16];
    let mut i = 1;
    while i < 16 {
        t[i] = t[i - 1] * 10;
        i += 1;
    }
    t
};

impl Dec {
    pub const ZERO: Dec = Dec { m: 0, s: 0 };

    /// Accepts `-?(0|[1-9][0-9]*)(\.[0-9]+)?`, which is the only form that
    /// round-trips byte for byte. Anything else, such as `+1`, `01`, `1.`,
    /// `1e5`, or `-0`, is refused rather than silently normalised.
    pub fn parse(text: &str) -> Result<Dec, Error> {
        let b = text.as_bytes();
        let (neg, digits) = match b.first() {
            Some(b'-') => (true, &b[1..]),
            _ => (false, b),
        };
        let dot = digits.iter().position(|&c| c == b'.');
        let (int, frac) = match dot {
            Some(i) => (&digits[..i], &digits[i + 1..]),
            None => (digits, &[][..]),
        };
        if int.is_empty() || (dot.is_some() && frac.is_empty()) {
            return Err(Error::BadDecimal);
        }
        if int.len() > 1 && int[0] == b'0' {
            return Err(Error::BadDecimal);
        }
        if frac.len() > MAX_SCALE as usize {
            return Err(Error::BadScale);
        }
        if int.len() + frac.len() > MAX_DIGITS {
            return Err(Error::DecimalOverflow);
        }
        let mut m: i128 = 0;
        if int.len() + frac.len() <= 18 {
            // Stay in 64-bit arithmetic for the common case.
            let mut f: u64 = 0;
            for &c in int.iter().chain(frac) {
                if !c.is_ascii_digit() {
                    return Err(Error::BadDecimal);
                }
                f = f * 10 + (c - b'0') as u64;
            }
            m = f as i128;
        } else {
            for &c in int.iter().chain(frac) {
                if !c.is_ascii_digit() {
                    return Err(Error::BadDecimal);
                }
                m = m * 10 + (c - b'0') as i128;
            }
        }
        if neg {
            if m == 0 {
                return Err(Error::BadDecimal);
            }
            m = -m;
        }
        Ok(Dec { m, s: frac.len() as u8 })
    }

    pub fn write_to(&self, out: &mut String) {
        let neg = self.m < 0;
        let mut buf = [0u8; 48];
        let mut i = buf.len();
        let mut mag = self.m.unsigned_abs();
        let mut frac_left = self.s as usize;
        // Emit digits right to left, placing the point after `s` digits.
        if mag <= u64::MAX as u128 {
            let mut v = mag as u64;
            while v > 0 || frac_left > 0 || i == buf.len() {
                i -= 1;
                buf[i] = b'0' + (v % 10) as u8;
                v /= 10;
                if frac_left > 0 {
                    frac_left -= 1;
                    if frac_left == 0 {
                        i -= 1;
                        buf[i] = b'.';
                        if v == 0 {
                            i -= 1;
                            buf[i] = b'0';
                            break;
                        }
                    }
                }
            }
        } else {
            while mag > 0 || frac_left > 0 {
                i -= 1;
                buf[i] = b'0' + (mag % 10) as u8;
                mag /= 10;
                if frac_left > 0 {
                    frac_left -= 1;
                    if frac_left == 0 {
                        i -= 1;
                        buf[i] = b'.';
                        if mag == 0 {
                            i -= 1;
                            buf[i] = b'0';
                            break;
                        }
                    }
                }
            }
        }
        if neg {
            out.push('-');
        }
        // Safe: only ASCII digits and '.' were written.
        out.push_str(std::str::from_utf8(&buf[i..]).unwrap_or(""));
    }

    pub fn to_text(&self) -> String {
        let mut s = String::with_capacity(24);
        self.write_to(&mut s);
        s
    }

    /// The mantissa at a larger scale, or `None` on overflow.
    pub fn at_scale(&self, s: u8) -> Option<i128> {
        if s < self.s || s > MAX_SCALE {
            return None;
        }
        self.m.checked_mul(POW10[(s - self.s) as usize])
    }

    pub fn is_trimmed(&self) -> bool {
        self.s == 0 || self.m % 10 != 0
    }

    pub fn trimmed(mut self) -> Dec {
        while self.s > 0 && self.m % 10 == 0 {
            self.m /= 10;
            self.s -= 1;
        }
        self
    }

    /// Numeric comparison across scales; `None` only if the common scale
    /// would overflow, which the caller treats as unrepresentable.
    pub fn cmp_num(&self, o: &Dec) -> Option<std::cmp::Ordering> {
        let s = self.s.max(o.s);
        Some(self.at_scale(s)?.cmp(&o.at_scale(s)?))
    }

    /// Scale 18 fixed point as used by the signed order words. Fails rather
    /// than truncates on anything the signature could not cover exactly.
    pub fn to_e18(&self) -> Option<i128> {
        self.m.checked_mul(10i128.checked_pow(18u32.checked_sub(self.s as u32)?)?)
    }
}

/// One self-describing value: `zigzag(m) << 4 | s`.
pub fn put_dec(out: &mut Vec<u8>, d: Dec) {
    let z = zig128(d.m);
    if z >> 60 == 0 {
        put_varint(out, ((z as u64) << 4) | d.s as u64);
    } else {
        put_varint128(out, (z << 4) | d.s as u128);
    }
}

pub fn get_dec(r: &mut Reader<'_>) -> Result<Dec, Error> {
    let v = r.varint128()?;
    let s = (v & 0xf) as u8;
    let m = unzig128(v >> 4);
    Ok(Dec { m, s })
}

/// How a run of values shares a scale.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColumnMode {
    /// Every value has exactly `scale` fractional digits.
    Fixed,
    /// Every value is written with trailing zeros trimmed; mantissas are
    /// stored at `scale` and trimmed again on decode.
    Trim,
    /// Mixed text forms: each value carries its own scale.
    PerValue,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Column {
    pub mode: ColumnMode,
    pub scale: u8,
}

impl Column {
    pub fn choose<'a>(vals: impl Iterator<Item = &'a Dec> + Clone) -> Column {
        let scale = vals.clone().map(|d| d.s).max().unwrap_or(0);
        let fixed = vals.clone().all(|d| d.s == scale);
        if fixed {
            return Column { mode: ColumnMode::Fixed, scale };
        }
        let trim = vals.clone().all(|d| d.is_trimmed());
        let fits = vals.clone().all(|d| d.at_scale(scale).is_some());
        if trim && fits {
            Column { mode: ColumnMode::Trim, scale }
        } else {
            Column { mode: ColumnMode::PerValue, scale: 0 }
        }
    }

    pub fn byte(&self) -> u8 {
        let mode = match self.mode {
            ColumnMode::Fixed => 0u8,
            ColumnMode::Trim => 1,
            ColumnMode::PerValue => 2,
        };
        (mode << 4) | self.scale
    }

    pub fn from_byte(b: u8) -> Result<Column, Error> {
        let mode = match b >> 4 {
            0 => ColumnMode::Fixed,
            1 => ColumnMode::Trim,
            2 => ColumnMode::PerValue,
            _ => return Err(Error::BadEnum),
        };
        Ok(Column { mode, scale: b & 0xf })
    }

    /// Mantissa as stored in this column, for columns with a shared scale.
    pub fn store(&self, d: &Dec) -> Option<i128> {
        d.at_scale(self.scale)
    }

    pub fn load(&self, m: i128) -> Dec {
        let d = Dec { m, s: self.scale };
        match self.mode {
            ColumnMode::Trim => d.trimmed(),
            _ => d,
        }
    }

    pub fn shared(&self) -> bool {
        self.mode != ColumnMode::PerValue
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn rt(s: &str) {
        assert_eq!(Dec::parse(s).unwrap().to_text(), s, "{s}");
    }

    #[test]
    fn text_round_trips() {
        for s in [
            "0", "0.0", "0.5", "0.50", "1", "1.5", "1.50", "10", "100.000", "3100.5", "-1.5", "-0.001",
            "0.000000000001", "123456789012345678901234.123456789012", "999999999999999999999999999999999999",
            "18446744073709551615", "18446744073709551616", "0.000000000000000",
        ] {
            rt(s);
        }
    }

    #[test]
    fn refuses_non_canonical_text() {
        for s in ["", "-", ".", ".5", "5.", "+1", "01", "00", "-0", "-0.0", "1e5", "1 ", " 1", "1.2.3", "0x10", "NaN", "١"] {
            assert!(Dec::parse(s).is_err(), "{s:?}");
        }
        assert_eq!(Dec::parse("0.1234567890123456"), Err(Error::BadScale));
        assert_eq!(Dec::parse(&"9".repeat(37)), Err(Error::DecimalOverflow));
    }

    #[test]
    fn e18_is_exact_or_refused() {
        assert_eq!(Dec::parse("310").unwrap().to_e18(), Some(310_000_000_000_000_000_000));
        assert_eq!(Dec::parse("0.01").unwrap().to_e18(), Some(10_000_000_000_000_000));
        assert_eq!(Dec::parse("310.0").unwrap().to_e18(), Dec::parse("310").unwrap().to_e18());
        assert_eq!(Dec::parse(&"9".repeat(36)).unwrap().to_e18(), None);
    }

    #[test]
    fn numeric_compare_crosses_scales() {
        use std::cmp::Ordering::*;
        let d = |s| Dec::parse(s).unwrap();
        assert_eq!(d("1.5").cmp_num(&d("1.50")), Some(Equal));
        assert_eq!(d("1.49").cmp_num(&d("1.5")), Some(Less));
        assert_eq!(d("-1").cmp_num(&d("0.1")), Some(Less));
    }

    prop_compose! {
        fn dec_text()(neg: bool, int in 0u128..10u128.pow(21), frac in proptest::option::of("[0-9]{1,15}")) -> String {
            let mut s = String::new();
            if neg { s.push('-'); }
            s.push_str(&int.to_string());
            if let Some(f) = frac { s.push('.'); s.push_str(&f); }
            s
        }
    }

    proptest! {
        #[test]
        fn any_canonical_text_round_trips(s in dec_text()) {
            match Dec::parse(&s) {
                Ok(d) => {
                    prop_assert_eq!(d.to_text(), s.clone());
                    let mut b = vec![];
                    put_dec(&mut b, d);
                    let mut r = Reader::new(&b);
                    let e = get_dec(&mut r).unwrap();
                    prop_assert_eq!(e, d);
                    prop_assert_eq!(e.to_text(), s);
                }
                Err(e) => {
                    // The only canonical-looking input refused is negative zero.
                    prop_assert_eq!(e, Error::BadDecimal);
                    prop_assert!(s.starts_with('-') && s.trim_start_matches('-').chars().all(|c| c == '0' || c == '.'));
                }
            }
        }

        #[test]
        fn any_string_never_panics(s in ".*") {
            let _ = Dec::parse(&s);
        }

        #[test]
        fn column_load_inverts_store(texts in proptest::collection::vec(dec_text(), 1..20)) {
            let ds: Vec<Dec> = texts.iter().filter_map(|t| Dec::parse(t).ok()).collect();
            let c = Column::choose(ds.iter());
            if c.shared() {
                for d in &ds {
                    prop_assert_eq!(c.load(c.store(d).unwrap()), *d);
                }
            }
        }
    }
}

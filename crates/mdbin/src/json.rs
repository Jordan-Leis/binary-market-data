//! The JSON side of the bridge: strict parsing of the documented public
//! payloads, and rendering back to the same text.
//!
//! Parsing is deliberately strict. Unknown keys, missing keys, duplicate keys
//! and non-canonical numbers are all errors, because the transcoder's answer
//! to anything it cannot carry exactly is to forward the original text. An
//! input is never reshaped to fit.

use crate::dec::Dec;
use crate::error::Error;
use crate::msg::*;
use serde::{Deserialize, Deserializer};
use serde_json::value::RawValue;
use sha3::{Digest, Keccak256};
use std::fmt::Write;

pub const GROUPS: [u32; 3] = [1, 10, 100];
pub const DEPTHS: [u32; 4] = [1, 10, 20, 100];
pub const INTERVALS: [u32; 2] = [100, 1000];

pub fn book_chan(group: u32, depth: u32) -> Option<u8> {
    let g = GROUPS.iter().position(|&x| x == group)? as u8;
    let d = DEPTHS.iter().position(|&x| x == depth)? as u8;
    Some((g << 2) | d)
}

pub fn book_chan_parts(chan: u8) -> Result<(u32, u32), Error> {
    let g = GROUPS.get((chan >> 2) as usize).ok_or(Error::BadChannel)?;
    let d = DEPTHS.get((chan & 3) as usize).ok_or(Error::BadChannel)?;
    Ok((*g, *d))
}

pub fn ticker_chan(interval: u32) -> Option<u8> {
    INTERVALS.iter().position(|&x| x == interval).map(|i| i as u8)
}

fn valid_instrument(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name.bytes().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == b'-' || c == b'_')
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Notification<'a> {
    #[serde(borrow)]
    method: &'a str,
    #[serde(borrow)]
    params: Params<'a>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Params<'a> {
    #[serde(borrow)]
    channel: &'a str,
    #[serde(borrow)]
    data: &'a RawValue,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BookJson<'a> {
    #[serde(borrow)]
    asks: Vec<[&'a str; 2]>,
    #[serde(borrow)]
    bids: Vec<[&'a str; 2]>,
    #[serde(borrow)]
    instrument_name: &'a str,
    publish_id: u64,
    timestamp: i64,
}

/// `Option` that distinguishes JSON null (accepted) from a missing key
/// (a parse error). A missing key could not be rendered back identically.
fn required_opt_str<'de: 'a, 'a, D: Deserializer<'de>>(d: D) -> Result<Option<&'a str>, D::Error> {
    Option::<&'a str>::deserialize(d)
}

fn required_opt_pricing<'de: 'a, 'a, D: Deserializer<'de>>(d: D) -> Result<Option<PricingJson<'a>>, D::Error> {
    Option::<PricingJson<'a>>::deserialize(d)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TickerPayload<'a> {
    #[serde(borrow)]
    instrument_ticker: TickerJson<'a>,
    timestamp: i64,
}

#[allow(non_snake_case)]
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TickerJson<'a> {
    #[serde(borrow)]
    A: &'a str,
    #[serde(borrow)]
    B: &'a str,
    #[serde(borrow)]
    I: &'a str,
    #[serde(borrow)]
    M: &'a str,
    #[serde(borrow)]
    a: &'a str,
    #[serde(borrow)]
    b: &'a str,
    #[serde(borrow, deserialize_with = "required_opt_str")]
    f: Option<&'a str>,
    #[serde(borrow)]
    maxp: &'a str,
    #[serde(borrow)]
    minp: &'a str,
    #[serde(borrow, deserialize_with = "required_opt_pricing")]
    option_pricing: Option<PricingJson<'a>>,
    #[serde(borrow)]
    stats: StatsJson<'a>,
    t: i64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StatsJson<'a> {
    #[serde(borrow)]
    c: &'a str,
    #[serde(borrow)]
    h: &'a str,
    #[serde(borrow)]
    l: &'a str,
    n: u64,
    #[serde(borrow)]
    oi: &'a str,
    #[serde(borrow)]
    p: &'a str,
    #[serde(borrow)]
    pr: &'a str,
    #[serde(borrow)]
    v: &'a str,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PricingJson<'a> {
    #[serde(borrow)]
    ai: &'a str,
    #[serde(borrow)]
    bi: &'a str,
    #[serde(borrow)]
    d: &'a str,
    #[serde(borrow)]
    df: &'a str,
    #[serde(borrow)]
    f: &'a str,
    #[serde(borrow)]
    g: &'a str,
    #[serde(borrow)]
    i: &'a str,
    #[serde(borrow)]
    m: &'a str,
    #[serde(borrow)]
    r: &'a str,
    #[serde(borrow)]
    t: &'a str,
    #[serde(borrow)]
    v: &'a str,
}

fn required_opt_id<'de: 'a, 'a, D: Deserializer<'de>>(d: D) -> Result<Option<&'a str>, D::Error> {
    Option::<&'a str>::deserialize(d)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TradeJson<'a> {
    #[serde(borrow)]
    direction: &'a str,
    #[serde(borrow)]
    expected_rebate: &'a str,
    #[serde(borrow)]
    extra_fee: &'a str,
    #[serde(borrow)]
    index_price: &'a str,
    #[serde(borrow)]
    instrument_name: &'a str,
    #[serde(borrow)]
    liquidity_role: &'a str,
    #[serde(borrow)]
    mark_price: &'a str,
    #[serde(borrow, deserialize_with = "required_opt_id")]
    quote_id: Option<&'a str>,
    #[serde(borrow)]
    realized_pnl: &'a str,
    #[serde(borrow)]
    realized_pnl_excl_fees: &'a str,
    #[serde(borrow, deserialize_with = "required_opt_id")]
    rfq_id: Option<&'a str>,
    subaccount_id: i64,
    timestamp: i64,
    #[serde(borrow)]
    trade_amount: &'a str,
    #[serde(borrow)]
    trade_fee: &'a str,
    #[serde(borrow)]
    trade_id: &'a str,
    #[serde(borrow)]
    trade_price: &'a str,
    #[serde(borrow)]
    wallet: &'a str,
}

/// A parsed public notification, with the values already in fixed point.
pub enum Parsed<'a> {
    Book { instrument: &'a str, timestamp: i64, book: BookSnapshot },
    Ticker { instrument: &'a str, chan: u8, timestamp: i64, state: Box<TickerState> },
    Trades { instrument: &'a str, trades: Vec<Trade> },
}

fn dec(s: &str) -> Result<Dec, Error> {
    Dec::parse(s).map_err(|_| Error::NotRepresentable)
}

fn levels(raw: &[[&str; 2]]) -> Result<Vec<Level>, Error> {
    raw.iter().map(|[p, q]| Ok(Level { price: dec(p)?, qty: dec(q)? })).collect()
}

/// Parses a subscription notification. Any `Err` means "forward the original
/// text", not "the feed is broken".
pub fn parse_notification(json: &str) -> Result<Parsed<'_>, Error> {
    let n: Notification<'_> = serde_json::from_str(json).map_err(|_| Error::NotRepresentable)?;
    if n.method != "subscription" {
        return Err(Error::NotRepresentable);
    }
    let channel = n.params.channel;
    let mut parts = channel.split('.');
    let kind = parts.next().ok_or(Error::NotRepresentable)?;
    let instrument = parts.next().ok_or(Error::NotRepresentable)?;
    if !valid_instrument(instrument) {
        return Err(Error::NotRepresentable);
    }
    let data = n.params.data.get();
    match kind {
        "orderbook" => {
            let group: u32 = parts.next().and_then(|s| s.parse().ok()).ok_or(Error::NotRepresentable)?;
            let depth: u32 = parts.next().and_then(|s| s.parse().ok()).ok_or(Error::NotRepresentable)?;
            if parts.next().is_some() {
                return Err(Error::NotRepresentable);
            }
            let chan = book_chan(group, depth).ok_or(Error::NotRepresentable)?;
            let b: BookJson<'_> = serde_json::from_str(data).map_err(|_| Error::NotRepresentable)?;
            if b.instrument_name != instrument {
                return Err(Error::NotRepresentable);
            }
            let book = BookSnapshot { chan, publish_id: b.publish_id, bids: levels(&b.bids)?, asks: levels(&b.asks)? };
            Ok(Parsed::Book { instrument, timestamp: b.timestamp, book })
        }
        "ticker_slim" => {
            let interval: u32 = parts.next().and_then(|s| s.parse().ok()).ok_or(Error::NotRepresentable)?;
            if parts.next().is_some() {
                return Err(Error::NotRepresentable);
            }
            let chan = ticker_chan(interval).ok_or(Error::NotRepresentable)?;
            let p: TickerPayload<'_> = serde_json::from_str(data).map_err(|_| Error::NotRepresentable)?;
            let k = &p.instrument_ticker;
            let mut fields = [None; TICKER_N];
            let required = [
                k.a, k.A, k.b, k.B, k.M, k.I,
            ];
            for (i, s) in required.iter().enumerate() {
                fields[i] = Some(dec(s)?);
            }
            fields[6] = k.f.map(dec).transpose()?;
            fields[7] = Some(dec(k.minp)?);
            fields[8] = Some(dec(k.maxp)?);
            let st = &k.stats;
            fields[9] = Some(dec(st.c)?);
            fields[10] = Some(dec(st.h)?);
            fields[11] = Some(dec(st.l)?);
            if st.n > i64::MAX as u64 {
                return Err(Error::NotRepresentable);
            }
            fields[12] = Some(Dec { m: st.n as i128, s: 0 });
            fields[13] = Some(dec(st.oi)?);
            fields[14] = Some(dec(st.p)?);
            fields[15] = Some(dec(st.pr)?);
            fields[16] = Some(dec(st.v)?);
            let pricing_null = k.option_pricing.is_none();
            if let Some(o) = &k.option_pricing {
                for (i, s) in [o.ai, o.bi, o.d, o.df, o.f, o.g, o.i, o.m, o.r, o.t, o.v].iter().enumerate() {
                    fields[TICKER_PRICING_START + i] = Some(dec(s)?);
                }
            }
            let state = Box::new(TickerState { fields, pricing_null, t: k.t });
            Ok(Parsed::Ticker { instrument, chan, timestamp: p.timestamp, state })
        }
        "trades" => {
            if parts.next().is_some() {
                return Err(Error::NotRepresentable);
            }
            let raw: Vec<TradeJson<'_>> = serde_json::from_str(data).map_err(|_| Error::NotRepresentable)?;
            if raw.is_empty() || raw.len() > MAX_TRADES {
                return Err(Error::NotRepresentable);
            }
            let mut trades = Vec::with_capacity(raw.len());
            for t in &raw {
                if t.instrument_name != instrument {
                    return Err(Error::NotRepresentable);
                }
                trades.push(Trade {
                    is_buy: match t.direction {
                        "buy" => true,
                        "sell" => false,
                        _ => return Err(Error::NotRepresentable),
                    },
                    is_maker: match t.liquidity_role {
                        "maker" => true,
                        "taker" => false,
                        _ => return Err(Error::NotRepresentable),
                    },
                    expected_rebate: dec(t.expected_rebate)?,
                    extra_fee: dec(t.extra_fee)?,
                    index_price: dec(t.index_price)?,
                    mark_price: dec(t.mark_price)?,
                    quote_id: t.quote_id.map(parse_id).transpose()?,
                    realized_pnl: dec(t.realized_pnl)?,
                    realized_pnl_excl_fees: dec(t.realized_pnl_excl_fees)?,
                    rfq_id: t.rfq_id.map(parse_id).transpose()?,
                    subaccount_id: t.subaccount_id,
                    timestamp: t.timestamp,
                    trade_amount: dec(t.trade_amount)?,
                    trade_fee: dec(t.trade_fee)?,
                    trade_id: parse_id(t.trade_id)?,
                    trade_price: dec(t.trade_price)?,
                    wallet: parse_addr(t.wallet)?,
                });
            }
            Ok(Parsed::Trades { instrument, trades })
        }
        _ => Err(Error::NotRepresentable),
    }
}

fn hex_val(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

pub fn parse_hex_into(text: &[u8], out: &mut [u8]) -> Result<(), Error> {
    if text.len() != out.len() * 2 {
        return Err(Error::BadHex);
    }
    for (i, pair) in text.chunks_exact(2).enumerate() {
        let h = hex_val(pair[0]).ok_or(Error::BadHex)?;
        let l = hex_val(pair[1]).ok_or(Error::BadHex)?;
        out[i] = (h << 4) | l;
    }
    Ok(())
}

const HEX: &[u8; 16] = b"0123456789abcdef";

pub fn push_hex(out: &mut String, bytes: &[u8]) {
    for b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 15) as usize] as char);
    }
}

/// EIP-55 checksummed text for a 20-byte address, without the `0x`.
pub fn eip55(addr: &[u8; 20]) -> String {
    let mut lower = String::with_capacity(40);
    push_hex(&mut lower, addr);
    let hash = Keccak256::digest(lower.as_bytes());
    lower
        .bytes()
        .enumerate()
        .map(|(i, c)| {
            let nibble = (hash[i / 2] >> (if i % 2 == 0 { 4 } else { 0 })) & 15;
            if c.is_ascii_alphabetic() && nibble >= 8 {
                c.to_ascii_uppercase() as char
            } else {
                c as char
            }
        })
        .collect()
}

/// An address keeps its text form: lowercase or EIP-55. Any other casing
/// cannot be rebuilt from 20 bytes, so it is refused.
pub fn parse_addr(s: &str) -> Result<Addr, Error> {
    let t = s.strip_prefix("0x").ok_or(Error::NotRepresentable)?;
    let mut b = [0u8; 20];
    parse_hex_into(t.as_bytes(), &mut b).map_err(|_| Error::NotRepresentable)?;
    if !t.bytes().any(|c| c.is_ascii_uppercase()) {
        return Ok(Addr::Lower(b));
    }
    if eip55(&b) == t {
        return Ok(Addr::Checksummed(b));
    }
    Err(Error::NotRepresentable)
}

pub fn addr_text(a: &Addr) -> String {
    match a {
        Addr::Lower(b) => {
            let mut s = String::with_capacity(42);
            s.push_str("0x");
            push_hex(&mut s, b);
            s
        }
        Addr::Checksummed(b) => format!("0x{}", eip55(b)),
    }
}

/// A canonical lowercase UUID becomes 16 raw bytes; anything else stays text.
pub fn parse_id(s: &str) -> Result<Id, Error> {
    let b = s.as_bytes();
    if b.len() == 36 && [8, 13, 18, 23].iter().all(|&i| b[i] == b'-') && !b.iter().any(|c| c.is_ascii_uppercase()) {
        let hex: Vec<u8> = b.iter().copied().filter(|&c| c != b'-').collect();
        let mut out = [0u8; 16];
        if parse_hex_into(&hex, &mut out).is_ok() {
            return Ok(Id::Uuid(out));
        }
    }
    if s.len() > MAX_TEXT {
        return Err(Error::NotRepresentable);
    }
    Ok(Id::Text(s.to_owned()))
}

pub fn id_text(id: &Id) -> String {
    match id {
        Id::Text(s) => s.clone(),
        Id::Uuid(b) => {
            let mut s = String::with_capacity(36);
            for (i, x) in b.iter().enumerate() {
                if matches!(i, 4 | 6 | 8 | 10) {
                    s.push('-');
                }
                push_hex(&mut s, &[*x]);
            }
            s
        }
    }
}

/// JSON string with escaping. Text from a decoded binary frame is untrusted
/// and may contain quotes or control characters.
pub fn push_json_str(out: &mut String, s: &str) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

fn push_dec_str(out: &mut String, d: &Dec) {
    out.push('"');
    d.write_to(out);
    out.push('"');
}

fn push_levels(out: &mut String, levels: &[Level]) {
    out.push('[');
    for (i, l) in levels.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push('[');
        push_dec_str(out, &l.price);
        out.push(',');
        push_dec_str(out, &l.qty);
        out.push(']');
    }
    out.push(']');
}

fn open_notification(out: &mut String, channel_tail: std::fmt::Arguments<'_>) {
    out.push_str(r#"{"method":"subscription","params":{"channel":""#);
    let _ = out.write_fmt(channel_tail);
    out.push_str(r#"","data":"#);
}

pub fn render_book(out: &mut String, instrument: &str, timestamp: i64, b: &BookSnapshot) -> Result<(), Error> {
    let (g, d) = book_chan_parts(b.chan)?;
    open_notification(out, format_args!("orderbook.{instrument}.{g}.{d}"));
    out.push_str(r#"{"asks":"#);
    push_levels(out, &b.asks);
    out.push_str(r#","bids":"#);
    push_levels(out, &b.bids);
    let _ = write!(
        out,
        r#","instrument_name":"{instrument}","publish_id":{},"timestamp":{timestamp}}}}}}}"#,
        b.publish_id
    );
    Ok(())
}

fn push_field(out: &mut String, key: &str, v: &Option<Dec>) {
    out.push('"');
    out.push_str(key);
    out.push_str("\":");
    match v {
        Some(d) => push_dec_str(out, d),
        None => out.push_str("null"),
    }
}

pub fn render_ticker(
    out: &mut String,
    instrument: &str,
    chan: u8,
    timestamp: i64,
    s: &TickerState,
) -> Result<(), Error> {
    let interval = INTERVALS.get(chan as usize).ok_or(Error::BadChannel)?;
    open_notification(out, format_args!("ticker_slim.{instrument}.{interval}"));
    let f = &s.fields;
    out.push_str(r#"{"instrument_ticker":{"#);
    for (key, i) in [("A", 1), ("B", 3), ("I", 5), ("M", 4), ("a", 0), ("b", 2), ("f", 6), ("maxp", 8), ("minp", 7)] {
        push_field(out, key, &f[i]);
        out.push(',');
    }
    out.push_str(r#""option_pricing":"#);
    if s.pricing_null {
        out.push_str("null");
    } else {
        out.push('{');
        let keys = ["ai", "bi", "d", "df", "f", "g", "i", "m", "r", "t", "v"];
        for (i, key) in keys.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            push_field(out, key, &f[TICKER_PRICING_START + i]);
        }
        out.push('}');
    }
    out.push_str(r#","stats":{"#);
    push_field(out, "c", &f[9]);
    out.push(',');
    push_field(out, "h", &f[10]);
    out.push(',');
    push_field(out, "l", &f[11]);
    out.push_str(r#","n":"#);
    match f[12] {
        Some(d) => d.write_to(out),
        None => out.push('0'),
    }
    out.push(',');
    push_field(out, "oi", &f[13]);
    out.push(',');
    push_field(out, "p", &f[14]);
    out.push(',');
    push_field(out, "pr", &f[15]);
    out.push(',');
    push_field(out, "v", &f[16]);
    let _ = write!(out, r#"}},"t":{}}},"timestamp":{timestamp}}}}}}}"#, s.t);
    Ok(())
}

fn push_opt_id(out: &mut String, id: &Option<Id>) {
    match id {
        Some(i) => push_json_str(out, &id_text(i)),
        None => out.push_str("null"),
    }
}

pub fn render_trades(out: &mut String, instrument: &str, trades: &[Trade]) {
    open_notification(out, format_args!("trades.{instrument}"));
    out.push('[');
    for (i, t) in trades.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        let _ = write!(out, r#"{{"direction":"{}","#, if t.is_buy { "buy" } else { "sell" });
        out.push_str(r#""expected_rebate":"#);
        push_dec_str(out, &t.expected_rebate);
        out.push_str(r#","extra_fee":"#);
        push_dec_str(out, &t.extra_fee);
        out.push_str(r#","index_price":"#);
        push_dec_str(out, &t.index_price);
        let _ = write!(
            out,
            r#","instrument_name":"{instrument}","liquidity_role":"{}","mark_price":"#,
            if t.is_maker { "maker" } else { "taker" }
        );
        push_dec_str(out, &t.mark_price);
        out.push_str(r#","quote_id":"#);
        push_opt_id(out, &t.quote_id);
        out.push_str(r#","realized_pnl":"#);
        push_dec_str(out, &t.realized_pnl);
        out.push_str(r#","realized_pnl_excl_fees":"#);
        push_dec_str(out, &t.realized_pnl_excl_fees);
        out.push_str(r#","rfq_id":"#);
        push_opt_id(out, &t.rfq_id);
        let _ = write!(out, r#","subaccount_id":{},"timestamp":{},"trade_amount":"#, t.subaccount_id, t.timestamp);
        push_dec_str(out, &t.trade_amount);
        out.push_str(r#","trade_fee":"#);
        push_dec_str(out, &t.trade_fee);
        out.push_str(r#","trade_id":"#);
        push_json_str(out, &id_text(&t.trade_id));
        out.push_str(r#","trade_price":"#);
        push_dec_str(out, &t.trade_price);
        out.push_str(r#","wallet":"#);
        push_json_str(out, &addr_text(&t.wallet));
        out.push('}');
    }
    out.push_str("]}}");
}

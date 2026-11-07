//! Message types and the frame codec. See docs/format-spec.md.
//!
//! ```text
//! frame   := magic_version:u8 message*
//! message := len:varint type:u8 flags:u8 instrument:varint seq:varint ts:zigzag body
//! ```
//!
//! `len` counts everything after itself. WebSocket and ZMQ already delimit a
//! frame, so `len` is not needed to find the end of a single message. It earns
//! its place in two ways: it lets several messages share one frame (batching),
//! and it lets an old client skip a message type it does not know.
//!
//! Message bytes never depend on the frame they travel in. That is what lets
//! a gateway encode each message once and then concatenate the shared bytes
//! into a different frame for each client.

use crate::dec::{get_dec, put_dec, Column, Dec};
use crate::error::Error;
use crate::wire::{put_varint, put_varint128, put_zigzag, put_zigzag128, Reader};

pub const MAGIC_VERSION: u8 = 0xB1;
pub const MAX_FRAME: usize = 1 << 20;
pub const MAX_LEVELS: usize = 1024;
pub const MAX_TRADES: usize = 1024;
pub const MAX_TEXT: usize = 256;
pub const MAX_RAW: usize = MAX_FRAME;

pub const TYPE_BOOK_SNAPSHOT: u8 = 1;
pub const TYPE_BOOK_DELTA: u8 = 2;
pub const TYPE_TICKER_SNAPSHOT: u8 = 3;
pub const TYPE_TICKER_DELTA: u8 = 4;
pub const TYPE_TRADES: u8 = 5;
pub const TYPE_INSTRUMENT_DEF: u8 = 6;
pub const TYPE_RESYNC_REQUEST: u8 = 7;
pub const TYPE_RAW_JSON: u8 = 8;
pub const TYPE_NEW_ORDER: u8 = 16;
pub const TYPE_ORDER_ACK: u8 = 17;

/// Set on a snapshot sent in answer to a resync request.
pub const FLAG_RESYNC: u8 = 0x01;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Header {
    pub ty: u8,
    pub flags: u8,
    pub instrument: u32,
    /// Per-stream sequence, contiguous, assigned by the encoder. It is not
    /// `publish_id`, because the public docs do not say whether that is
    /// contiguous, and a gap detector needs to know.
    pub seq: u64,
    /// Absolute milliseconds in a snapshot, change since the previous message
    /// of the same stream in a delta. Relative to the stream, not the frame,
    /// so that batching does not change the bytes.
    pub ts: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Side {
    Bid,
    Ask,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Level {
    pub price: Dec,
    pub qty: Dec,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct BookSnapshot {
    pub chan: u8,
    pub publish_id: u64,
    pub bids: Vec<Level>,
    pub asks: Vec<Level>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BookOp {
    pub side: Side,
    pub price: Dec,
    /// `None` removes the level.
    pub qty: Option<Dec>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct BookDelta {
    pub chan: u8,
    pub publish_id_delta: i64,
    pub ops: Vec<BookOp>,
}

/// 28 ticker fields, in the order of `TICKER_FIELDS`. `None` means JSON null.
pub const TICKER_N: usize = 28;
pub const TICKER_PRICING_START: usize = 17;
/// Bit in the null mask meaning `option_pricing` itself is null.
pub const TICKER_PRICING_NULL_BIT: u32 = 1 << 28;

pub type TickerFields = [Option<Dec>; TICKER_N];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TickerState {
    pub fields: TickerFields,
    pub pricing_null: bool,
    /// `t` inside the snapshot, in ms since the epoch.
    pub t: i64,
}

impl Default for TickerState {
    fn default() -> Self {
        Self { fields: [None; TICKER_N], pricing_null: true, t: 0 }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TickerMsg {
    pub chan: u8,
    /// Which fields follow. A snapshot sets all of them.
    pub mask: u32,
    /// Subset of `mask` that is null, plus `TICKER_PRICING_NULL_BIT`.
    pub null_mask: u32,
    /// `t` minus the message timestamp.
    pub t_offset: i64,
    pub values: Vec<Dec>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Addr {
    /// Lowercase hex in JSON.
    Lower([u8; 20]),
    /// EIP-55 mixed-case hex in JSON, recomputed on decode.
    Checksummed([u8; 20]),
}

impl Addr {
    pub fn bytes(&self) -> &[u8; 20] {
        match self {
            Addr::Lower(b) | Addr::Checksummed(b) => b,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Id {
    Uuid([u8; 16]),
    Text(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Trade {
    pub is_buy: bool,
    pub is_maker: bool,
    pub expected_rebate: Dec,
    pub extra_fee: Dec,
    pub index_price: Dec,
    pub mark_price: Dec,
    pub quote_id: Option<Id>,
    pub realized_pnl: Dec,
    pub realized_pnl_excl_fees: Dec,
    pub rfq_id: Option<Id>,
    pub subaccount_id: i64,
    pub timestamp: i64,
    pub trade_amount: Dec,
    pub trade_fee: Dec,
    pub trade_id: Id,
    pub trade_price: Dec,
    pub wallet: Addr,
}

/// Fields of `private/order` needed to rebuild the request exactly.
/// See `order.rs` for which of them the signature covers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewOrder {
    pub rpc_id: u64,
    pub subaccount_id: i64,
    pub is_buy: bool,
    pub limit_price: Dec,
    pub amount: Dec,
    pub max_fee: Dec,
    pub nonce: u128,
    pub signer: Addr,
    pub signature: [u8; 65],
    pub signature_expiry_sec: i64,
    pub order_type: Option<OrderType>,
    pub time_in_force: Option<Tif>,
    pub reduce_only: Option<bool>,
    pub mmp: Option<bool>,
    pub label: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrderType {
    Limit,
    Market,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tif {
    Gtc,
    PostOnly,
    Fok,
    Ioc,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrderAck {
    pub rpc_id: u64,
    pub accepted: bool,
    pub order_id: [u8; 16],
    pub reason: u16,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Body {
    BookSnapshot(BookSnapshot),
    BookDelta(BookDelta),
    TickerSnapshot(TickerMsg),
    TickerDelta(TickerMsg),
    Trades { chan: u8, trades: Vec<Trade> },
    InstrumentDef { name: String },
    ResyncRequest { family: u8, chan: u8 },
    RawJson(Vec<u8>),
    NewOrder(Box<NewOrder>),
    OrderAck(OrderAck),
    /// A type this version does not know. Skipped using `len`.
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    pub header: Header,
    pub body: Body,
}

// ---------------------------------------------------------------- encode

fn put_text(out: &mut Vec<u8>, s: &str) {
    put_varint(out, s.len() as u64);
    out.extend_from_slice(s.as_bytes());
}

fn put_levels_prices(out: &mut Vec<u8>, c: &Column, levels: &[Level], prev: &mut i128) -> Result<(), Error> {
    for l in levels {
        if c.shared() {
            let m = c.store(&l.price).ok_or(Error::NotRepresentable)?;
            let d = m.checked_sub(*prev).ok_or(Error::NotRepresentable)?;
            put_zigzag128(out, d);
            *prev = m;
        } else {
            put_dec(out, l.price);
        }
    }
    Ok(())
}

fn put_levels_qtys(out: &mut Vec<u8>, c: &Column, levels: &[Level]) -> Result<(), Error> {
    for l in levels {
        if c.shared() {
            put_zigzag128(out, c.store(&l.qty).ok_or(Error::NotRepresentable)?);
        } else {
            put_dec(out, l.qty);
        }
    }
    Ok(())
}

fn encode_book_snapshot(out: &mut Vec<u8>, b: &BookSnapshot) -> Result<(), Error> {
    out.push(b.chan);
    put_varint(out, b.publish_id);
    let pc = Column::choose(b.bids.iter().chain(&b.asks).map(|l| &l.price));
    let qc = Column::choose(b.bids.iter().chain(&b.asks).map(|l| &l.qty));
    out.push(pc.byte());
    out.push(qc.byte());
    put_varint(out, b.bids.len() as u64);
    put_varint(out, b.asks.len() as u64);
    // Prices are deltas down the bid side, then up the ask side from the best
    // bid. Sorted books give small numbers; unsorted input still round-trips.
    let mut prev = 0i128;
    put_levels_prices(out, &pc, &b.bids, &mut prev)?;
    if let Some(best) = b.bids.first() {
        prev = if pc.shared() { pc.store(&best.price).ok_or(Error::NotRepresentable)? } else { 0 };
    }
    put_levels_prices(out, &pc, &b.asks, &mut prev)?;
    put_levels_qtys(out, &qc, &b.bids)?;
    put_levels_qtys(out, &qc, &b.asks)?;
    Ok(())
}

fn encode_book_delta(out: &mut Vec<u8>, b: &BookDelta) -> Result<(), Error> {
    out.push(b.chan);
    put_zigzag(out, b.publish_id_delta);
    let pc = Column::choose(b.ops.iter().map(|o| &o.price));
    let qc = Column::choose(b.ops.iter().filter_map(|o| o.qty.as_ref()));
    out.push(pc.byte());
    out.push(qc.byte());
    put_varint(out, b.ops.len() as u64);
    let mut prev = 0i128;
    for o in &b.ops {
        // Side and remove bits ride in a single control byte's worth of the
        // price delta, so a plain quantity change costs no extra byte.
        let ctl = (matches!(o.side, Side::Ask) as u128) | ((o.qty.is_none() as u128) << 1);
        if pc.shared() {
            let m = pc.store(&o.price).ok_or(Error::NotRepresentable)?;
            let d = m.checked_sub(prev).ok_or(Error::NotRepresentable)?;
            let z = crate::wire::zig128(d);
            if z >> 125 != 0 {
                return Err(Error::NotRepresentable);
            }
            put_varint128(out, (z << 2) | ctl);
            prev = m;
        } else {
            put_varint128(out, ctl);
            put_dec(out, o.price);
        }
        if let Some(q) = o.qty {
            if qc.shared() {
                put_zigzag128(out, qc.store(&q).ok_or(Error::NotRepresentable)?);
            } else {
                put_dec(out, q);
            }
        }
    }
    Ok(())
}

fn encode_ticker(out: &mut Vec<u8>, t: &TickerMsg) {
    out.push(t.chan);
    put_varint(out, t.mask as u64);
    put_varint(out, t.null_mask as u64);
    put_zigzag(out, t.t_offset);
    for v in &t.values {
        put_dec(out, *v);
    }
}

fn put_addr(out: &mut Vec<u8>, a: &Addr) {
    out.extend_from_slice(a.bytes());
}

fn put_opt_id(out: &mut Vec<u8>, id: &Option<Id>) {
    match id {
        None => out.push(0),
        Some(Id::Uuid(b)) => {
            out.push(1);
            out.extend_from_slice(b);
        }
        Some(Id::Text(s)) => {
            out.push(2);
            put_text(out, s);
        }
    }
}

fn get_opt_id(r: &mut Reader<'_>) -> Result<Option<Id>, Error> {
    Ok(match r.u8()? {
        0 => None,
        1 => Some(Id::Uuid(r.array()?)),
        2 => Some(Id::Text(get_text(r)?)),
        _ => return Err(Error::BadEnum),
    })
}

fn encode_trades(out: &mut Vec<u8>, chan: u8, trades: &[Trade]) {
    out.push(chan);
    put_varint(out, trades.len() as u64);
    for t in trades {
        let bits = (t.is_buy as u8)
            | ((t.is_maker as u8) << 1)
            | ((matches!(t.wallet, Addr::Checksummed(_)) as u8) << 2);
        out.push(bits);
        put_dec(out, t.trade_price);
        put_dec(out, t.trade_amount);
        put_dec(out, t.index_price);
        put_dec(out, t.mark_price);
        put_dec(out, t.trade_fee);
        put_dec(out, t.extra_fee);
        put_dec(out, t.expected_rebate);
        put_dec(out, t.realized_pnl);
        put_dec(out, t.realized_pnl_excl_fees);
        put_zigzag(out, t.subaccount_id);
        put_zigzag(out, t.timestamp);
        put_opt_id(out, &Some(t.trade_id.clone()));
        put_opt_id(out, &t.quote_id);
        put_opt_id(out, &t.rfq_id);
        put_addr(out, &t.wallet);
    }
}

fn encode_new_order(out: &mut Vec<u8>, o: &NewOrder) {
    put_varint(out, o.rpc_id);
    let mut bits = (o.is_buy as u16) | ((matches!(o.signer, Addr::Checksummed(_)) as u16) << 1);
    let tri = |v: Option<bool>| -> u16 {
        match v {
            None => 0,
            Some(false) => 1,
            Some(true) => 2,
        }
    };
    bits |= tri(o.reduce_only) << 2;
    bits |= tri(o.mmp) << 4;
    bits |= (match o.order_type {
        None => 0u16,
        Some(OrderType::Limit) => 1,
        Some(OrderType::Market) => 2,
    }) << 6;
    bits |= (match o.time_in_force {
        None => 0u16,
        Some(Tif::Gtc) => 1,
        Some(Tif::PostOnly) => 2,
        Some(Tif::Fok) => 3,
        Some(Tif::Ioc) => 4,
    }) << 8;
    bits |= (o.label.is_some() as u16) << 11;
    put_varint(out, bits as u64);
    put_zigzag(out, o.subaccount_id);
    put_dec(out, o.limit_price);
    put_dec(out, o.amount);
    put_dec(out, o.max_fee);
    put_varint128(out, o.nonce);
    put_addr(out, &o.signer);
    out.extend_from_slice(&o.signature);
    put_zigzag(out, o.signature_expiry_sec);
    if let Some(l) = &o.label {
        put_text(out, l);
    }
}

fn encode_body(out: &mut Vec<u8>, body: &Body) -> Result<(), Error> {
    match body {
        Body::BookSnapshot(b) => encode_book_snapshot(out, b)?,
        Body::BookDelta(b) => encode_book_delta(out, b)?,
        Body::TickerSnapshot(t) | Body::TickerDelta(t) => encode_ticker(out, t),
        Body::Trades { chan, trades } => encode_trades(out, *chan, trades),
        Body::InstrumentDef { name } => put_text(out, name),
        Body::ResyncRequest { family, chan } => {
            out.push(*family);
            out.push(*chan);
        }
        Body::RawJson(b) => out.extend_from_slice(b),
        Body::NewOrder(o) => encode_new_order(out, o),
        Body::OrderAck(a) => {
            put_varint(out, a.rpc_id);
            out.push(a.accepted as u8);
            out.extend_from_slice(&a.order_id);
            put_varint(out, a.reason as u64);
        }
        Body::Unknown => return Err(Error::BadEnum),
    }
    Ok(())
}

pub fn type_of(body: &Body) -> u8 {
    match body {
        Body::BookSnapshot(_) => TYPE_BOOK_SNAPSHOT,
        Body::BookDelta(_) => TYPE_BOOK_DELTA,
        Body::TickerSnapshot(_) => TYPE_TICKER_SNAPSHOT,
        Body::TickerDelta(_) => TYPE_TICKER_DELTA,
        Body::Trades { .. } => TYPE_TRADES,
        Body::InstrumentDef { .. } => TYPE_INSTRUMENT_DEF,
        Body::ResyncRequest { .. } => TYPE_RESYNC_REQUEST,
        Body::RawJson(_) => TYPE_RAW_JSON,
        Body::NewOrder(_) => TYPE_NEW_ORDER,
        Body::OrderAck(_) => TYPE_ORDER_ACK,
        Body::Unknown => 0,
    }
}

/// Appends one length-prefixed message, without the frame's leading byte.
pub fn encode_message(out: &mut Vec<u8>, h: &Header, body: &Body) -> Result<(), Error> {
    let mut inner = Vec::with_capacity(256);
    inner.push(type_of(body));
    inner.push(h.flags);
    put_varint(&mut inner, h.instrument as u64);
    put_varint(&mut inner, h.seq);
    put_zigzag(&mut inner, h.ts);
    encode_body(&mut inner, body)?;
    put_varint(out, inner.len() as u64);
    out.extend_from_slice(&inner);
    Ok(())
}

/// A complete single-message frame.
pub fn encode_frame(h: &Header, body: &Body) -> Result<Vec<u8>, Error> {
    let mut out = Vec::with_capacity(128);
    out.push(MAGIC_VERSION);
    encode_message(&mut out, h, body)?;
    Ok(out)
}

// ---------------------------------------------------------------- decode

fn get_text(r: &mut Reader<'_>) -> Result<String, Error> {
    let n = r.count(MAX_TEXT, 1)?;
    let b = r.bytes(n)?;
    std::str::from_utf8(b).map(str::to_owned).map_err(|_| Error::BadUtf8)
}

fn get_price_col(r: &mut Reader<'_>, c: &Column, n: usize, prev: &mut i128, out: &mut Vec<Dec>) -> Result<(), Error> {
    for _ in 0..n {
        if c.shared() {
            let d = r.zigzag128()?;
            let m = prev.checked_add(d).ok_or(Error::DecimalOverflow)?;
            *prev = m;
            out.push(c.load(m));
        } else {
            out.push(get_dec(r)?);
        }
    }
    Ok(())
}

fn get_qty(r: &mut Reader<'_>, c: &Column) -> Result<Dec, Error> {
    if c.shared() {
        Ok(c.load(r.zigzag128()?))
    } else {
        get_dec(r)
    }
}

fn decode_book_snapshot(r: &mut Reader<'_>) -> Result<BookSnapshot, Error> {
    let chan = r.u8()?;
    let publish_id = r.varint()?;
    let pc = Column::from_byte(r.u8()?)?;
    let qc = Column::from_byte(r.u8()?)?;
    let nb = r.count(MAX_LEVELS, 2)?;
    let na = r.count(MAX_LEVELS, 2)?;
    if (nb + na).saturating_mul(2) > r.remaining() {
        return Err(Error::Truncated);
    }
    let mut prices = Vec::with_capacity(nb + na);
    let mut prev = 0i128;
    get_price_col(r, &pc, nb, &mut prev, &mut prices)?;
    if nb > 0 && pc.shared() {
        // Rewind to the best bid: the first delta was absolute.
        prev = pc.store(&prices[0]).ok_or(Error::DecimalOverflow)?;
    } else if nb > 0 {
        prev = 0;
    }
    get_price_col(r, &pc, na, &mut prev, &mut prices)?;
    let mut bids = Vec::with_capacity(nb);
    let mut asks = Vec::with_capacity(na);
    for p in prices.iter().take(nb) {
        bids.push(Level { price: *p, qty: get_qty(r, &qc)? });
    }
    for p in prices.iter().skip(nb) {
        asks.push(Level { price: *p, qty: get_qty(r, &qc)? });
    }
    Ok(BookSnapshot { chan, publish_id, bids, asks })
}

fn decode_book_delta(r: &mut Reader<'_>) -> Result<BookDelta, Error> {
    let chan = r.u8()?;
    let publish_id_delta = r.zigzag()?;
    let pc = Column::from_byte(r.u8()?)?;
    let qc = Column::from_byte(r.u8()?)?;
    let n = r.count(2 * MAX_LEVELS, 1)?;
    let mut ops = Vec::with_capacity(n);
    let mut prev = 0i128;
    for _ in 0..n {
        let v = r.varint128()?;
        let side = if v & 1 == 1 { Side::Ask } else { Side::Bid };
        let remove = v & 2 != 0;
        let price = if pc.shared() {
            let d = crate::wire::unzig128(v >> 2);
            let m = prev.checked_add(d).ok_or(Error::DecimalOverflow)?;
            prev = m;
            pc.load(m)
        } else {
            if v >> 2 != 0 {
                return Err(Error::BadEnum);
            }
            get_dec(r)?
        };
        let qty = if remove { None } else { Some(get_qty(r, &qc)?) };
        ops.push(BookOp { side, price, qty });
    }
    Ok(BookDelta { chan, publish_id_delta, ops })
}

fn decode_ticker(r: &mut Reader<'_>) -> Result<TickerMsg, Error> {
    let chan = r.u8()?;
    let mask = u32::try_from(r.varint()?).map_err(|_| Error::BadEnum)?;
    let null_mask = u32::try_from(r.varint()?).map_err(|_| Error::BadEnum)?;
    if mask >> TICKER_N != 0 || (null_mask & !TICKER_PRICING_NULL_BIT) & !mask != 0 {
        return Err(Error::BadEnum);
    }
    let t_offset = r.zigzag()?;
    let n = (mask & !null_mask).count_ones() as usize;
    let mut values = Vec::with_capacity(n);
    for _ in 0..n {
        values.push(get_dec(r)?);
    }
    Ok(TickerMsg { chan, mask, null_mask, t_offset, values })
}

fn get_addr(r: &mut Reader<'_>, checksummed: bool) -> Result<Addr, Error> {
    let b: [u8; 20] = r.array()?;
    Ok(if checksummed { Addr::Checksummed(b) } else { Addr::Lower(b) })
}

fn decode_trades(r: &mut Reader<'_>) -> Result<(u8, Vec<Trade>), Error> {
    let chan = r.u8()?;
    // A trade is never shorter than its fixed fields: 1 + 9 + 2 + 3 + 20.
    let n = r.count(MAX_TRADES, 35)?;
    let mut trades = Vec::with_capacity(n);
    for _ in 0..n {
        let bits = r.u8()?;
        if bits >> 3 != 0 {
            return Err(Error::BadEnum);
        }
        let trade_price = get_dec(r)?;
        let trade_amount = get_dec(r)?;
        let index_price = get_dec(r)?;
        let mark_price = get_dec(r)?;
        let trade_fee = get_dec(r)?;
        let extra_fee = get_dec(r)?;
        let expected_rebate = get_dec(r)?;
        let realized_pnl = get_dec(r)?;
        let realized_pnl_excl_fees = get_dec(r)?;
        let subaccount_id = r.zigzag()?;
        let timestamp = r.zigzag()?;
        let trade_id = get_opt_id(r)?.ok_or(Error::BadEnum)?;
        let quote_id = get_opt_id(r)?;
        let rfq_id = get_opt_id(r)?;
        let wallet = get_addr(r, bits & 4 != 0)?;
        trades.push(Trade {
            is_buy: bits & 1 != 0,
            is_maker: bits & 2 != 0,
            expected_rebate,
            extra_fee,
            index_price,
            mark_price,
            quote_id,
            realized_pnl,
            realized_pnl_excl_fees,
            rfq_id,
            subaccount_id,
            timestamp,
            trade_amount,
            trade_fee,
            trade_id,
            trade_price,
            wallet,
        });
    }
    Ok((chan, trades))
}

fn decode_new_order(r: &mut Reader<'_>) -> Result<NewOrder, Error> {
    let rpc_id = r.varint()?;
    let bits = r.varint()?;
    if bits >> 12 != 0 {
        return Err(Error::BadEnum);
    }
    let tri = |v: u64| -> Result<Option<bool>, Error> {
        match v {
            0 => Ok(None),
            1 => Ok(Some(false)),
            2 => Ok(Some(true)),
            _ => Err(Error::BadEnum),
        }
    };
    let reduce_only = tri((bits >> 2) & 3)?;
    let mmp = tri((bits >> 4) & 3)?;
    let order_type = match (bits >> 6) & 3 {
        0 => None,
        1 => Some(OrderType::Limit),
        2 => Some(OrderType::Market),
        _ => return Err(Error::BadEnum),
    };
    let time_in_force = match (bits >> 8) & 7 {
        0 => None,
        1 => Some(Tif::Gtc),
        2 => Some(Tif::PostOnly),
        3 => Some(Tif::Fok),
        4 => Some(Tif::Ioc),
        _ => return Err(Error::BadEnum),
    };
    let subaccount_id = r.zigzag()?;
    let limit_price = get_dec(r)?;
    let amount = get_dec(r)?;
    let max_fee = get_dec(r)?;
    let nonce = r.varint128()?;
    let signer = get_addr(r, bits & 2 != 0)?;
    let signature: [u8; 65] = r.array()?;
    let signature_expiry_sec = r.zigzag()?;
    let label = if bits & (1 << 11) != 0 { Some(get_text(r)?) } else { None };
    Ok(NewOrder {
        rpc_id,
        subaccount_id,
        is_buy: bits & 1 != 0,
        limit_price,
        amount,
        max_fee,
        nonce,
        signer,
        signature,
        signature_expiry_sec,
        order_type,
        time_in_force,
        reduce_only,
        mmp,
        label,
    })
}

fn decode_body(ty: u8, r: &mut Reader<'_>) -> Result<Body, Error> {
    Ok(match ty {
        TYPE_BOOK_SNAPSHOT => Body::BookSnapshot(decode_book_snapshot(r)?),
        TYPE_BOOK_DELTA => Body::BookDelta(decode_book_delta(r)?),
        TYPE_TICKER_SNAPSHOT => Body::TickerSnapshot(decode_ticker(r)?),
        TYPE_TICKER_DELTA => Body::TickerDelta(decode_ticker(r)?),
        TYPE_TRADES => {
            let (chan, trades) = decode_trades(r)?;
            Body::Trades { chan, trades }
        }
        TYPE_INSTRUMENT_DEF => Body::InstrumentDef { name: get_text(r)? },
        TYPE_RESYNC_REQUEST => Body::ResyncRequest { family: r.u8()?, chan: r.u8()? },
        TYPE_RAW_JSON => {
            let n = r.remaining();
            if n > MAX_RAW {
                return Err(Error::FrameTooLarge);
            }
            Body::RawJson(r.bytes(n)?.to_vec())
        }
        TYPE_NEW_ORDER => Body::NewOrder(Box::new(decode_new_order(r)?)),
        TYPE_ORDER_ACK => {
            let rpc_id = r.varint()?;
            let accepted = match r.u8()? {
                0 => false,
                1 => true,
                _ => return Err(Error::BadEnum),
            };
            let order_id: [u8; 16] = r.array()?;
            let reason = u16::try_from(r.varint()?).map_err(|_| Error::BadEnum)?;
            Body::OrderAck(OrderAck { rpc_id, accepted, order_id, reason })
        }
        _ => Body::Unknown,
    })
}

/// Reads one message from a reader positioned after the frame byte.
pub fn decode_message(r: &mut Reader<'_>) -> Result<Message, Error> {
    let len = r.varint()?;
    if len > MAX_FRAME as u64 {
        return Err(Error::FrameTooLarge);
    }
    let inner = r.bytes(len as usize)?;
    let mut m = Reader::new(inner);
    let ty = m.u8()?;
    let flags = m.u8()?;
    let instrument = u32::try_from(m.varint()?).map_err(|_| Error::BadEnum)?;
    let seq = m.varint()?;
    let ts = m.zigzag()?;
    let body = decode_body(ty, &mut m)?;
    // Trailing bytes inside `len` are allowed: a newer minor version may
    // append fields, and an older decoder must ignore them.
    Ok(Message { header: Header { ty, flags, instrument, seq, ts }, body })
}

/// Decodes a whole frame. Stops at the first error.
pub fn decode_frame(frame: &[u8]) -> Result<Vec<Message>, Error> {
    if frame.len() > MAX_FRAME {
        return Err(Error::FrameTooLarge);
    }
    let mut r = Reader::new(frame);
    let magic = r.u8()?;
    if magic >> 4 != MAGIC_VERSION >> 4 {
        return Err(Error::BadMagic);
    }
    if magic & 0xf != MAGIC_VERSION & 0xf {
        return Err(Error::BadVersion);
    }
    let mut out = Vec::new();
    while !r.is_empty() {
        out.push(decode_message(&mut r)?);
    }
    Ok(out)
}

//! JSON to binary, once per message.
//!
//! The transcoder owns all per-stream state, so its cost is paid per upstream
//! message and not per client. A message that cannot be carried exactly goes
//! out unchanged as `RawJson`. Binary clients therefore never lose data; they
//! just lose the size saving on that message.

use crate::dec::Dec;
use crate::error::Error;
use crate::json::{parse_notification, Parsed};
use crate::msg::*;
use std::collections::HashMap;

#[derive(Debug, Clone, Copy, Default)]
pub struct Config {
    /// Send changes instead of full state. Exact: every JSON message can be
    /// rebuilt, including `publish_id` and `timestamp`.
    pub deltas: bool,
    /// With `deltas`, drop messages that change nothing. Not exact: the book
    /// or ticker is right, but the skipped `publish_id`s are not delivered.
    pub suppress_unchanged: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Family {
    Book = 1,
    Ticker = 2,
    Trades = 3,
}

impl Family {
    pub fn from_u8(v: u8) -> Option<Family> {
        match v {
            1 => Some(Family::Book),
            2 => Some(Family::Ticker),
            3 => Some(Family::Trades),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct StreamId {
    pub instrument: u32,
    pub family: Family,
    pub chan: u8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Sent { stream: StreamId, kind: Kind },
    /// Unchanged and `suppress_unchanged` is on: nothing was written.
    Suppressed { stream: StreamId },
    /// Forwarded as raw JSON.
    Raw,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Snapshot,
    Delta,
    Trades,
}

struct BookStream {
    seq: u64,
    ts: i64,
    book: BookSnapshot,
}

struct TickerStream {
    seq: u64,
    ts: i64,
    state: TickerState,
}

#[derive(Default)]
pub struct Transcoder {
    pub config: Config,
    ids: HashMap<String, u32>,
    names: Vec<String>,
    books: HashMap<(u32, u8), BookStream>,
    tickers: HashMap<(u32, u8), TickerStream>,
    trades: HashMap<u32, u64>,
    raw_seq: u64,
}

/// Ops that turn the sorted side `prev` into `cur`, or `None` if either side
/// is not strictly sorted, in which case the caller sends a snapshot.
fn diff_side(side: Side, prev: &[Level], cur: &[Level], ops: &mut Vec<BookOp>) -> Option<()> {
    use std::cmp::Ordering::*;
    // Best price first: descending bids, ascending asks.
    let order = |a: &Dec, b: &Dec| -> Option<std::cmp::Ordering> {
        let o = a.cmp_num(b)?;
        Some(if side == Side::Bid { o.reverse() } else { o })
    };
    // Both sides must be strictly sorted, or the merge below would emit wrong
    // ops. `prev` can be unsorted if an earlier snapshot carried such a book.
    for w in cur.windows(2).chain(prev.windows(2)) {
        if order(&w[0].price, &w[1].price)? != Less {
            return None;
        }
    }
    let (mut i, mut j) = (0, 0);
    while i < prev.len() || j < cur.len() {
        let c = match (prev.get(i), cur.get(j)) {
            (Some(p), Some(q)) => order(&p.price, &q.price)?,
            (Some(_), None) => Less,
            (None, _) => Greater,
        };
        match c {
            Less => {
                ops.push(BookOp { side, price: prev[i].price, qty: None });
                i += 1;
            }
            Greater => {
                ops.push(BookOp { side, price: cur[j].price, qty: Some(cur[j].qty) });
                j += 1;
            }
            Equal => {
                if prev[i] != cur[j] {
                    ops.push(BookOp { side, price: cur[j].price, qty: Some(cur[j].qty) });
                }
                i += 1;
                j += 1;
            }
        }
    }
    Some(())
}

pub fn diff_book(prev: &BookSnapshot, cur: &BookSnapshot) -> Option<Vec<BookOp>> {
    let mut ops = Vec::new();
    diff_side(Side::Bid, &prev.bids, &cur.bids, &mut ops)?;
    diff_side(Side::Ask, &prev.asks, &cur.asks, &mut ops)?;
    Some(ops)
}

fn ticker_values(s: &TickerState, mask: u32) -> (u32, Vec<Dec>) {
    let mut null_mask = 0u32;
    let mut values = Vec::with_capacity(mask.count_ones() as usize);
    for i in 0..TICKER_N {
        if mask & (1 << i) != 0 {
            match s.fields[i] {
                Some(d) => values.push(d),
                None => null_mask |= 1 << i,
            }
        }
    }
    if s.pricing_null {
        null_mask |= TICKER_PRICING_NULL_BIT;
    }
    (null_mask, values)
}

const TICKER_ALL: u32 = (1 << TICKER_N) - 1;

impl Transcoder {
    pub fn new(config: Config) -> Self {
        Self { config, ..Default::default() }
    }

    pub fn instruments(&self) -> &[String] {
        &self.names
    }

    /// Writes a definition message when a name is seen for the first time.
    fn intern(&mut self, name: &str, out: &mut Vec<u8>) -> Result<u32, Error> {
        if let Some(id) = self.ids.get(name) {
            return Ok(*id);
        }
        let id = self.names.len() as u32 + 1;
        self.ids.insert(name.to_owned(), id);
        self.names.push(name.to_owned());
        let h = Header { ty: TYPE_INSTRUMENT_DEF, instrument: id, ..Default::default() };
        encode_message(out, &h, &Body::InstrumentDef { name: name.to_owned() })?;
        Ok(id)
    }

    /// Every instrument definition, for a client that has just connected.
    pub fn write_directory(&self, out: &mut Vec<u8>) -> Result<(), Error> {
        for (i, name) in self.names.iter().enumerate() {
            let h = Header { ty: TYPE_INSTRUMENT_DEF, instrument: i as u32 + 1, ..Default::default() };
            encode_message(out, &h, &Body::InstrumentDef { name: name.clone() })?;
        }
        Ok(())
    }

    /// Transcodes one JSON notification, appending to `out`.
    pub fn push(&mut self, json: &str, out: &mut Vec<u8>) -> Result<Outcome, Error> {
        let start = out.len();
        match self.try_push(json, out) {
            Ok(o) => Ok(o),
            Err(_) => {
                out.truncate(start);
                self.raw_seq += 1;
                let h = Header { ty: TYPE_RAW_JSON, seq: self.raw_seq, ..Default::default() };
                encode_message(out, &h, &Body::RawJson(json.as_bytes().to_vec()))?;
                Ok(Outcome::Raw)
            }
        }
    }

    fn try_push(&mut self, json: &str, out: &mut Vec<u8>) -> Result<Outcome, Error> {
        match parse_notification(json)? {
            Parsed::Book { instrument, timestamp, book } => {
                let id = self.intern(instrument, out)?;
                self.push_book(id, timestamp, book, out)
            }
            Parsed::Ticker { instrument, chan, timestamp, state } => {
                let id = self.intern(instrument, out)?;
                self.push_ticker(id, chan, timestamp, *state, out)
            }
            Parsed::Trades { instrument, trades } => {
                let id = self.intern(instrument, out)?;
                let seq = self.trades.entry(id).or_insert(0);
                *seq += 1;
                let ts = trades.first().map_or(0, |t| t.timestamp);
                let h = Header { ty: TYPE_TRADES, instrument: id, seq: *seq, ts, flags: 0 };
                encode_message(out, &h, &Body::Trades { chan: 0, trades })?;
                let stream = StreamId { instrument: id, family: Family::Trades, chan: 0 };
                Ok(Outcome::Sent { stream, kind: Kind::Trades })
            }
        }
    }

    fn push_book(&mut self, id: u32, ts: i64, book: BookSnapshot, out: &mut Vec<u8>) -> Result<Outcome, Error> {
        let stream = StreamId { instrument: id, family: Family::Book, chan: book.chan };
        let key = (id, book.chan);
        if let (true, Some(s)) = (self.config.deltas, self.books.get_mut(&key)) {
            let pub_delta = i64::try_from(book.publish_id as i128 - s.book.publish_id as i128).ok();
            let ts_delta = ts.checked_sub(s.ts);
            if let (Some(ops), Some(pub_delta), Some(ts_delta)) = (diff_book(&s.book, &book), pub_delta, ts_delta) {
                if ops.is_empty() && self.config.suppress_unchanged {
                    return Ok(Outcome::Suppressed { stream });
                }
                // A delta that touches most levels is no smaller than the full book.
                if ops.len() <= book.bids.len() + book.asks.len() {
                    s.seq += 1;
                    let h = Header { ty: TYPE_BOOK_DELTA, instrument: id, seq: s.seq, ts: ts_delta, flags: 0 };
                    let chan = book.chan;
                    encode_message(out, &h, &Body::BookDelta(BookDelta { chan, publish_id_delta: pub_delta, ops }))?;
                    s.book = book;
                    s.ts = ts;
                    return Ok(Outcome::Sent { stream, kind: Kind::Delta });
                }
            }
        }
        let s = self.books.entry(key).or_insert_with(|| BookStream { seq: 0, ts, book: BookSnapshot::default() });
        s.seq += 1;
        let h = Header { ty: TYPE_BOOK_SNAPSHOT, instrument: id, seq: s.seq, ts, flags: 0 };
        encode_message(out, &h, &Body::BookSnapshot(book.clone()))?;
        s.book = book;
        s.ts = ts;
        Ok(Outcome::Sent { stream, kind: Kind::Snapshot })
    }

    fn push_ticker(&mut self, id: u32, chan: u8, ts: i64, state: TickerState, out: &mut Vec<u8>) -> Result<Outcome, Error> {
        let stream = StreamId { instrument: id, family: Family::Ticker, chan };
        let key = (id, chan);
        let t_offset = state.t.checked_sub(ts).ok_or(Error::NotRepresentable)?;
        if let (true, Some(s)) = (self.config.deltas, self.tickers.get_mut(&key)) {
            let mut mask = 0u32;
            for i in 0..TICKER_N {
                if s.state.fields[i] != state.fields[i] {
                    mask |= 1 << i;
                }
            }
            if let (Some(ts_delta), true) = (ts.checked_sub(s.ts), s.state.pricing_null == state.pricing_null) {
                if mask == 0 && self.config.suppress_unchanged {
                    return Ok(Outcome::Suppressed { stream });
                }
                s.seq += 1;
                let (null_mask, values) = ticker_values(&state, mask);
                let h = Header { ty: TYPE_TICKER_DELTA, instrument: id, seq: s.seq, ts: ts_delta, flags: 0 };
                encode_message(out, &h, &Body::TickerDelta(TickerMsg { chan, mask, null_mask, t_offset, values }))?;
                s.state = state;
                s.ts = ts;
                return Ok(Outcome::Sent { stream, kind: Kind::Delta });
            }
        }
        let s = self.tickers.entry(key).or_insert_with(|| TickerStream { seq: 0, ts, state: TickerState::default() });
        s.seq += 1;
        let (null_mask, values) = ticker_values(&state, TICKER_ALL);
        let h = Header { ty: TYPE_TICKER_SNAPSHOT, instrument: id, seq: s.seq, ts, flags: 0 };
        encode_message(out, &h, &Body::TickerSnapshot(TickerMsg { chan, mask: TICKER_ALL, null_mask, t_offset, values }))?;
        s.state = state;
        s.ts = ts;
        Ok(Outcome::Sent { stream, kind: Kind::Snapshot })
    }

    /// Current full state of a stream as a snapshot message, without advancing
    /// the sequence. Used for late joiners and resync requests.
    pub fn snapshot_of(&self, stream: StreamId, flags: u8, out: &mut Vec<u8>) -> Result<bool, Error> {
        let key = (stream.instrument, stream.chan);
        match stream.family {
            Family::Book => {
                let Some(s) = self.books.get(&key) else { return Ok(false) };
                let h = Header { ty: TYPE_BOOK_SNAPSHOT, instrument: stream.instrument, seq: s.seq, ts: s.ts, flags };
                encode_message(out, &h, &Body::BookSnapshot(s.book.clone()))?;
                Ok(true)
            }
            Family::Ticker => {
                let Some(s) = self.tickers.get(&key) else { return Ok(false) };
                let (null_mask, values) = ticker_values(&s.state, TICKER_ALL);
                let t_offset = s.state.t - s.ts;
                let h = Header { ty: TYPE_TICKER_SNAPSHOT, instrument: stream.instrument, seq: s.seq, ts: s.ts, flags };
                let body = TickerMsg { chan: stream.chan, mask: TICKER_ALL, null_mask, t_offset, values };
                encode_message(out, &h, &Body::TickerSnapshot(body))?;
                Ok(true)
            }
            Family::Trades => Ok(false),
        }
    }

    pub fn streams(&self) -> Vec<StreamId> {
        let mut v: Vec<StreamId> = self
            .books
            .keys()
            .map(|&(instrument, chan)| StreamId { instrument, family: Family::Book, chan })
            .chain(self.tickers.keys().map(|&(instrument, chan)| StreamId { instrument, family: Family::Ticker, chan }))
            .collect();
        v.sort_by_key(|s| (s.instrument, s.family as u8, s.chan));
        v
    }
}

//! The client side: rebuild state from a binary stream, detect sequence gaps,
//! and recover from a snapshot.
//!
//! Recovery follows the same shape as Binance's documented local order book
//! procedure: drop state on a gap, wait for a snapshot, then ignore anything
//! at or below the snapshot's sequence.

use crate::error::Error;
use crate::json::{render_book, render_ticker, render_trades};
use crate::msg::*;
use crate::transcode::{Family, StreamId};
use std::collections::HashMap;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Applied {
    /// State advanced. Holds the rebuilt JSON if rendering was requested.
    Updated(Option<String>),
    /// This stream is out of sync and needs a snapshot. Request one with
    /// `ResyncRequest`.
    Gap { stream: StreamId, expected: u64, got: u64 },
    /// Dropped: duplicate, already covered by a snapshot, or waiting for one.
    Ignored,
    /// Not a stream message, for example a definition.
    Other,
}

struct BookRep {
    seq: u64,
    ts: i64,
    stale: bool,
    book: BookSnapshot,
}

struct TickerRep {
    seq: u64,
    ts: i64,
    stale: bool,
    state: TickerState,
}

#[derive(Default)]
pub struct Replica {
    names: HashMap<u32, String>,
    books: HashMap<(u32, u8), BookRep>,
    tickers: HashMap<(u32, u8), TickerRep>,
    trades: HashMap<u32, u64>,
    pub gaps: u64,
}

fn apply_side(levels: &mut Vec<Level>, side: Side, op: &BookOp) -> Result<(), Error> {
    let mut pos = levels.len();
    let mut found = false;
    for (i, l) in levels.iter().enumerate() {
        let c = l.price.cmp_num(&op.price).ok_or(Error::DecimalOverflow)?;
        let c = if side == Side::Bid { c.reverse() } else { c };
        match c {
            std::cmp::Ordering::Less => {}
            std::cmp::Ordering::Equal => {
                pos = i;
                found = true;
                break;
            }
            std::cmp::Ordering::Greater => {
                pos = i;
                break;
            }
        }
    }
    match (op.qty, found) {
        (None, true) => {
            levels.remove(pos);
        }
        (None, false) => return Err(Error::MissingState),
        (Some(q), true) => levels[pos] = Level { price: op.price, qty: q },
        (Some(q), false) => {
            if levels.len() >= MAX_LEVELS {
                return Err(Error::CountTooLarge);
            }
            levels.insert(pos, Level { price: op.price, qty: q });
        }
    }
    Ok(())
}

fn apply_ticker(state: &mut TickerState, m: &TickerMsg, ts: i64) -> Result<(), Error> {
    let mut it = m.values.iter();
    for i in 0..TICKER_N {
        if m.mask & (1 << i) != 0 {
            state.fields[i] = if m.null_mask & (1 << i) != 0 {
                None
            } else {
                Some(*it.next().ok_or(Error::Truncated)?)
            };
        }
    }
    state.pricing_null = m.null_mask & TICKER_PRICING_NULL_BIT != 0;
    state.t = ts.checked_add(m.t_offset).ok_or(Error::DecimalOverflow)?;
    Ok(())
}

impl Replica {
    pub fn new() -> Self {
        Self::default()
    }

    fn name(&self, id: u32) -> Result<&str, Error> {
        self.names.get(&id).map(String::as_str).ok_or(Error::UnknownInstrument)
    }

    pub fn book(&self, instrument: u32, chan: u8) -> Option<&BookSnapshot> {
        self.books.get(&(instrument, chan)).filter(|b| !b.stale).map(|b| &b.book)
    }

    pub fn instrument_id(&self, name: &str) -> Option<u32> {
        self.names.iter().find(|(_, n)| n.as_str() == name).map(|(i, _)| *i)
    }

    pub fn apply(&mut self, m: &Message, render: bool) -> Result<Applied, Error> {
        let h = &m.header;
        match &m.body {
            Body::InstrumentDef { name } => {
                self.names.insert(h.instrument, name.clone());
                Ok(Applied::Other)
            }
            Body::BookSnapshot(b) => {
                let key = (h.instrument, b.chan);
                if let Some(s) = self.books.get(&key) {
                    if !s.stale && h.seq <= s.seq && h.flags & FLAG_RESYNC == 0 {
                        return Ok(Applied::Ignored);
                    }
                }
                self.books.insert(key, BookRep { seq: h.seq, ts: h.ts, stale: false, book: b.clone() });
                self.render_book(key, render)
            }
            Body::BookDelta(d) => {
                let key = (h.instrument, d.chan);
                let stream = StreamId { instrument: h.instrument, family: Family::Book, chan: d.chan };
                let Some(s) = self.books.get_mut(&key) else { return Ok(Applied::Ignored) };
                if s.stale || h.seq <= s.seq {
                    return Ok(Applied::Ignored);
                }
                if h.seq != s.seq + 1 {
                    s.stale = true;
                    self.gaps += 1;
                    return Ok(Applied::Gap { stream, expected: s.seq + 1, got: h.seq });
                }
                // Apply to a copy, so a bad delta leaves the book marked stale
                // rather than half-updated.
                let mut book = std::mem::take(&mut s.book);
                let res = (|| -> Result<(), Error> {
                    for op in &d.ops {
                        let side = if op.side == Side::Bid { &mut book.bids } else { &mut book.asks };
                        apply_side(side, op.side, op)?;
                    }
                    let id = book.publish_id as i128 + d.publish_id_delta as i128;
                    book.publish_id = u64::try_from(id).map_err(|_| Error::DecimalOverflow)?;
                    Ok(())
                })();
                s.book = book;
                if let Err(e) = res {
                    s.stale = true;
                    return Err(e);
                }
                s.seq = h.seq;
                s.ts = s.ts.checked_add(h.ts).ok_or(Error::DecimalOverflow)?;
                self.render_book(key, render)
            }
            Body::TickerSnapshot(t) => {
                let key = (h.instrument, t.chan);
                if let Some(s) = self.tickers.get(&key) {
                    if !s.stale && h.seq <= s.seq && h.flags & FLAG_RESYNC == 0 {
                        return Ok(Applied::Ignored);
                    }
                }
                let mut state = TickerState::default();
                apply_ticker(&mut state, t, h.ts)?;
                self.tickers.insert(key, TickerRep { seq: h.seq, ts: h.ts, stale: false, state });
                self.render_ticker(key, render)
            }
            Body::TickerDelta(t) => {
                let key = (h.instrument, t.chan);
                let stream = StreamId { instrument: h.instrument, family: Family::Ticker, chan: t.chan };
                let Some(s) = self.tickers.get_mut(&key) else { return Ok(Applied::Ignored) };
                if s.stale || h.seq <= s.seq {
                    return Ok(Applied::Ignored);
                }
                if h.seq != s.seq + 1 {
                    s.stale = true;
                    self.gaps += 1;
                    return Ok(Applied::Gap { stream, expected: s.seq + 1, got: h.seq });
                }
                // Apply to a copy so an error leaves the stream stale, not half-updated.
                let res = s.ts.checked_add(h.ts).ok_or(Error::DecimalOverflow).and_then(|ts| {
                    let mut state = s.state.clone();
                    apply_ticker(&mut state, t, ts)?;
                    Ok((ts, state))
                });
                match res {
                    Ok((ts, state)) => {
                        s.ts = ts;
                        s.state = state;
                        s.seq = h.seq;
                    }
                    Err(e) => {
                        s.stale = true;
                        return Err(e);
                    }
                }
                self.render_ticker(key, render)
            }
            Body::Trades { trades, .. } => {
                let stream = StreamId { instrument: h.instrument, family: Family::Trades, chan: 0 };
                let seq = self.trades.entry(h.instrument).or_insert(0);
                let gap = *seq != 0 && h.seq != *seq + 1;
                let expected = *seq + 1;
                *seq = h.seq;
                if gap {
                    // Trades are events, not state, so a snapshot cannot
                    // restore them. Report the gap and keep going.
                    self.gaps += 1;
                    return Ok(Applied::Gap { stream, expected, got: h.seq });
                }
                if !render {
                    return Ok(Applied::Updated(None));
                }
                let mut out = String::with_capacity(512);
                render_trades(&mut out, self.name(h.instrument)?, trades);
                Ok(Applied::Updated(Some(out)))
            }
            Body::RawJson(b) => {
                if !render {
                    return Ok(Applied::Updated(None));
                }
                let s = std::str::from_utf8(b).map_err(|_| Error::BadUtf8)?;
                Ok(Applied::Updated(Some(s.to_owned())))
            }
            _ => Ok(Applied::Other),
        }
    }

    fn render_book(&self, key: (u32, u8), render: bool) -> Result<Applied, Error> {
        if !render {
            return Ok(Applied::Updated(None));
        }
        let s = self.books.get(&key).ok_or(Error::MissingState)?;
        let mut out = String::with_capacity(1024);
        render_book(&mut out, self.name(key.0)?, s.ts, &s.book)?;
        Ok(Applied::Updated(Some(out)))
    }

    fn render_ticker(&self, key: (u32, u8), render: bool) -> Result<Applied, Error> {
        if !render {
            return Ok(Applied::Updated(None));
        }
        let s = self.tickers.get(&key).ok_or(Error::MissingState)?;
        let mut out = String::with_capacity(1024);
        render_ticker(&mut out, self.name(key.0)?, key.1, s.ts, &s.state)?;
        Ok(Applied::Updated(Some(out)))
    }
}

//! SYNTHETIC market data. Nothing here came from a live exchange.
//!
//! The field shapes follow the public AsyncAPI and OpenAPI documents. The
//! numbers are invented by a seeded simulation, so every result built on this
//! is a result about the simulation. The regime parameters below were fixed and
//! committed before any size was measured (see docs/synthetic-data.md).

use k256::ecdsa::SigningKey;
use mdbin::dec::Dec;
use mdbin::json::{eip55, id_text};
use mdbin::msg::{Addr, Id, NewOrder, OrderType, Tif};
use mdbin::order::{address_of, order_digest, sign_digest, SignedContext, CHAIN_TESTNET};
use rand::{Rng, SeedableRng};
use rand_chacha::ChaCha8Rng;
use rand_distr::{Distribution, Normal, Poisson};
use std::fmt::Write;

pub const TICK_MS: i64 = 100;
pub const START_MS: i64 = 1_790_000_000_000;
const SECONDS_PER_YEAR: f64 = 365.25 * 86_400.0;

/// One market condition. All of these are assumptions, not measurements.
#[derive(Debug, Clone, Copy)]
pub struct Regime {
    pub name: &'static str,
    /// Annualised volatility of the underlying.
    pub vol: f64,
    /// A market maker requotes an option once theoretical value moves this
    /// many price ticks away from its resting quote.
    pub requote_ticks: i64,
    /// Baseline quote refreshes per option per second, independent of spot.
    pub option_refresh_per_s: f64,
    /// Chance per level per tick that a perp level changes size.
    pub perp_level_change: f64,
    pub perp_trades_per_s: f64,
    pub option_trades_per_s: f64,
}

pub const REGIMES: [Regime; 3] = [
    Regime {
        name: "calm",
        vol: 0.40,
        requote_ticks: 4,
        option_refresh_per_s: 0.05,
        perp_level_change: 0.02,
        perp_trades_per_s: 0.2,
        option_trades_per_s: 0.002,
    },
    Regime {
        name: "base",
        vol: 0.70,
        requote_ticks: 3,
        option_refresh_per_s: 0.2,
        perp_level_change: 0.06,
        perp_trades_per_s: 0.5,
        option_trades_per_s: 0.01,
    },
    Regime {
        name: "busy",
        vol: 1.20,
        requote_ticks: 2,
        option_refresh_per_s: 0.8,
        perp_level_change: 0.15,
        perp_trades_per_s: 2.0,
        option_trades_per_s: 0.05,
    },
];

pub fn regime(name: &str) -> Option<Regime> {
    REGIMES.iter().copied().find(|r| r.name == name)
}

#[derive(Debug, Clone, Copy)]
pub struct FeedConfig {
    pub seed: u64,
    /// The documented depths are 1, 10, 20, 100.
    pub depth: usize,
    /// The documented ticker intervals are 100 and 1000 ms.
    pub ticker_ms: i64,
    /// Fractional digits in greeks and implied vols. The docs say decimals
    /// have "up to 12"; what is typical is unknown, so this is swept.
    pub greek_digits: u8,
    pub expiries: usize,
    pub strikes: usize,
}

impl Default for FeedConfig {
    fn default() -> Self {
        Self { seed: 7, depth: 20, ticker_ms: 1000, greek_digits: 12, expiries: 4, strikes: 11 }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EventKind {
    Book,
    Ticker,
    Trades,
}

#[derive(Debug, Clone)]
pub struct Event {
    pub kind: EventKind,
    pub instrument: u32,
    pub json: String,
}

struct Underlying {
    name: &'static str,
    spot: f64,
    price_dp: u8,
    perp_tick: i64,
    option_tick: i64,
    amount_dp: u8,
}

/// Prices are integer ticks of `10^-dp`.
const UNDERLYINGS: [Underlying; 2] = [
    Underlying { name: "ETH", spot: 3000.0, price_dp: 2, perp_tick: 1, option_tick: 10, amount_dp: 2 },
    Underlying { name: "BTC", spot: 90000.0, price_dp: 1, perp_tick: 1, option_tick: 10, amount_dp: 3 },
];

struct OptionSpec {
    expiry_years: f64,
    strike: f64,
    is_call: bool,
    base_iv: f64,
}

struct Instrument {
    name: String,
    under: usize,
    option: Option<OptionSpec>,
    bids: Vec<(i64, i64)>,
    asks: Vec<(i64, i64)>,
    /// Value the resting option quotes were placed around, in price units.
    quoted_theo: i64,
    publish_id: u64,
    stats_n: u64,
    stats_c: i64,
    stats_v: i64,
    oi: i64,
}

pub struct Feed {
    regime: Regime,
    cfg: FeedConfig,
    rng: ChaCha8Rng,
    spots: Vec<f64>,
    spot0: Vec<f64>,
    instruments: Vec<Instrument>,
    wallets: Vec<[u8; 20]>,
    now: i64,
    tick_no: u64,
    perp_trades: Poisson<f64>,
    option_trades: Poisson<f64>,
    option_refresh: f64,
    shock: Normal<f64>,
}

fn norm_cdf(x: f64) -> f64 {
    // Abramowitz and Stegun 7.1.26. Accuracy is irrelevant here: this only
    // needs to move plausibly with spot.
    let t = 1.0 / (1.0 + 0.2316419 * x.abs());
    let d = 0.39894228 * (-x * x / 2.0).exp();
    let p = d * t * (0.3193815 + t * (-0.3565638 + t * (1.781478 + t * (-1.821256 + t * 1.330274))));
    if x >= 0.0 { 1.0 - p } else { p }
}

struct Greeks {
    price: f64,
    delta: f64,
    gamma: f64,
    vega: f64,
    theta: f64,
    rho: f64,
}

fn black_scholes(s: f64, k: f64, t: f64, iv: f64, call: bool) -> Greeks {
    let sq = iv * t.sqrt();
    let d1 = ((s / k).ln() + 0.5 * iv * iv * t) / sq;
    let d2 = d1 - sq;
    let pdf = 0.39894228 * (-d1 * d1 / 2.0).exp();
    let (price, delta) = if call {
        (s * norm_cdf(d1) - k * norm_cdf(d2), norm_cdf(d1))
    } else {
        (k * norm_cdf(-d2) - s * norm_cdf(-d1), norm_cdf(d1) - 1.0)
    };
    Greeks {
        price,
        delta,
        gamma: pdf / (s * sq),
        vega: s * pdf * t.sqrt() / 100.0,
        theta: -s * pdf * iv / (2.0 * t.sqrt()) / 365.0,
        rho: k * t * norm_cdf(if call { d2 } else { -d2 }) / 100.0 * if call { 1.0 } else { -1.0 },
    }
}

/// Text form: scaled, rounded, trailing zeros trimmed, matching the
/// documented examples such as `"3100.5"` and `"1.5"`.
fn fmt_f(v: f64, dp: u8) -> String {
    let m = (v * 10f64.powi(dp as i32)).round() as i128;
    Dec { m, s: dp }.trimmed().to_text()
}

fn fmt_i(m: i64, dp: u8) -> String {
    Dec { m: m as i128, s: dp }.trimmed().to_text()
}

fn expiry_text(index: usize) -> String {
    // Fixed labels: the names only need the documented shape.
    ["20261009", "20261016", "20261030", "20261127", "20261225", "20270326"][index % 6].to_string()
}

impl Feed {
    pub fn new(regime: Regime, cfg: FeedConfig) -> Self {
        let mut rng = ChaCha8Rng::seed_from_u64(cfg.seed);
        let mut instruments = Vec::new();
        let days = [7.0, 14.0, 30.0, 60.0, 90.0, 180.0];
        for (u, und) in UNDERLYINGS.iter().enumerate() {
            instruments.push(Instrument {
                name: format!("{}-PERP", und.name),
                under: u,
                option: None,
                bids: vec![],
                asks: vec![],
                quoted_theo: 0,
                publish_id: rng.gen_range(1_000_000..2_000_000),
                stats_n: rng.gen_range(5_000..20_000),
                stats_c: rng.gen_range(1_000_000..9_000_000),
                stats_v: rng.gen_range(1_000_000..9_000_000),
                oi: rng.gen_range(1_000_000..9_000_000),
            });
            for e in 0..cfg.expiries {
                let half = (cfg.strikes / 2) as i64;
                for k in -half..=half {
                    let step = und.spot * 0.05;
                    let strike = ((und.spot + k as f64 * step) / 100.0).round() * 100.0;
                    for is_call in [true, false] {
                        let cp = if is_call { 'C' } else { 'P' };
                        instruments.push(Instrument {
                            name: format!("{}-{}-{}-{}", und.name, expiry_text(e), strike as i64, cp),
                            under: u,
                            option: Some(OptionSpec {
                                expiry_years: days[e % 6] / 365.25,
                                strike,
                                is_call,
                                base_iv: 0.55 + 0.02 * (k as f64).abs(),
                            }),
                            bids: vec![],
                            asks: vec![],
                            quoted_theo: 0,
                            publish_id: rng.gen_range(1_000_000..2_000_000),
                            stats_n: rng.gen_range(0..800),
                            stats_c: rng.gen_range(0..200_000),
                            stats_v: rng.gen_range(0..200_000),
                            oi: rng.gen_range(0..900_000),
                        });
                    }
                }
            }
        }
        let wallets = (0..64).map(|_| rng.gen()).collect();
        let spots: Vec<f64> = UNDERLYINGS.iter().map(|u| u.spot).collect();
        let dt = TICK_MS as f64 / 1000.0;
        let mut feed = Self {
            regime,
            cfg,
            spot0: spots.clone(),
            spots,
            instruments,
            wallets,
            now: START_MS,
            tick_no: 0,
            perp_trades: Poisson::new(regime.perp_trades_per_s * dt).expect("positive rate"),
            option_trades: Poisson::new(regime.option_trades_per_s * dt).expect("positive rate"),
            option_refresh: regime.option_refresh_per_s * dt,
            shock: Normal::new(0.0, regime.vol * (dt / SECONDS_PER_YEAR).sqrt()).expect("positive vol"),
            rng,
        };
        for i in 0..feed.instruments.len() {
            feed.seed_book(i);
        }
        feed
    }

    pub fn instrument_names(&self) -> Vec<&str> {
        self.instruments.iter().map(|i| i.name.as_str()).collect()
    }

    pub fn now_ms(&self) -> i64 {
        self.now
    }

    fn theo_ticks(&self, i: usize) -> i64 {
        let ins = &self.instruments[i];
        let und = &UNDERLYINGS[ins.under];
        let unit = 10f64.powi(und.price_dp as i32);
        let s = self.spots[ins.under];
        match &ins.option {
            None => (s * unit).round() as i64,
            Some(o) => {
                let g = black_scholes(s, o.strike, o.expiry_years, o.base_iv, o.is_call);
                (g.price.max(0.0) * unit).round() as i64
            }
        }
    }

    fn fresh_qty(&mut self, under: usize) -> i64 {
        let dp = UNDERLYINGS[under].amount_dp as i32;
        let lots: f64 = self.rng.gen_range(0.05..12.0);
        ((lots * 10f64.powi(dp)).round() as i64).max(1)
    }

    fn seed_book(&mut self, i: usize) {
        let under = self.instruments[i].under;
        let theo = self.theo_ticks(i);
        let is_option = self.instruments[i].option.is_some();
        let (tick, levels) = if is_option {
            let lv = self.rng.gen_range(1..=5usize);
            (UNDERLYINGS[under].option_tick, lv)
        } else {
            (UNDERLYINGS[under].perp_tick, self.cfg.depth.max(20))
        };
        let spread = if is_option { tick * self.rng.gen_range(3..12) } else { tick * 2 };
        let mut bids = Vec::new();
        let mut asks = Vec::new();
        let base = (theo / tick) * tick;
        for l in 0..levels as i64 {
            let gap = tick * (1 + l * if is_option { 3 } else { 1 });
            let bid = base - spread / 2 - gap;
            if bid > 0 {
                bids.push((bid, self.fresh_qty(under)));
            }
            asks.push((base + spread / 2 + gap, self.fresh_qty(under)));
        }
        let ins = &mut self.instruments[i];
        ins.bids = bids;
        ins.asks = asks;
        ins.quoted_theo = theo;
    }

    fn step_perp(&mut self, i: usize) {
        let under = self.instruments[i].under;
        let tick = UNDERLYINGS[under].perp_tick;
        let theo = self.theo_ticks(i);
        let prev = self.instruments[i].quoted_theo;
        let cap = self.cfg.depth.max(20);
        if theo != prev {
            // Quotes are re-anchored to the new mid: the inside moves, levels
            // deeper in the book mostly stay where they were.
            let shift = theo - prev;
            let new_best_bid = theo - tick;
            let new_best_ask = theo + tick;
            let q1 = self.fresh_qty(under);
            let q2 = self.fresh_qty(under);
            let ins = &mut self.instruments[i];
            ins.bids.retain(|l| l.0 < new_best_bid);
            ins.asks.retain(|l| l.0 > new_best_ask);
            ins.bids.insert(0, (new_best_bid, q1));
            ins.asks.insert(0, (new_best_ask, q2));
            if shift.abs() > 4 * tick {
                ins.bids.truncate(cap);
                ins.asks.truncate(cap);
            }
            ins.quoted_theo = theo;
        }
        for side in 0..2 {
            let n = if side == 0 { self.instruments[i].bids.len() } else { self.instruments[i].asks.len() };
            for l in 0..n {
                if self.rng.gen_bool(self.regime.perp_level_change) {
                    let q = self.fresh_qty(under);
                    let ins = &mut self.instruments[i];
                    let s = if side == 0 { &mut ins.bids } else { &mut ins.asks };
                    s[l].1 = q;
                }
            }
            // Refill to full depth with levels behind the last one.
            loop {
                let ins = &self.instruments[i];
                let s = if side == 0 { &ins.bids } else { &ins.asks };
                if s.len() >= cap {
                    break;
                }
                let last = s.last().map_or(theo, |l| l.0);
                let gap = tick * self.rng.gen_range(1..4);
                let q = self.fresh_qty(under);
                let ins = &mut self.instruments[i];
                let s = if side == 0 { &mut ins.bids } else { &mut ins.asks };
                s.push((if side == 0 { last - gap } else { last + gap }, q));
            }
            let ins = &mut self.instruments[i];
            let s = if side == 0 { &mut ins.bids } else { &mut ins.asks };
            s.truncate(cap);
        }
    }

    fn step_option(&mut self, i: usize) {
        let under = self.instruments[i].under;
        let tick = UNDERLYINGS[under].option_tick;
        let theo = self.theo_ticks(i);
        let moved = (theo - self.instruments[i].quoted_theo).abs();
        if moved >= self.regime.requote_ticks * tick {
            self.seed_book(i);
            return;
        }
        if self.rng.gen_bool(self.option_refresh) {
            let q = self.fresh_qty(under);
            let pick_bid = self.rng.gen_bool(0.5);
            let r: f64 = self.rng.gen();
            let ins = &mut self.instruments[i];
            let s = if pick_bid { &mut ins.bids } else { &mut ins.asks };
            if !s.is_empty() {
                let idx = ((r * s.len() as f64) as usize).min(s.len() - 1);
                s[idx].1 = q;
            }
        }
    }

    fn book_json(&mut self, i: usize) -> String {
        let depth = self.cfg.depth;
        let now = self.now;
        let ins = &mut self.instruments[i];
        ins.publish_id += 1;
        let und = &UNDERLYINGS[ins.under];
        let side = |levels: &[(i64, i64)], out: &mut String| {
            out.push('[');
            for (n, (p, q)) in levels.iter().take(depth).enumerate() {
                if n > 0 {
                    out.push(',');
                }
                let _ = write!(out, r#"["{}","{}"]"#, fmt_i(*p, und.price_dp), fmt_i(*q, und.amount_dp));
            }
            out.push(']');
        };
        let mut s = String::with_capacity(1024);
        let _ = write!(s, r#"{{"method":"subscription","params":{{"channel":"orderbook.{}.1.{depth}","data":{{"asks":"#, ins.name);
        side(&ins.asks, &mut s);
        s.push_str(r#","bids":"#);
        side(&ins.bids, &mut s);
        let _ = write!(s, r#","instrument_name":"{}","publish_id":{},"timestamp":{now}}}}}}}"#, ins.name, ins.publish_id);
        s
    }

    fn ticker_json(&mut self, i: usize) -> String {
        let now = self.now;
        let interval = self.cfg.ticker_ms;
        let gd = self.cfg.greek_digits;
        let ins = &self.instruments[i];
        let und = &UNDERLYINGS[ins.under];
        let pd = und.price_dp;
        let spot = self.spots[ins.under];
        let (ba, aa) = (ins.bids.first().copied().unwrap_or((0, 0)), ins.asks.first().copied().unwrap_or((0, 0)));
        let (mark, funding, pricing) = match &ins.option {
            None => (spot * 1.0002, format!("\"{}\"", fmt_f(0.0000125 + (spot / self.spot0[ins.under] - 1.0) * 1e-4, 8)), "null".to_owned()),
            Some(o) => {
                let g = black_scholes(spot, o.strike, o.expiry_years, o.base_iv, o.is_call);
                let df = (-0.04 * o.expiry_years).exp();
                let p = format!(
                    r#"{{"ai":"{}","bi":"{}","d":"{}","df":"{}","f":"{}","g":"{}","i":"{}","m":"{}","r":"{}","t":"{}","v":"{}"}}"#,
                    fmt_f(o.base_iv + 0.012, gd),
                    fmt_f(o.base_iv - 0.012, gd),
                    fmt_f(g.delta, gd),
                    fmt_f(df, gd),
                    fmt_f(spot * 1.001, pd),
                    fmt_f(g.gamma, gd),
                    fmt_f(o.base_iv, gd),
                    fmt_f(g.price.max(0.0), gd),
                    fmt_f(g.rho, gd),
                    fmt_f(g.theta, gd),
                    fmt_f(g.vega, gd),
                );
                (g.price.max(0.0), "null".to_owned(), p)
            }
        };
        let band = if ins.option.is_some() { 0.5 } else { 0.03 };
        let mut s = String::with_capacity(1024);
        let _ = write!(
            s,
            r#"{{"method":"subscription","params":{{"channel":"ticker_slim.{}.{interval}","data":{{"instrument_ticker":{{"A":"{}","B":"{}","I":"{}","M":"{}","a":"{}","b":"{}","f":{funding},"maxp":"{}","minp":"{}","option_pricing":{pricing},"stats":{{"c":"{}","h":"{}","l":"{}","n":{},"oi":"{}","p":"{}","pr":"{}","v":"{}"}},"t":{now}}},"timestamp":{now}}}}}}}"#,
            ins.name,
            fmt_i(aa.1, und.amount_dp),
            fmt_i(ba.1, und.amount_dp),
            fmt_f(spot, pd),
            fmt_f(mark, pd),
            fmt_i(aa.0, pd),
            fmt_i(ba.0, pd),
            fmt_f(mark * (1.0 + band), pd),
            fmt_f((mark * (1.0 - band)).max(0.0), pd),
            fmt_i(ins.stats_c, und.amount_dp),
            fmt_f(mark * 1.04, pd),
            fmt_f(mark * 0.96, pd),
            ins.stats_n,
            fmt_i(ins.oi, und.amount_dp),
            fmt_f((spot / self.spot0[ins.under] - 1.0) * 100.0, 4),
            fmt_i(ins.stats_v / 10, 2),
            fmt_i(ins.stats_v, 2),
        );
        s
    }

    fn trades_json(&mut self, i: usize, n: u64) -> String {
        let now = self.now;
        let (name, pd, ad, spot, top) = {
            let ins = &self.instruments[i];
            let und = &UNDERLYINGS[ins.under];
            (ins.name.clone(), und.price_dp, und.amount_dp, self.spots[ins.under], ins.asks.first().copied().unwrap_or((1, 1)))
        };
        let mut s = String::with_capacity(600 * n as usize);
        let _ = write!(s, r#"{{"method":"subscription","params":{{"channel":"trades.{name}","data":["#);
        for k in 0..n {
            if k > 0 {
                s.push(',');
            }
            let wallet = self.wallets[self.rng.gen_range(0..self.wallets.len())];
            let id: [u8; 16] = self.rng.gen();
            let buy = self.rng.gen_bool(0.5);
            let maker = self.rng.gen_bool(0.5);
            let amount = self.fresh_qty(self.instruments[i].under);
            let price = top.0;
            let fee = self.rng.gen_range(0.01..4.0);
            let pnl = self.rng.gen_range(-50.0..50.0);
            let _ = write!(
                s,
                r#"{{"direction":"{}","expected_rebate":"0","extra_fee":"0","index_price":"{}","instrument_name":"{name}","liquidity_role":"{}","mark_price":"{}","quote_id":null,"realized_pnl":"{}","realized_pnl_excl_fees":"{}","rfq_id":null,"subaccount_id":{},"timestamp":{now},"trade_amount":"{}","trade_fee":"{}","trade_id":"{}","trade_price":"{}","wallet":"0x{}"}}"#,
                if buy { "buy" } else { "sell" },
                fmt_f(spot, pd),
                if maker { "maker" } else { "taker" },
                fmt_i(price, pd),
                fmt_f(pnl - fee, 6),
                fmt_f(pnl, 6),
                self.rng.gen_range(1000..90000),
                fmt_i(amount, ad),
                fmt_f(fee, 6),
                id_text(&Id::Uuid(id)),
                fmt_i(price, pd),
                eip55(&wallet),
            );
            let ins = &mut self.instruments[i];
            ins.stats_n += 1;
            ins.stats_c += amount;
            ins.stats_v += amount;
        }
        s.push_str("]}}");
        s
    }

    /// Advances one 100 ms tick and appends everything published in it.
    /// Assumption: books publish every tick whether or not they changed. The
    /// docs say "periodically", without saying whether unchanged books are
    /// skipped; docs/synthetic-data.md explains what changes if they are.
    pub fn tick(&mut self, out: &mut Vec<Event>) {
        for u in 0..self.spots.len() {
            let z = self.shock.sample(&mut self.rng);
            self.spots[u] *= 1.0 + z;
        }
        self.now += TICK_MS;
        self.tick_no += 1;
        let ticker_due = (self.tick_no as i64 * TICK_MS) % self.cfg.ticker_ms == 0;
        for i in 0..self.instruments.len() {
            let is_option = self.instruments[i].option.is_some();
            if is_option {
                self.step_option(i);
            } else {
                self.step_perp(i);
            }
            let dist = if is_option { self.option_trades } else { self.perp_trades };
            let n = dist.sample(&mut self.rng) as u64;
            if n > 0 {
                let json = self.trades_json(i, n);
                out.push(Event { kind: EventKind::Trades, instrument: i as u32, json });
            }
            let json = self.book_json(i);
            out.push(Event { kind: EventKind::Book, instrument: i as u32, json });
            if ticker_due {
                let json = self.ticker_json(i);
                out.push(Event { kind: EventKind::Ticker, instrument: i as u32, json });
            }
        }
    }
}

/// Test signing key. It holds nothing and exists only to produce valid
/// signatures for encoding tests.
pub fn test_key() -> SigningKey {
    SigningKey::from_slice(&[0x42u8; 32]).expect("valid scalar")
}

/// A synthetic signed order, plus what a gateway would look up to verify it.
pub fn sample_order(rng: &mut ChaCha8Rng, rpc_id: u64) -> (String, NewOrder, SignedContext) {
    let key = test_key();
    let owner = address_of(key.verifying_key());
    let ctx = SignedContext { asset: [0xAB; 20], sub_id: 39614082545622357598554435968, owner, chain_id: CHAIN_TESTNET };
    let strike = rng.gen_range(20..40) * 100;
    let name = format!("ETH-20261030-{strike}-C");
    let mut o = NewOrder {
        rpc_id,
        subaccount_id: rng.gen_range(1000..90000),
        is_buy: rng.gen_bool(0.5),
        limit_price: Dec { m: rng.gen_range(1000..500_000), s: 1 }.trimmed(),
        amount: Dec { m: rng.gen_range(1..5000), s: 2 }.trimmed(),
        max_fee: Dec { m: rng.gen_range(1..300), s: 2 }.trimmed(),
        nonce: 1_790_000_000_000_000_000u128 + rng.gen_range(0..1_000_000_000u128),
        signer: Addr::Checksummed(owner),
        signature: [0; 65],
        signature_expiry_sec: 1_790_000_300,
        order_type: Some(OrderType::Limit),
        time_in_force: Some(Tif::Gtc),
        reduce_only: None,
        mmp: None,
        label: None,
    };
    let digest = order_digest(&o, &ctx).expect("valid order");
    o.signature = sign_digest(&key, &digest).expect("valid key");
    (name, o, ctx)
}

pub fn order_rng(seed: u64) -> ChaCha8Rng {
    ChaCha8Rng::seed_from_u64(seed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use mdbin::msg::{decode_frame, MAGIC_VERSION};
    use mdbin::replica::{Applied, Replica};
    use mdbin::transcode::{Config, Outcome, Transcoder};

    #[test]
    fn generator_is_deterministic() {
        let run = || {
            let mut f = Feed::new(REGIMES[1], FeedConfig::default());
            let mut v = Vec::new();
            for _ in 0..20 {
                f.tick(&mut v);
            }
            v.into_iter().map(|e| e.json).collect::<Vec<_>>()
        };
        assert_eq!(run(), run());
    }

    #[test]
    fn every_message_round_trips_exactly_in_every_regime() {
        for r in REGIMES {
            let mut f = Feed::new(r, FeedConfig { ticker_ms: 100, ..Default::default() });
            let mut t = Transcoder::new(Config { deltas: true, suppress_unchanged: false });
            let mut rep = Replica::new();
            let mut events = Vec::new();
            for _ in 0..30 {
                events.clear();
                f.tick(&mut events);
                for e in &events {
                    let mut frame = vec![MAGIC_VERSION];
                    let o = t.push(&e.json, &mut frame).unwrap();
                    assert!(!matches!(o, Outcome::Raw), "raw fallback on {}", e.json);
                    let mut rebuilt = None;
                    for m in decode_frame(&frame).unwrap() {
                        if let Applied::Updated(Some(s)) = rep.apply(&m, true).unwrap() {
                            rebuilt = Some(s);
                        }
                    }
                    assert_eq!(rebuilt.as_deref(), Some(e.json.as_str()));
                }
            }
        }
    }

    #[test]
    fn books_are_sorted_and_uncrossed() {
        let mut f = Feed::new(REGIMES[2], FeedConfig::default());
        let mut v = Vec::new();
        for _ in 0..200 {
            f.tick(&mut v);
        }
        for ins in &f.instruments {
            assert!(ins.bids.windows(2).all(|w| w[0].0 > w[1].0), "{}", ins.name);
            assert!(ins.asks.windows(2).all(|w| w[0].0 < w[1].0), "{}", ins.name);
            if let (Some(b), Some(a)) = (ins.bids.first(), ins.asks.first()) {
                assert!(b.0 < a.0, "{} crossed", ins.name);
            }
        }
    }

    #[test]
    fn sample_orders_verify() {
        let mut r = order_rng(1);
        for i in 0..5 {
            let (_, o, c) = sample_order(&mut r, i);
            assert!(mdbin::order::verify_order(&o, &c).unwrap());
        }
    }
}

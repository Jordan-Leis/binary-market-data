//! Byte counts for every encoding on the same SYNTHETIC stream.
//! Deterministic: the output is identical on every machine and run.

use bench::{wire, Pmd};
use mdbin::msg::MAGIC_VERSION;
use mdbin::transcode::{Config, Outcome, Transcoder};
use serde_json::{json, Value};
use std::collections::HashMap;
use synth::{Event, EventKind, Feed, FeedConfig, Regime, REGIMES};

const WARMUP_TICKS: u64 = 50;
const MEASURE_TICKS: u64 = 600;

#[derive(Default, Clone)]
struct Tally {
    payload: u64,
    messages: u64,
    frames: u64,
    wire: u64,
    by_family: [u64; 3],
}

impl Tally {
    fn add(&mut self, kind: EventKind, bytes: u64) {
        self.payload += bytes;
        self.messages += 1;
        self.by_family[fam(kind)] += bytes;
    }
    fn frame(&mut self, bytes: u64) {
        self.frames += 1;
        self.wire += wire::bytes_for_frame(bytes);
    }
    fn json(&self, secs: f64) -> Value {
        json!({
            "payload_bytes": self.payload,
            "messages": self.messages,
            "frames": self.frames,
            "wire_bytes_modelled": self.wire,
            "payload_bytes_per_s": (self.payload as f64 / secs).round(),
            "wire_bytes_per_s_modelled": (self.wire as f64 / secs).round(),
            "by_family_payload": {"book": self.by_family[0], "ticker": self.by_family[1], "trades": self.by_family[2]},
        })
    }
}

fn fam(k: EventKind) -> usize {
    match k {
        EventKind::Book => 0,
        EventKind::Ticker => 1,
        EventKind::Trades => 2,
    }
}

/// One subscriber connection under a given encoding.
struct BinConn {
    t: Transcoder,
    tally: Tally,
    batch: u64,
    pmd: Option<Pmd>,
    pmd_batch: Vec<u8>,
    conflate_every: u64,
}

impl BinConn {
    fn new(cfg: Config, pmd: bool, conflate_every: u64) -> Self {
        Self { t: Transcoder::new(cfg), tally: Tally::default(), batch: 0, pmd: pmd.then(|| Pmd::new(true)), pmd_batch: vec![], conflate_every }
    }

    fn push(&mut self, e: &Event, tick: u64, measure: bool, scratch: &mut Vec<u8>) -> bool {
        if e.kind == EventKind::Book && tick % self.conflate_every != 0 {
            return true;
        }
        scratch.clear();
        let o = self.t.push(&e.json, scratch).expect("encode");
        if measure && !scratch.is_empty() {
            self.tally.add(e.kind, scratch.len() as u64);
            self.batch += scratch.len() as u64;
            if self.pmd.is_some() {
                self.pmd_batch.extend_from_slice(scratch);
            }
        }
        !matches!(o, Outcome::Raw)
    }

    /// One WebSocket message per tick: the batch, behind one magic byte.
    fn end_tick(&mut self, measure: bool) {
        if measure && self.batch > 0 {
            match &mut self.pmd {
                None => {
                    self.tally.payload += 1;
                    self.tally.frame(self.batch + 1);
                }
                Some(p) => {
                    let mut f = vec![MAGIC_VERSION];
                    f.extend_from_slice(&self.pmd_batch);
                    let n = p.compress(&f).len() as u64;
                    // Report the compressed size as the payload.
                    self.tally.payload = self.tally.payload + n - self.batch;
                    self.tally.frame(n);
                }
            }
        }
        self.batch = 0;
        self.pmd_batch.clear();
    }
}

struct Stats {
    books: u64,
    books_unchanged: u64,
    levels: u64,
    book_json_bytes: u64,
    raw_fallbacks: u64,
}

fn levels_of(json: &str) -> &str {
    let a = json.find("\"asks\":").unwrap_or(0);
    let b = json.find(",\"instrument_name\"").unwrap_or(json.len());
    &json[a..b]
}

fn run(regime: Regime, cfg: FeedConfig) -> Value {
    let secs = MEASURE_TICKS as f64 * synth::TICK_MS as f64 / 1000.0;
    let mut feed = Feed::new(regime, cfg);
    let mut events = Vec::new();

    let mut json_t = Tally::default();
    let mut pmd_full = Pmd::new(true);
    let mut pmd_full_t = Tally::default();
    let mut pmd_noctx = Pmd::new(false);
    let mut pmd_noctx_t = Tally::default();
    let mut pmd_single: HashMap<u32, Pmd> = HashMap::new();
    let mut pmd_single_t = Tally::default();
    let mut zstd_t = Tally::default();
    let mut sbe_book_bytes = 0u64;
    let mut train: Vec<Vec<u8>> = Vec::new();
    let mut zc: Option<zstd::bulk::Compressor<'static>> = None;
    let mut dict_len = 0usize;

    let exact = Config { deltas: false, suppress_unchanged: false };
    let delta = Config { deltas: true, suppress_unchanged: false };
    let suppress = Config { deltas: true, suppress_unchanged: true };
    let mut conns: Vec<(&str, BinConn)> = vec![
        ("binary_snapshots", BinConn::new(exact, false, 1)),
        ("binary_deltas", BinConn::new(delta, false, 1)),
        ("binary_deltas_suppress_unchanged", BinConn::new(suppress, false, 1)),
        ("binary_deltas_suppress_conflate_1s", BinConn::new(suppress, false, 10)),
        ("binary_deltas_plus_pmd", BinConn::new(delta, true, 1)),
    ];
    // One-instrument subscribers: one connection per instrument, binary deltas.
    let mut single_bin: HashMap<u32, BinConn> = HashMap::new();
    let mut single_bin_t = Tally::default();
    let mut single_json_t = Tally::default();

    let mut last_book: HashMap<u32, String> = HashMap::new();
    let mut st = Stats { books: 0, books_unchanged: 0, levels: 0, book_json_bytes: 0, raw_fallbacks: 0 };
    let mut scratch = Vec::with_capacity(4096);
    let mut join_bytes = 0u64;

    for tick in 0..WARMUP_TICKS + MEASURE_TICKS {
        events.clear();
        feed.tick(&mut events);
        let measure = tick >= WARMUP_TICKS;
        if tick == WARMUP_TICKS {
            // Train the shared dictionary on warm-up traffic only; score on the rest.
            let dict = zstd::dict::from_samples(&train, 64 * 1024).expect("train dictionary");
            dict_len = dict.len();
            let leaked: &'static [u8] = Box::leak(dict.into_boxed_slice());
            zc = Some(zstd::bulk::Compressor::with_dictionary(3, leaked).expect("zstd"));
            // What a new binary subscriber to the whole feed receives first.
            let t = &conns[1].1.t;
            let mut j = vec![MAGIC_VERSION];
            t.write_directory(&mut j).unwrap();
            for s in t.streams() {
                t.snapshot_of(s, 0, &mut j).unwrap();
            }
            join_bytes = j.len() as u64;
        }
        let mut single_batch: HashMap<u32, u64> = HashMap::new();
        let mut single_json: HashMap<u32, Vec<u64>> = HashMap::new();
        for e in &events {
            let b = e.json.as_bytes();
            if !measure {
                if train.len() < 20_000 {
                    train.push(b.to_vec());
                }
            } else {
                json_t.add(e.kind, b.len() as u64);
                json_t.frame(b.len() as u64);
                let n = pmd_full.compress(b).len() as u64;
                pmd_full_t.add(e.kind, n);
                pmd_full_t.frame(n);
                let n = pmd_noctx.compress(b).len() as u64;
                pmd_noctx_t.add(e.kind, n);
                pmd_noctx_t.frame(n);
                let n = zc.as_mut().unwrap().compress(b).expect("zstd").len() as u64;
                zstd_t.add(e.kind, n);
                zstd_t.frame(n);
                let p = pmd_single.entry(e.instrument).or_insert_with(|| Pmd::new(true));
                let n = p.compress(b).len() as u64;
                pmd_single_t.add(e.kind, n);
                pmd_single_t.frame(n);
                single_json.entry(e.instrument).or_default().push(b.len() as u64);
                single_json_t.add(e.kind, b.len() as u64);
            }
            if e.kind == EventKind::Book {
                let lv = levels_of(&e.json);
                if measure {
                    st.books += 1;
                    st.book_json_bytes += b.len() as u64;
                    let n_levels = lv.matches("],[").count() as u64 + 2 * (lv.matches("[[").count() as u64);
                    st.levels += n_levels.min(1000);
                    if last_book.get(&e.instrument).is_some_and(|p| p == lv) {
                        st.books_unchanged += 1;
                    }
                    // SBE-style fixed layout, following Binance's
                    // DepthSnapshotStreamEvent: 8-byte header, 8 event time,
                    // 8 update id, 2 exponents, two 4-byte group headers,
                    // 16 bytes per level, 1 + name length.
                    let n_lv = lv.matches("\",\"").count() as u64;
                    let name_len = e.json.split("\"instrument_name\":\"").nth(1).and_then(|s| s.find('"')).unwrap_or(0) as u64;
                    sbe_book_bytes += 8 + 8 + 8 + 2 + 4 + 4 + 16 * n_lv + 1 + name_len;
                }
                last_book.insert(e.instrument, lv.to_owned());
            }
            for (_, c) in conns.iter_mut() {
                if !c.push(e, tick, measure, &mut scratch) {
                    st.raw_fallbacks += 1;
                }
            }
            let c = single_bin.entry(e.instrument).or_insert_with(|| BinConn::new(delta, false, 1));
            let before = c.tally.payload;
            c.push(e, tick, measure, &mut scratch);
            *single_batch.entry(e.instrument).or_default() += c.tally.payload - before;
        }
        for (_, c) in conns.iter_mut() {
            c.end_tick(measure);
        }
        if measure {
            for (i, bytes) in single_batch {
                if bytes > 0 {
                    single_bin_t.payload += bytes + 1;
                    single_bin_t.frame(bytes + 1);
                }
                let _ = i;
            }
            for (_, frames) in single_json {
                for f in frames {
                    single_json_t.frame(f);
                }
            }
            for c in single_bin.values_mut() {
                c.batch = 0;
            }
        }
    }
    for c in single_bin.values() {
        for (k, v) in c.tally.by_family.iter().enumerate() {
            single_bin_t.by_family[k] += v;
        }
        single_bin_t.messages += c.tally.messages;
    }

    let mut variants = serde_json::Map::new();
    variants.insert("json".into(), json_t.json(secs));
    variants.insert("json_pmd_context_takeover".into(), pmd_full_t.json(secs));
    variants.insert("json_pmd_no_context_takeover".into(), pmd_noctx_t.json(secs));
    variants.insert("json_zstd_dictionary".into(), zstd_t.json(secs));
    for (name, c) in &conns {
        variants.insert((*name).into(), c.tally.json(secs));
    }
    let mut single = serde_json::Map::new();
    single.insert("json".into(), single_json_t.json(secs));
    single.insert("json_pmd_context_takeover".into(), pmd_single_t.json(secs));
    single.insert("binary_deltas".into(), single_bin_t.json(secs));

    json!({
        "regime": regime.name,
        "simulated_seconds": secs,
        "instruments": feed.instrument_names().len(),
        "feed_config": {"depth": cfg.depth, "ticker_ms": cfg.ticker_ms, "greek_digits": cfg.greek_digits},
        "whole_feed_subscriber": variants,
        "one_instrument_subscribers_summed": single,
        "book_stats": {
            "messages": st.books,
            "unchanged_fraction": st.books_unchanged as f64 / st.books.max(1) as f64,
            "mean_json_bytes": st.book_json_bytes as f64 / st.books.max(1) as f64,
            "sbe_fixed_layout_bytes": sbe_book_bytes,
            "json_bytes": st.book_json_bytes,
        },
        "raw_fallbacks": st.raw_fallbacks,
        "zstd_dictionary_bytes": dict_len,
        "join_bytes_whole_feed_binary": join_bytes,
    })
}

fn orders() -> Value {
    let mut rng = synth::order_rng(11);
    let (mut json_b, mut bin_b, mut sig_hex, n) = (0u64, 0u64, 0u64, 1000u64);
    for i in 0..n {
        let (name, o, _) = synth::sample_order(&mut rng, i);
        let j = mdbin::order::render_order_json(&name, &o);
        let h = mdbin::msg::Header { ty: mdbin::msg::TYPE_NEW_ORDER, instrument: 1 + (i % 100) as u32, ..Default::default() };
        let b = mdbin::msg::encode_frame(&h, &mdbin::msg::Body::NewOrder(Box::new(o))).unwrap();
        json_b += j.len() as u64;
        bin_b += b.len() as u64;
        sig_hex += 132 + 2;
    }
    json!({
        "orders": n,
        "mean_json_bytes": json_b as f64 / n as f64,
        "mean_binary_bytes": bin_b as f64 / n as f64,
        "signature_json_chars": sig_hex / n,
        "signature_binary_bytes": 65,
        "note": "Orders flow client to exchange. AWS lists inbound data transfer at $0.00/GB, so order size does not change the transfer bill.",
    })
}

fn main() {
    let out_dir = std::env::args().nth(1).unwrap_or_else(|| "results".into());
    let base = FeedConfig::default();
    let mut regimes = serde_json::Map::new();
    for r in REGIMES {
        eprintln!("sizes: regime {}", r.name);
        regimes.insert(r.name.into(), run(r, base));
    }
    let base_regime = REGIMES[1];
    let mut sens = serde_json::Map::new();
    eprintln!("sizes: sensitivity runs");
    sens.insert("greek_digits_6".into(), run(base_regime, FeedConfig { greek_digits: 6, ..base }));
    sens.insert("ticker_100ms".into(), run(base_regime, FeedConfig { ticker_ms: 100, ..base }));
    sens.insert("depth_10".into(), run(base_regime, FeedConfig { depth: 10, ..base }));
    let doc = json!({
        "label": "SYNTHETIC: generated by crates/synth, not captured from any exchange",
        "generator": "crates/synth (regimes pre-registered, see docs/synthetic-data.md)",
        "warmup_ticks_excluded": WARMUP_TICKS,
        "measured_ticks": MEASURE_TICKS,
        "wire_model": {
            "note": "MODELLED, not measured: each WebSocket message as its own TCP segment(s), TLS 1.3 AES-GCM, IPv4, TCP timestamps, 1500 MTU",
            "tcp_ip_per_segment": wire::TCP_IP_PER_SEGMENT, "mss": wire::MSS, "tls_per_record": wire::TLS_PER_RECORD,
        },
        "framing": "JSON: one WebSocket message per notification (as today). Binary: one WebSocket message per 100 ms tick per connection (batched).",
        "regimes": regimes,
        "sensitivity_base_regime": sens,
        "orders": orders(),
    });
    std::fs::create_dir_all(&out_dir).unwrap();
    let path = format!("{out_dir}/sizes.json");
    std::fs::write(&path, serde_json::to_string_pretty(&doc).unwrap() + "\n").unwrap();
    eprintln!("wrote {path}");
}

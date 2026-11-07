//! JSON -> binary -> JSON must give back the same bytes, for every message,
//! whether or not deltas are on.

use mdbin::dec::Dec;
use mdbin::json::render_book;
use mdbin::msg::*;
use mdbin::replica::{Applied, Replica};
use mdbin::transcode::{Config, Outcome, Transcoder};
use proptest::prelude::*;

fn level_text(m: u64, scale: u8, trim: bool) -> String {
    let d = Dec { m: m as i128, s: scale };
    if trim { d.trimmed().to_text() } else { d.to_text() }
}

#[derive(Debug, Clone)]
struct Tick {
    bids: Vec<(u64, u64)>,
    asks: Vec<(u64, u64)>,
}

fn book_json(name: &str, publish_id: u64, ts: i64, t: &Tick, scale: u8, trim: bool) -> String {
    let side = |v: &[(u64, u64)]| {
        v.iter()
            .map(|(p, q)| format!(r#"["{}","{}"]"#, level_text(*p, scale, trim), level_text(*q, 3, trim)))
            .collect::<Vec<_>>()
            .join(",")
    };
    format!(
        r#"{{"method":"subscription","params":{{"channel":"orderbook.{name}.1.20","data":{{"asks":[{}],"bids":[{}],"instrument_name":"{name}","publish_id":{publish_id},"timestamp":{ts}}}}}}}"#,
        side(&t.asks),
        side(&t.bids)
    )
}

fn sorted_tick() -> impl Strategy<Value = Tick> {
    let levels = || proptest::collection::btree_map(1u64..2_000_000, 0u64..5_000_000, 0..20);
    (levels(), levels()).prop_map(|(b, a)| Tick {
        bids: b.into_iter().rev().collect(),
        asks: a.into_iter().collect(),
    })
}

fn run_exact(config: Config, msgs: &[String]) {
    let mut t = Transcoder::new(config);
    let mut r = Replica::new();
    for json in msgs {
        let mut frame = vec![MAGIC_VERSION];
        let outcome = t.push(json, &mut frame).unwrap();
        assert!(!matches!(outcome, Outcome::Raw), "unexpected raw fallback: {json}");
        let decoded = decode_frame(&frame).unwrap();
        let mut rebuilt = None;
        for m in &decoded {
            if let Applied::Updated(Some(s)) = r.apply(m, true).unwrap() {
                rebuilt = Some(s);
            }
        }
        assert_eq!(rebuilt.as_deref(), Some(json.as_str()));
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(300))]

    #[test]
    fn book_stream_is_byte_exact(
        ticks in proptest::collection::vec(sorted_tick(), 1..12),
        scale in 0u8..=12,
        trim: bool,
        deltas: bool,
        step in 1u64..1000,
    ) {
        let mut id = 1_000_000u64;
        let mut ts = 1_790_000_000_000i64;
        let msgs: Vec<String> = ticks.iter().map(|t| {
            id += step;
            ts += 100;
            book_json("ETH-PERP", id, ts, t, scale, trim)
        }).collect();
        run_exact(Config { deltas, suppress_unchanged: false }, &msgs);
    }

    #[test]
    fn mixed_text_forms_still_round_trip(
        prices in proptest::collection::btree_set(1u64..100_000, 1..15),
        pad in proptest::collection::vec(0usize..4, 15),
        deltas: bool,
    ) {
        // Some values carry trailing zeros and some do not: the per-value path.
        let side: Vec<String> = prices.iter().enumerate().map(|(i, p)| {
            let zeros = "0".repeat(pad[i]);
            let text = if pad[i] == 0 { format!("{p}") } else { format!("{p}.{zeros}") };
            format!(r#"["{text}","1.5"]"#)
        }).collect();
        let json = format!(
            r#"{{"method":"subscription","params":{{"channel":"orderbook.BTC-PERP.1.20","data":{{"asks":[{}],"bids":[],"instrument_name":"BTC-PERP","publish_id":7,"timestamp":1}}}}}}"#,
            side.join(",")
        );
        run_exact(Config { deltas, suppress_unchanged: false }, &[json]);
    }
}

fn ticker_json(name: &str, ts: i64, mark: &str, funding: Option<&str>, pricing: bool, n: u64) -> String {
    let f = funding.map_or("null".to_owned(), |s| format!("\"{s}\""));
    let p = if pricing {
        r#"{"ai":"0.71","bi":"0.69","d":"0.512345678901","df":"0.998","f":"3100.25","g":"0.0012","i":"0.7","m":"212.5","r":"1.2","t":"-3.4","v":"4.1"}"#
    } else {
        "null"
    };
    format!(
        r#"{{"method":"subscription","params":{{"channel":"ticker_slim.{name}.100","data":{{"instrument_ticker":{{"A":"1.5","B":"2","I":"3100.1","M":"{mark}","a":"3100.5","b":"3099.5","f":{f},"maxp":"3200","minp":"3000","option_pricing":{p},"stats":{{"c":"100","h":"3200","l":"3000","n":{n},"oi":"5000","p":"-0.5","pr":"0","v":"310000"}},"t":{ts}}},"timestamp":{ts}}}}}}}"#
    )
}

#[test]
fn ticker_stream_is_byte_exact_including_null_transitions() {
    let msgs = vec![
        ticker_json("ETH-PERP", 1000, "3100.2", Some("0.0001"), false, 5),
        ticker_json("ETH-PERP", 1100, "3100.3", Some("0.0001"), false, 5),
        ticker_json("ETH-PERP", 1200, "3100.3", None, false, 6),
        ticker_json("ETH-PERP", 1300, "3100.3", Some("0.0002"), false, 6),
        ticker_json("ETH-PERP", 1400, "3100.4", Some("0.0002"), true, 7),
        ticker_json("ETH-PERP", 1500, "3100.4", Some("0.0002"), true, 7),
        ticker_json("ETH-PERP", 1600, "3100.5", Some("0.0002"), false, 7),
    ];
    run_exact(Config { deltas: true, suppress_unchanged: false }, &msgs);
    run_exact(Config { deltas: false, suppress_unchanged: false }, &msgs);
}

fn trade(wallet: &str, quote: &str) -> String {
    format!(
        r#"{{"direction":"buy","expected_rebate":"0","extra_fee":"0","index_price":"3100.1","instrument_name":"ETH-PERP","liquidity_role":"taker","mark_price":"3100.2","quote_id":{quote},"realized_pnl":"-1.25","realized_pnl_excl_fees":"-1","rfq_id":null,"subaccount_id":9,"timestamp":1700,"trade_amount":"0.5","trade_fee":"0.25","trade_id":"b2d1c3a4-1111-4222-8333-444455556666","trade_price":"3100.5","wallet":"{wallet}"}}"#
    )
}

#[test]
fn trades_are_byte_exact_with_both_address_forms() {
    let lower = "0x5aaeb6053f3e94c9b9a09f33669435e7ef1beaed";
    let checksummed = "0x5aAeb6053F3E94C9b9A09f33669435E7Ef1BeAed";
    let batch = format!("{},{}", trade(lower, "null"), trade(checksummed, r#""7f1c0a3e-9d7b-4c58-a111-222233334444""#));
    let json = format!(r#"{{"method":"subscription","params":{{"channel":"trades.ETH-PERP","data":[{batch}]}}}}"#);
    run_exact(Config::default(), &[json]);
}

#[test]
fn anything_unrepresentable_is_forwarded_unchanged() {
    let cases = [
        // Unknown field.
        r#"{"method":"subscription","params":{"channel":"orderbook.ETH-PERP.1.20","data":{"asks":[],"bids":[],"instrument_name":"ETH-PERP","publish_id":1,"timestamp":1,"new_field":true}}}"#,
        // A number where the docs promise a string.
        r#"{"method":"subscription","params":{"channel":"orderbook.ETH-PERP.1.20","data":{"asks":[[3000,"1"]],"bids":[],"instrument_name":"ETH-PERP","publish_id":1,"timestamp":1}}}"#,
        // Non-canonical decimal.
        r#"{"method":"subscription","params":{"channel":"orderbook.ETH-PERP.1.20","data":{"asks":[["03000","1"]],"bids":[],"instrument_name":"ETH-PERP","publish_id":1,"timestamp":1}}}"#,
        // Channel this codec does not model.
        r#"{"method":"subscription","params":{"channel":"spot_feed.ETH","data":{"feeds":{},"timestamp":1}}}"#,
        // Instrument mismatch between channel and payload.
        r#"{"method":"subscription","params":{"channel":"orderbook.ETH-PERP.1.20","data":{"asks":[],"bids":[],"instrument_name":"BTC-PERP","publish_id":1,"timestamp":1}}}"#,
        // Not JSON at all.
        "ping",
    ];
    for json in cases {
        let mut t = Transcoder::new(Config { deltas: true, suppress_unchanged: false });
        let mut frame = vec![MAGIC_VERSION];
        assert_eq!(t.push(json, &mut frame).unwrap(), Outcome::Raw, "{json}");
        let msgs = decode_frame(&frame).unwrap();
        let mut r = Replica::new();
        let got = r.apply(&msgs[0], true).unwrap();
        assert_eq!(got, Applied::Updated(Some(json.to_owned())));
    }
}

#[test]
fn unsorted_book_never_uses_a_delta() {
    let bad = Tick { bids: vec![(100, 1), (200, 1)], asks: vec![] };
    let good = Tick { bids: vec![(200, 1), (100, 1)], asks: vec![] };
    let msgs = vec![
        book_json("ETH-PERP", 1, 1, &bad, 2, false),
        book_json("ETH-PERP", 2, 2, &good, 2, false),
        book_json("ETH-PERP", 3, 3, &bad, 2, false),
    ];
    run_exact(Config { deltas: true, suppress_unchanged: false }, &msgs);
}

#[test]
fn suppressed_message_writes_nothing_and_later_delta_is_correct() {
    let t0 = Tick { bids: vec![(200, 1)], asks: vec![(300, 1)] };
    let t1 = Tick { bids: vec![(200, 2)], asks: vec![(300, 1)] };
    let cfg = Config { deltas: true, suppress_unchanged: true };
    let mut enc = Transcoder::new(cfg);
    let mut r = Replica::new();
    let mut f = vec![MAGIC_VERSION];
    enc.push(&book_json("ETH-PERP", 1, 100, &t0, 2, false), &mut f).unwrap();
    for m in decode_frame(&f).unwrap() {
        r.apply(&m, false).unwrap();
    }
    let mut f = vec![MAGIC_VERSION];
    let o = enc.push(&book_json("ETH-PERP", 2, 200, &t0, 2, false), &mut f).unwrap();
    assert!(matches!(o, Outcome::Suppressed { .. }));
    assert_eq!(f.len(), 1);
    let want = book_json("ETH-PERP", 3, 300, &t1, 2, false);
    let mut f = vec![MAGIC_VERSION];
    enc.push(&want, &mut f).unwrap();
    let m = decode_frame(&f).unwrap();
    assert_eq!(r.apply(&m[0], true).unwrap(), Applied::Updated(Some(want)));
}

#[test]
fn encoded_message_does_not_depend_on_its_frame() {
    // Encode-once fan-out relies on this: the same bytes are valid at any
    // position in any client's frame.
    let t0 = Tick { bids: vec![(200, 1)], asks: vec![(300, 1)] };
    let mut enc = Transcoder::new(Config { deltas: true, suppress_unchanged: false });
    let (mut a, mut b) = (Vec::new(), Vec::new());
    enc.push(&book_json("ETH-PERP", 1, 100, &t0, 2, false), &mut a).unwrap();
    enc.push(&book_json("BTC-PERP", 1, 100, &t0, 2, false), &mut b).unwrap();
    let mut ab = vec![MAGIC_VERSION];
    ab.extend_from_slice(&a);
    ab.extend_from_slice(&b);
    let mut ba = vec![MAGIC_VERSION];
    ba.extend_from_slice(&b);
    ba.extend_from_slice(&a);
    let n = |f: &[u8]| decode_frame(f).unwrap().len();
    assert_eq!(n(&ab), 4);
    assert_eq!(n(&ba), 4);
}

#[test]
fn renderer_matches_for_an_empty_book() {
    let b = BookSnapshot { chan: 2, publish_id: 9, ..Default::default() };
    let mut s = String::new();
    render_book(&mut s, "ETH-PERP", 5, &b).unwrap();
    assert!(s.contains(r#""asks":[],"bids":[]"#));
    assert!(s.contains("orderbook.ETH-PERP.1.20"));
}

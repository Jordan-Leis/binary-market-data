//! The decoder faces untrusted input. It must return errors, never panic, and
//! never allocate more than the input justifies.

use mdbin::dec::Dec;
use mdbin::msg::*;
use mdbin::replica::Replica;
use mdbin::transcode::{Config, Transcoder};
use mdbin::wire::put_varint;
use proptest::prelude::*;

fn sample_frames() -> Vec<Vec<u8>> {
    let mut t = Transcoder::new(Config { deltas: true, suppress_unchanged: false });
    let mut out = Vec::new();
    for i in 0..6u32 {
        let json = format!(
            r#"{{"method":"subscription","params":{{"channel":"orderbook.ETH-PERP.1.20","data":{{"asks":[["3001.5","{}"],["3002","1.25"]],"bids":[["3000.25","1"],["2999","2"]],"instrument_name":"ETH-PERP","publish_id":{i},"timestamp":{}}}}}}}"#,
            i + 1,
            1000 + i * 100
        );
        let mut f = vec![MAGIC_VERSION];
        t.push(&json, &mut f).unwrap();
        out.push(f);
    }
    let o = NewOrder {
        rpc_id: 1,
        subaccount_id: 9,
        is_buy: true,
        limit_price: Dec::parse("1.5").unwrap(),
        amount: Dec::parse("2").unwrap(),
        max_fee: Dec::parse("0.1").unwrap(),
        nonce: 5,
        signer: Addr::Lower([1; 20]),
        signature: [2; 65],
        signature_expiry_sec: 7,
        order_type: None,
        time_in_force: None,
        reduce_only: None,
        mmp: None,
        label: Some("x".into()),
    };
    let h = Header { ty: TYPE_NEW_ORDER, ..Default::default() };
    out.push(encode_frame(&h, &Body::NewOrder(Box::new(o))).unwrap());
    out
}

fn feed(r: &mut Replica, f: &[u8]) {
    if let Ok(msgs) = decode_frame(f) {
        for m in &msgs {
            let _ = r.apply(m, true);
        }
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(4000))]

    #[test]
    fn random_bytes_never_panic(bytes in proptest::collection::vec(any::<u8>(), 0..512)) {
        let mut r = Replica::new();
        let mut f = vec![MAGIC_VERSION];
        f.extend_from_slice(&bytes);
        feed(&mut r, &f);
        feed(&mut r, &bytes);
    }

    #[test]
    fn mutated_valid_frames_never_panic(
        which in 0usize..7,
        flips in proptest::collection::vec((any::<usize>(), any::<u8>()), 1..8),
        cut in any::<usize>(),
    ) {
        let frames = sample_frames();
        let mut r = Replica::new();
        feed(&mut r, &frames[0]);
        let mut f = frames[which].clone();
        for (pos, v) in flips {
            let n = f.len();
            f[pos % n] ^= v;
        }
        let keep = 1 + cut % f.len();
        f.truncate(keep);
        feed(&mut r, &f);
    }

    /// Anything the decoder accepts re-encodes to something that decodes the same.
    #[test]
    fn accepted_snapshots_re_encode(bytes in proptest::collection::vec(any::<u8>(), 0..256)) {
        let mut f = vec![MAGIC_VERSION];
        f.extend_from_slice(&bytes);
        if let Ok(msgs) = decode_frame(&f) {
            for m in msgs {
                if matches!(m.body, Body::Unknown) { continue; }
                if let Ok(re) = encode_frame(&m.header, &m.body) {
                    let again = decode_frame(&re).unwrap();
                    prop_assert_eq!(&again[0], &m);
                }
            }
        }
    }
}

#[test]
fn a_tiny_frame_cannot_claim_a_huge_book() {
    let mut body = vec![TYPE_BOOK_SNAPSHOT, 0];
    put_varint(&mut body, 1); // instrument
    put_varint(&mut body, 1); // seq
    put_varint(&mut body, 0); // ts
    body.push(0); // chan
    put_varint(&mut body, 1); // publish id
    body.extend_from_slice(&[0, 0]); // column modes
    put_varint(&mut body, MAX_LEVELS as u64); // bids
    put_varint(&mut body, MAX_LEVELS as u64); // asks
    let mut f = vec![MAGIC_VERSION];
    put_varint(&mut f, body.len() as u64);
    f.extend_from_slice(&body);
    assert_eq!(decode_frame(&f), Err(mdbin::Error::Truncated));
}

#[test]
fn a_length_past_the_frame_limit_is_refused() {
    let mut f = vec![MAGIC_VERSION];
    put_varint(&mut f, u64::MAX);
    assert_eq!(decode_frame(&f), Err(mdbin::Error::FrameTooLarge));
    assert_eq!(decode_frame(&vec![MAGIC_VERSION; MAX_FRAME + 1]), Err(mdbin::Error::FrameTooLarge));
}

#[test]
fn wrong_magic_and_version_are_refused() {
    assert_eq!(decode_frame(&[0x00]), Err(mdbin::Error::BadMagic));
    assert_eq!(decode_frame(&[0xB2]), Err(mdbin::Error::BadVersion));
    assert_eq!(decode_frame(&[]), Err(mdbin::Error::Truncated));
}

#[test]
fn unknown_types_are_skipped_using_len() {
    let mut f = vec![MAGIC_VERSION];
    // Type 99, flags, instrument, seq, ts, then opaque bytes.
    let inner = [99u8, 0, 1, 1, 0, 0xde, 0xad];
    put_varint(&mut f, inner.len() as u64);
    f.extend_from_slice(&inner);
    let h = Header { ty: TYPE_INSTRUMENT_DEF, instrument: 1, ..Default::default() };
    encode_message(&mut f, &h, &Body::InstrumentDef { name: "ETH-PERP".into() }).unwrap();
    let msgs = decode_frame(&f).unwrap();
    assert_eq!(msgs[0].body, Body::Unknown);
    assert!(matches!(msgs[1].body, Body::InstrumentDef { .. }));
}

#[test]
fn hostile_text_is_escaped_when_rendered() {
    let mut s = String::new();
    mdbin::json::push_json_str(&mut s, "a\"b\\c\n\u{1}");
    assert_eq!(s, r#""a\"b\\c\n\u0001""#.replace("\\n", "\\u000a"));
    assert!(serde_json::from_str::<String>(&s).is_ok());
}

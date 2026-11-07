//! Gap detection and snapshot recovery.

use mdbin::msg::*;
use mdbin::replica::{Applied, Replica};
use mdbin::transcode::{Config, Family, StreamId, Transcoder};

fn json(id: u64, qty: u32) -> String {
    format!(
        r#"{{"method":"subscription","params":{{"channel":"orderbook.ETH-PERP.1.20","data":{{"asks":[["3001","{qty}"]],"bids":[["3000","1"]],"instrument_name":"ETH-PERP","publish_id":{id},"timestamp":{}}}}}}}"#,
        1000 + id * 100
    )
}

fn encode_all(t: &mut Transcoder, range: std::ops::Range<u64>) -> Vec<Vec<Message>> {
    range
        .map(|i| {
            let mut f = vec![MAGIC_VERSION];
            t.push(&json(i, i as u32 + 1), &mut f).unwrap();
            decode_frame(&f).unwrap()
        })
        .collect()
}

#[test]
fn gap_is_detected_then_recovered_from_a_snapshot() {
    let mut t = Transcoder::new(Config { deltas: true, suppress_unchanged: false });
    let frames = encode_all(&mut t, 1..8);
    let mut r = Replica::new();
    for m in frames[0].iter().chain(&frames[1]) {
        r.apply(m, false).unwrap();
    }

    // Frames 2 and 3 are lost. Frame 4 reveals the gap.
    let Applied::Gap { stream, expected, got } = r.apply(&frames[4][0], false).unwrap() else {
        panic!("gap not detected");
    };
    assert_eq!((expected, got), (3, 5));
    assert_eq!(stream.family, Family::Book);
    assert_eq!(r.gaps, 1);

    // Deltas are refused until a snapshot arrives, and the stale book is hidden.
    assert_eq!(r.apply(&frames[5][0], false).unwrap(), Applied::Ignored);
    assert!(r.book(1, frames[0][1].header.instrument as u8).is_none());

    // The server answers a resync with current state, without advancing seq.
    let mut snap = vec![MAGIC_VERSION];
    assert!(t.snapshot_of(stream, FLAG_RESYNC, &mut snap).unwrap());
    let m = decode_frame(&snap).unwrap();
    assert_eq!(m[0].header.flags & FLAG_RESYNC, FLAG_RESYNC);
    assert_eq!(m[0].header.seq, 7);
    assert!(matches!(r.apply(&m[0], false).unwrap(), Applied::Updated(_)));

    // Anything at or below the snapshot sequence is dropped; the next one applies.
    assert_eq!(r.apply(&frames[6][0], false).unwrap(), Applied::Ignored);
    let next = encode_all(&mut t, 8..9);
    let want = json(8, 9);
    assert_eq!(r.apply(&next[0][0], true).unwrap(), Applied::Updated(Some(want)));
}

#[test]
fn duplicates_are_ignored() {
    let mut t = Transcoder::new(Config { deltas: true, suppress_unchanged: false });
    let frames = encode_all(&mut t, 1..4);
    let mut r = Replica::new();
    for f in &frames[..2] {
        for m in f {
            r.apply(m, false).unwrap();
        }
    }
    assert_eq!(r.apply(&frames[1][0], false).unwrap(), Applied::Ignored);
    assert!(matches!(r.apply(&frames[2][0], false).unwrap(), Applied::Updated(_)));
    assert_eq!(r.gaps, 0);
}

#[test]
fn delta_for_an_unknown_stream_waits_for_a_snapshot() {
    let mut t = Transcoder::new(Config { deltas: true, suppress_unchanged: false });
    let frames = encode_all(&mut t, 1..4);
    let mut r = Replica::new();
    r.apply(&frames[0][0], false).unwrap(); // the definition only
    assert_eq!(r.apply(&frames[1][0], false).unwrap(), Applied::Ignored);
}

#[test]
fn a_delta_that_contradicts_state_marks_the_book_stale() {
    let mut t = Transcoder::new(Config { deltas: true, suppress_unchanged: false });
    let frames = encode_all(&mut t, 1..2);
    let mut r = Replica::new();
    for m in &frames[0] {
        r.apply(m, false).unwrap();
    }
    let chan = mdbin::json::book_chan(1, 20).unwrap();
    let op = BookOp { side: Side::Ask, price: mdbin::dec::Dec::parse("9999").unwrap(), qty: None };
    let h = Header { ty: TYPE_BOOK_DELTA, instrument: 1, seq: 2, ts: 100, flags: 0 };
    let m = Message { header: h, body: Body::BookDelta(BookDelta { chan, publish_id_delta: 1, ops: vec![op] }) };
    assert!(r.apply(&m, false).is_err());
    assert!(r.book(1, chan).is_none(), "a contradicted book must not stay readable");
}

#[test]
fn late_joiner_converges_from_a_snapshot() {
    let mut t = Transcoder::new(Config { deltas: true, suppress_unchanged: false });
    encode_all(&mut t, 1..20);
    let chan = mdbin::json::book_chan(1, 20).unwrap();
    let stream = StreamId { instrument: 1, family: Family::Book, chan };
    let mut f = vec![MAGIC_VERSION];
    t.write_directory(&mut f).unwrap();
    t.snapshot_of(stream, 0, &mut f).unwrap();
    let mut r = Replica::new();
    for m in decode_frame(&f).unwrap() {
        r.apply(&m, false).unwrap();
    }
    let next = encode_all(&mut t, 20..21);
    let want = json(20, 21);
    assert_eq!(r.apply(&next[0][0], true).unwrap(), Applied::Updated(Some(want)));
}

//! Seeded mutation testing of the decoder and client state machine. This is
//! not coverage-guided fuzzing: it mutates valid frames at random. Each mutated
//! frame is applied to a client that already holds state from a valid
//! snapshot frame, so deltas meet established books. Any panic is caught,
//! counted and reported.

use mdbin::msg::{decode_frame, MAGIC_VERSION};
use mdbin::replica::Replica;
use mdbin::transcode::{Config, Transcoder};
use rand::{Rng, SeedableRng};
use std::panic;

fn main() {
    let mut args = std::env::args().skip(1);
    let out_dir = args.next().unwrap_or_else(|| "results".into());
    let iters: u64 = args.next().and_then(|s| s.parse().ok()).unwrap_or(2_000_000);
    let mut feed = synth::Feed::new(synth::REGIMES[2], synth::FeedConfig::default());
    let mut t = Transcoder::new(Config { deltas: true, suppress_unchanged: false });
    let mut ev = Vec::new();
    let mut corpus = Vec::new();
    for _ in 0..20 {
        ev.clear();
        feed.tick(&mut ev);
        let mut frame = vec![MAGIC_VERSION];
        for e in &ev {
            t.push(&e.json, &mut frame).unwrap();
        }
        corpus.push(frame);
    }
    let mut rng = rand_chacha::ChaCha8Rng::seed_from_u64(1);
    panic::set_hook(Box::new(|_| {}));
    let (mut panics, mut accepted, mut rejected) = (0u64, 0u64, 0u64);
    for i in 0..iters {
        let mut f = corpus[rng.gen_range(0..corpus.len())].clone();
        match i % 4 {
            0 => {
                for _ in 0..rng.gen_range(1..8) {
                    let p = rng.gen_range(0..f.len());
                    f[p] = rng.gen();
                }
            }
            1 => f.truncate(rng.gen_range(1..=f.len())),
            2 => {
                let p = rng.gen_range(1..f.len());
                let n = rng.gen_range(1..32);
                let junk: Vec<u8> = (0..n).map(|_| rng.gen()).collect();
                f.splice(p..p, junk);
            }
            _ => {
                let n = rng.gen_range(1..256);
                f = std::iter::once(MAGIC_VERSION).chain((0..n).map(|_| rng.gen())).collect();
            }
        }
        let r = panic::catch_unwind(|| {
            let mut rep = Replica::new();
            for m in decode_frame(&corpus[0]).unwrap_or_default() {
                let _ = rep.apply(&m, false);
            }
            match decode_frame(&f) {
                Ok(msgs) => {
                    for m in &msgs {
                        let _ = rep.apply(m, true);
                    }
                    true
                }
                Err(_) => false,
            }
        });
        match r {
            Ok(true) => accepted += 1,
            Ok(false) => rejected += 1,
            Err(_) => panics += 1,
        }
    }
    let doc = serde_json::json!({
        "iterations": iters, "panics": panics, "frames_accepted": accepted, "frames_rejected": rejected,
        "method": "seeded mutation testing (not coverage-guided): byte flips, truncation, junk insertion, random frames, applied after a valid snapshot frame",
    });
    std::fs::create_dir_all(&out_dir).unwrap();
    std::fs::write(format!("{out_dir}/fuzz.json"), serde_json::to_string_pretty(&doc).unwrap() + "\n").unwrap();
    eprintln!("fuzz: {iters} iterations, {panics} panics");
    if panics > 0 {
        std::process::exit(1);
    }
}

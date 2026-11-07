//! Writes a small SYNTHETIC sample of the feed as JSONL.
use std::io::Write;
use synth::{Feed, FeedConfig, REGIMES};

fn main() {
    let path = std::env::args().nth(1).unwrap_or_else(|| "data/synthetic/sample.jsonl".into());
    let mut f = Feed::new(REGIMES[1], FeedConfig::default());
    let mut ev = Vec::new();
    let mut out = std::io::BufWriter::new(std::fs::File::create(&path).expect("create"));
    for tick in 0..60 {
        ev.clear();
        f.tick(&mut ev);
        if tick >= 50 {
            for e in &ev {
                writeln!(out, "{}", e.json).unwrap();
            }
        }
    }
    eprintln!("wrote {path} (SYNTHETIC: 1 simulated second, base regime)");
}

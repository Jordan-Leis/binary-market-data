//! Criterion micro-benchmarks for the codec on SYNTHETIC messages.
//! `make bench-criterion` runs these; `results/timing.json` comes from the
//! `timing` binary, which also records CPU time and memory.

use criterion::{criterion_group, criterion_main, Criterion, Throughput};
use mdbin::msg::{decode_frame, MAGIC_VERSION};
use mdbin::replica::Replica;
use mdbin::transcode::{Config, Transcoder};
use std::hint::black_box;
use synth::{Feed, FeedConfig, REGIMES};

fn sample() -> Vec<String> {
    let mut f = Feed::new(REGIMES[1], FeedConfig::default());
    let mut ev = Vec::new();
    for _ in 0..30 {
        f.tick(&mut ev);
    }
    ev.into_iter().map(|e| e.json).collect()
}

fn bench(c: &mut Criterion) {
    let msgs = sample();
    let mut g = c.benchmark_group("per_message");
    g.throughput(Throughput::Elements(msgs.len() as u64));
    g.bench_function("serde_json_parse_value", |b| {
        b.iter(|| {
            for m in &msgs {
                black_box(serde_json::from_str::<serde_json::Value>(m).unwrap());
            }
        })
    });
    g.bench_function("transcode_json_to_binary_deltas", |b| {
        b.iter(|| {
            let mut t = Transcoder::new(Config { deltas: true, suppress_unchanged: false });
            let mut out = Vec::with_capacity(1 << 20);
            for m in &msgs {
                t.push(m, &mut out).unwrap();
            }
            black_box(out.len())
        })
    });
    let mut t = Transcoder::new(Config { deltas: true, suppress_unchanged: false });
    let frames: Vec<Vec<u8>> = msgs
        .iter()
        .map(|m| {
            let mut f = vec![MAGIC_VERSION];
            t.push(m, &mut f).unwrap();
            f
        })
        .collect();
    g.bench_function("decode_and_apply_binary", |b| {
        b.iter(|| {
            let mut r = Replica::new();
            for f in &frames {
                for m in decode_frame(f).unwrap() {
                    black_box(r.apply(&m, false).unwrap());
                }
            }
        })
    });
    g.finish();
}

criterion_group!(benches, bench);
criterion_main!(benches);

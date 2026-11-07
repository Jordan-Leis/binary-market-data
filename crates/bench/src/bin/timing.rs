//! CPU time per message and per connection, on this machine only.
//! Inputs are SYNTHETIC. These are serialization and memory-copy costs, not
//! network latency, and not numbers for any production system.

use bench::{now_cpu_ns, Pmd};
use mdbin::msg::{decode_frame, MAGIC_VERSION};
use mdbin::replica::{Applied, Replica};
use mdbin::transcode::{Config, Transcoder};
use serde_json::{json, Value};
use std::alloc::{GlobalAlloc, Layout, System};
use std::hint::black_box;
use std::sync::atomic::{AtomicIsize, Ordering::Relaxed};
use std::time::Instant;
use synth::{Feed, FeedConfig, REGIMES};

/// Counts live heap bytes so per-connection state can be measured, not guessed.
struct Tracking;
static LIVE: AtomicIsize = AtomicIsize::new(0);
unsafe impl GlobalAlloc for Tracking {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        LIVE.fetch_add(l.size() as isize, Relaxed);
        unsafe { System.alloc(l) }
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        LIVE.fetch_sub(l.size() as isize, Relaxed);
        unsafe { System.dealloc(p, l) }
    }
}
#[global_allocator]
static A: Tracking = Tracking;

const RUNS: usize = 7;

/// Median over runs of (CPU ns, wall ns) per item.
fn measure<F: FnMut() -> u64>(mut f: F) -> Value {
    let mut cpu = Vec::new();
    let mut wall = Vec::new();
    let mut items = 0;
    f(); // warm up
    for _ in 0..RUNS {
        let (c0, w0) = (now_cpu_ns(), Instant::now());
        items = f();
        cpu.push((now_cpu_ns() - c0) as f64 / items as f64);
        wall.push(w0.elapsed().as_nanos() as f64 / items as f64);
    }
    cpu.sort_by(f64::total_cmp);
    wall.sort_by(f64::total_cmp);
    json!({"cpu_ns_per_item_median": cpu[RUNS / 2].round(), "wall_ns_per_item_median": wall[RUNS / 2].round(),
           "wall_ns_min": wall[0].round(), "wall_ns_max": wall[RUNS - 1].round(), "items_per_run": items, "runs": RUNS})
}

fn ticks(n: usize) -> Vec<Vec<String>> {
    let mut f = Feed::new(REGIMES[1], FeedConfig::default());
    let mut ev = Vec::new();
    (0..n)
        .map(|_| {
            ev.clear();
            f.tick(&mut ev);
            ev.iter().map(|e| e.json.clone()).collect()
        })
        .collect()
}

fn sh(cmd: &str) -> String {
    std::process::Command::new("sh").arg("-c").arg(cmd).output().map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned()).unwrap_or_default()
}

fn main() {
    let out_dir = std::env::args().nth(1).unwrap_or_else(|| "results".into());
    let load_before = sh("cut -d' ' -f1-3 /proc/loadavg");
    let feed = ticks(300);
    let msgs: Vec<&String> = feed.iter().flatten().collect();
    let total_json: usize = msgs.iter().map(|m| m.len()).sum();
    eprintln!("timing: {} messages, {} JSON bytes", msgs.len(), total_json);

    let mut per_msg = serde_json::Map::new();
    per_msg.insert("serde_json_parse_to_value".into(), measure(|| {
        for m in &msgs {
            black_box(serde_json::from_str::<Value>(m).unwrap());
        }
        msgs.len() as u64
    }));
    per_msg.insert("transcode_json_to_binary_deltas".into(), measure(|| {
        let mut t = Transcoder::new(Config { deltas: true, suppress_unchanged: false });
        let mut out = Vec::with_capacity(1 << 16);
        for m in &msgs {
            out.clear();
            t.push(m, &mut out).unwrap();
        }
        msgs.len() as u64
    }));
    let mut t = Transcoder::new(Config { deltas: true, suppress_unchanged: false });
    let frames: Vec<Vec<u8>> = msgs.iter().map(|m| {
        let mut f = vec![MAGIC_VERSION];
        t.push(m, &mut f).unwrap();
        f
    }).collect();
    per_msg.insert("decode_binary_and_apply".into(), measure(|| {
        let mut r = Replica::new();
        for f in &frames {
            for m in decode_frame(f).unwrap() {
                black_box(r.apply(&m, false).unwrap());
            }
        }
        frames.len() as u64
    }));
    per_msg.insert("decode_binary_apply_and_render_json".into(), measure(|| {
        let mut r = Replica::new();
        for f in &frames {
            for m in decode_frame(f).unwrap() {
                if let Applied::Updated(Some(s)) = r.apply(&m, true).unwrap() {
                    black_box(s);
                }
            }
        }
        frames.len() as u64
    }));
    per_msg.insert("pmd_compress_context_takeover".into(), measure(|| {
        let mut p = Pmd::new(true);
        for m in &msgs {
            black_box(p.compress(m.as_bytes()).len());
        }
        msgs.len() as u64
    }));
    per_msg.insert("pmd_compress_no_context_takeover".into(), measure(|| {
        let mut p = Pmd::new(false);
        for m in &msgs {
            black_box(p.compress(m.as_bytes()).len());
        }
        msgs.len() as u64
    }));
    let train: Vec<&[u8]> = msgs.iter().take(5000).map(|m| m.as_bytes()).collect();
    let dict = zstd::dict::from_samples(&train, 64 * 1024).unwrap();
    per_msg.insert("zstd_dictionary_compress".into(), measure(|| {
        let mut c = zstd::bulk::Compressor::with_dictionary(3, &dict).unwrap();
        for m in &msgs {
            black_box(c.compress(m.as_bytes()).unwrap().len());
        }
        msgs.len() as u64
    }));

    // Orders.
    let mut rng = synth::order_rng(5);
    let orders: Vec<_> = (0..300).map(|i| synth::sample_order(&mut rng, i)).collect();
    let order_json: Vec<String> = orders.iter().map(|(n, o, _)| mdbin::order::render_order_json(n, o)).collect();
    let mut per_order = serde_json::Map::new();
    per_order.insert("parse_json_order".into(), measure(|| {
        for j in &order_json {
            black_box(mdbin::order::parse_order_json(j).unwrap());
        }
        order_json.len() as u64
    }));
    let order_frames: Vec<Vec<u8>> = orders.iter().map(|(_, o, _)| {
        let h = mdbin::msg::Header { ty: mdbin::msg::TYPE_NEW_ORDER, instrument: 1, ..Default::default() };
        mdbin::msg::encode_frame(&h, &mdbin::msg::Body::NewOrder(Box::new(o.clone()))).unwrap()
    }).collect();
    per_order.insert("decode_binary_order".into(), measure(|| {
        for f in &order_frames {
            black_box(decode_frame(f).unwrap());
        }
        order_frames.len() as u64
    }));
    per_order.insert("verify_signature_secp256k1_recover".into(), measure(|| {
        for (_, o, c) in &orders {
            assert!(mdbin::order::verify_order(o, c).unwrap());
        }
        orders.len() as u64
    }));

    // Fan-out of one second of feed (10 ticks) to N connections.
    let n_conn = 100usize;
    let second: Vec<&Vec<String>> = feed.iter().skip(100).take(10).collect();
    let second_json: usize = second.iter().flat_map(|t| t.iter()).map(|m| m.len()).sum();
    let mut fan = serde_json::Map::new();
    fan.insert("json_as_is_copy_to_each_connection".into(), measure(|| {
        let mut bufs: Vec<Vec<u8>> = (0..n_conn).map(|_| Vec::with_capacity(1 << 17)).collect();
        for tick in &second {
            for b in bufs.iter_mut() {
                b.clear();
                for m in tick.iter() {
                    b.extend_from_slice(m.as_bytes());
                }
                black_box(b.len());
            }
        }
        n_conn as u64
    }));
    // Warm the shared stream state outside the timed region, as a running
    // gateway would have it.
    let mut warm = Transcoder::new(Config { deltas: true, suppress_unchanged: false });
    for tick in feed.iter().take(100) {
        let mut o = Vec::new();
        for m in tick {
            warm.push(m, &mut o).unwrap();
        }
    }
    fan.insert("binary_transcode_once_then_copy".into(), measure(|| {
        let mut bufs: Vec<Vec<u8>> = (0..n_conn).map(|_| Vec::with_capacity(1 << 16)).collect();
        let mut frame = Vec::with_capacity(1 << 16);
        for tick in &second {
            frame.clear();
            frame.push(MAGIC_VERSION);
            for m in tick.iter() {
                warm.push(m, &mut frame).unwrap();
            }
            for b in bufs.iter_mut() {
                b.clear();
                b.extend_from_slice(&frame);
                black_box(b.len());
            }
        }
        n_conn as u64
    }));
    fan.insert("json_pmd_context_takeover_per_connection".into(), measure(|| {
        let mut ps: Vec<Pmd> = (0..n_conn).map(|_| Pmd::new(true)).collect();
        for tick in &second {
            for p in ps.iter_mut() {
                for m in tick.iter() {
                    black_box(p.compress(m.as_bytes()).len());
                }
            }
        }
        n_conn as u64
    }));
    fan.insert("json_pmd_no_context_compress_once_then_copy".into(), measure(|| {
        let mut p = Pmd::new(false);
        let mut bufs: Vec<Vec<u8>> = (0..n_conn).map(|_| Vec::with_capacity(1 << 17)).collect();
        for tick in &second {
            let comp: Vec<Vec<u8>> = tick.iter().map(|m| p.compress(m.as_bytes()).to_vec()).collect();
            for b in bufs.iter_mut() {
                b.clear();
                for c in &comp {
                    b.extend_from_slice(c);
                }
                black_box(b.len());
            }
        }
        n_conn as u64
    }));

    // Live heap per compression context after it has seen traffic.
    let before = LIVE.load(Relaxed);
    let mut held: Vec<Pmd> = (0..50).map(|_| Pmd::new(true)).collect();
    for p in held.iter_mut() {
        for m in second[0].iter() {
            black_box(p.compress(m.as_bytes()).len());
        }
    }
    let pmd_bytes = (LIVE.load(Relaxed) - before) as f64 / 50.0;
    drop(held);
    let zstd_cctx = zstd::zstd_safe::CCtx::create();
    let zstd_bytes = zstd_cctx.sizeof();

    let doc = json!({
        "label": "MEASURED on this machine only, SYNTHETIC inputs. Serialization and memcpy costs; not network latency; not production numbers.",
        "machine": {
            "cpu": sh("grep -m1 'model name' /proc/cpuinfo | cut -d: -f2"),
            "cores": sh("nproc"),
            "kernel": sh("uname -r"),
            "rustc": sh("rustc --version"),
            "load_avg_1_5_15_before": load_before,
            "load_avg_1_5_15_after": sh("cut -d' ' -f1-3 /proc/loadavg"),
        },
        "messages_measured": msgs.len(),
        "mean_json_bytes": total_json as f64 / msgs.len() as f64,
        "per_message": per_msg,
        "per_order": per_order,
        "fan_out_one_second_of_feed": {
            "note": "Items are connections: cpu_ns_per_item is CPU per connection per second of feed, measured with 100 connections in one thread. Socket writes are excluded; each connection's bytes are copied into its own buffer.",
            "json_bytes_per_second_of_feed": second_json,
            "connections": n_conn,
            "results": fan,
        },
        "per_connection_state_bytes": {
            "pmd_context_takeover_live_heap": pmd_bytes.round(),
            "zstd_cctx_unused_sizeof": zstd_bytes,
            "binary_encode_once": "none beyond the outgoing frame buffer; stream state is shared",
        },
    });
    std::fs::create_dir_all(&out_dir).unwrap();
    let path = format!("{out_dir}/timing.json");
    std::fs::write(&path, serde_json::to_string_pretty(&doc).unwrap() + "\n").unwrap();
    eprintln!("wrote {path}");
}

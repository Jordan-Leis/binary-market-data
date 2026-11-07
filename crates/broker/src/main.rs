//! Can Redis or Dragonfly pub/sub carry the binary format as is, and is the
//! broker a bottleneck? Starts a private server for each, on loopback.
//!
//! Latency figures are publish-to-receive on one machine's loopback. They say
//! nothing about network latency. Payloads are SYNTHETIC.

use futures_util::StreamExt;
use rand::{Rng, SeedableRng};
use serde_json::{json, Value};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

struct Server {
    name: &'static str,
    child: Child,
    port: u16,
    _dir: std::path::PathBuf,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

fn start(name: &'static str) -> Option<Server> {
    let port = free_port();
    let dir = std::env::temp_dir().join(format!("mdbin-broker-{name}-{port}"));
    std::fs::create_dir_all(&dir).ok()?;
    let mut cmd = match name {
        "redis" => {
            let mut c = Command::new("redis-server");
            c.args(["--port", &port.to_string(), "--bind", "127.0.0.1", "--save", "", "--appendonly", "no"]);
            c.arg("--dir").arg(&dir);
            c
        }
        _ => {
            let mut c = Command::new("dragonfly");
            c.args(["--port", &port.to_string(), "--bind", "127.0.0.1", "--dbfilename", "", "--proactor_threads", "2"]);
            c.arg("--dir").arg(&dir);
            c
        }
    };
    let child = cmd.stdout(Stdio::null()).stderr(Stdio::null()).spawn().ok()?;
    let s = Server { name, child, port, _dir: dir };
    for _ in 0..100 {
        if std::net::TcpStream::connect(("127.0.0.1", port)).is_ok() {
            return Some(s);
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    None
}

fn hostile_payloads(real_frame: &[u8]) -> Vec<(&'static str, Vec<u8>)> {
    let mut rng = rand_chacha::ChaCha8Rng::seed_from_u64(9);
    let mut big = vec![0u8; 1 << 20];
    rng.fill(&mut big[..]);
    vec![
        ("empty", vec![]),
        ("all_256_byte_values", (0..=255u8).collect()),
        ("nul_bytes_only", vec![0u8; 64]),
        ("crlf_and_resp_lookalikes", b"\r\n$5\r\nhello\r\n*1\r\n+OK\r\n-ERR\r\n\r\n".to_vec()),
        ("invalid_utf8", vec![0xff, 0xfe, 0x80, 0xc0, 0xaf, 0xed, 0xa0, 0x80, 0xf8, 0x88]),
        ("random_1_mib", big),
        ("real_binary_frame", real_frame.to_vec()),
    ]
}

/// A real batched frame of the binary format, and typical message samples.
fn samples() -> (Vec<u8>, Vec<Vec<u8>>, Vec<Vec<u8>>) {
    use mdbin::transcode::{Config, Transcoder};
    let mut feed = synth::Feed::new(synth::REGIMES[1], synth::FeedConfig::default());
    let mut t = Transcoder::new(Config { deltas: true, suppress_unchanged: false });
    let mut ev = Vec::new();
    let (mut json_msgs, mut bin_msgs, mut frame) = (Vec::new(), Vec::new(), Vec::new());
    for tick in 0..60 {
        ev.clear();
        feed.tick(&mut ev);
        frame = vec![mdbin::msg::MAGIC_VERSION];
        for e in &ev {
            let mut one = Vec::new();
            t.push(&e.json, &mut one).unwrap();
            frame.extend_from_slice(&one);
            if tick >= 50 && !one.is_empty() {
                json_msgs.push(e.json.as_bytes().to_vec());
                let mut f = vec![mdbin::msg::MAGIC_VERSION];
                f.extend_from_slice(&one);
                bin_msgs.push(f);
            }
        }
    }
    (frame, json_msgs, bin_msgs)
}

async fn exactness(url: &str, real_frame: &[u8]) -> redis::RedisResult<Value> {
    let client = redis::Client::open(url)?;
    let mut publisher = client.get_multiplexed_async_connection().await?;
    let mut sub = client.get_async_pubsub().await?;
    sub.subscribe("md").await?;
    let mut stream = sub.into_on_message();
    let mut out = serde_json::Map::new();
    for (name, payload) in hostile_payloads(real_frame) {
        let _: i64 = redis::cmd("PUBLISH").arg("md").arg(&payload[..]).query_async(&mut publisher).await?;
        let got = tokio::time::timeout(Duration::from_secs(5), stream.next()).await.ok().flatten();
        let pub_ok = got.as_ref().is_some_and(|m| m.get_payload_bytes() == payload.as_slice());
        // The cache path: SET then GET must also be exact.
        let _: () = redis::cmd("SET").arg("cache").arg(&payload[..]).query_async(&mut publisher).await?;
        let back: Vec<u8> = redis::cmd("GET").arg("cache").query_async(&mut publisher).await?;
        out.insert(name.into(), json!({"bytes": payload.len(), "publish_exact": pub_ok, "set_get_exact": back == payload}));
    }
    Ok(Value::Object(out))
}

fn strip_ansi(s: &str) -> String {
    let mut out = String::new();
    let mut esc = false;
    for c in s.chars() {
        match (esc, c) {
            (false, '\u{1b}') => esc = true,
            (true, 'm') => esc = false,
            (true, _) => {}
            (false, c) => out.push(c),
        }
    }
    out
}

fn pct(v: &mut [u64], p: f64) -> u64 {
    v.sort_unstable();
    v[((v.len() as f64 - 1.0) * p).round() as usize]
}

/// Paced publishing: what one receives at a fixed rate.
async fn latency(url: &str, payloads: &[Vec<u8>], rate: u64, secs: u64) -> redis::RedisResult<Value> {
    let client = redis::Client::open(url)?;
    let mut publisher = client.get_multiplexed_async_connection().await?;
    let mut sub = client.get_async_pubsub().await?;
    sub.subscribe("lat").await?;
    let mut stream = sub.into_on_message();
    let base = Instant::now();
    let n = rate * secs;
    let rx = tokio::spawn(async move {
        let mut lat = Vec::with_capacity(n as usize);
        while let Ok(Some(m)) = tokio::time::timeout(Duration::from_secs(3), stream.next()).await {
            let p = m.get_payload_bytes();
            let sent = u64::from_le_bytes(p[..8].try_into().unwrap());
            lat.push(base.elapsed().as_nanos() as u64 - sent);
            if lat.len() as u64 == n {
                break;
            }
        }
        lat
    });
    let mut tick = tokio::time::interval(Duration::from_nanos(1_000_000_000 / rate));
    let mut bytes = 0u64;
    for i in 0..n {
        tick.tick().await;
        let body = &payloads[i as usize % payloads.len()];
        let mut p = (base.elapsed().as_nanos() as u64).to_le_bytes().to_vec();
        p.extend_from_slice(body);
        bytes += p.len() as u64;
        let _: i64 = redis::cmd("PUBLISH").arg("lat").arg(p).query_async(&mut publisher).await?;
    }
    let mut lat = rx.await.unwrap();
    let got = lat.len();
    Ok(json!({
        "rate_msgs_per_s": rate, "sent": n, "received": got, "mean_payload_bytes": bytes as f64 / n as f64,
        "p50_us": pct(&mut lat, 0.5) as f64 / 1e3, "p99_us": pct(&mut lat, 0.99) as f64 / 1e3,
        "p999_us": pct(&mut lat, 0.999) as f64 / 1e3, "max_us": pct(&mut lat, 1.0) as f64 / 1e3,
    }))
}

/// Pipelined publishing as fast as the client can: a ceiling, not a target.
async fn ceiling(url: &str, payloads: &[Vec<u8>], n: usize) -> redis::RedisResult<Value> {
    let client = redis::Client::open(url)?;
    let mut publisher = client.get_multiplexed_async_connection().await?;
    let mut sub = client.get_async_pubsub().await?;
    sub.subscribe("max").await?;
    let mut stream = sub.into_on_message();
    let rx = tokio::spawn(async move {
        let mut got = 0usize;
        let mut last = Instant::now();
        while let Ok(Some(_)) = tokio::time::timeout(Duration::from_secs(3), stream.next()).await {
            got += 1;
            last = Instant::now();
            if got == n {
                break;
            }
        }
        (got, last)
    });
    let t0 = Instant::now();
    for chunk in (0..n).collect::<Vec<_>>().chunks(1000) {
        let mut pipe = redis::pipe();
        for &i in chunk {
            pipe.cmd("PUBLISH").arg("max").arg(&payloads[i % payloads.len()][..]).ignore();
        }
        let _: () = pipe.query_async(&mut publisher).await?;
    }
    let (got, last) = rx.await.unwrap();
    let secs = (last - t0).as_secs_f64();
    Ok(json!({"published": n, "received": got, "seconds": secs, "received_msgs_per_s": (got as f64 / secs).round()}))
}

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() {
    let out_dir = std::env::args().nth(1).unwrap_or_else(|| "results".into());
    let (frame, json_msgs, bin_msgs) = samples();
    let mean = |v: &[Vec<u8>]| v.iter().map(Vec::len).sum::<usize>() as f64 / v.len() as f64;
    let mut servers = serde_json::Map::new();
    for name in ["redis", "dragonfly"] {
        eprintln!("broker: {name}");
        let Some(s) = start(name) else {
            servers.insert(name.into(), json!({"tested": false, "reason": "server binary not found or did not start"}));
            continue;
        };
        let url = format!("redis://127.0.0.1:{}/", s.port);
        tokio::time::sleep(Duration::from_millis(200)).await;
        let version = Command::new(if name == "redis" { "redis-server" } else { "dragonfly" })
            .arg("--version").output().map(|o| strip_ansi(String::from_utf8_lossy(&o.stdout).lines().next().unwrap_or(""))).unwrap_or_default();
        let mut r = serde_json::Map::new();
        r.insert("tested".into(), true.into());
        r.insert("version".into(), version.into());
        r.insert("byte_exactness".into(), exactness(&url, &frame).await.unwrap());
        let mut lat = serde_json::Map::new();
        for (label, set) in [("json_messages", &json_msgs), ("binary_messages", &bin_msgs)] {
            lat.insert(format!("{label}_2000_per_s"), latency(&url, set, 2000, 5).await.unwrap());
        }
        lat.insert("binary_tick_frames_10_per_s".into(), latency(&url, std::slice::from_ref(&frame), 10, 10).await.unwrap());
        r.insert("publish_to_receive_loopback".into(), Value::Object(lat));
        let mut cap = serde_json::Map::new();
        cap.insert("json_messages".into(), ceiling(&url, &json_msgs, 200_000).await.unwrap());
        cap.insert("binary_messages".into(), ceiling(&url, &bin_msgs, 200_000).await.unwrap());
        r.insert("pipelined_ceiling_one_subscriber".into(), Value::Object(cap));
        servers.insert(s.name.into(), Value::Object(r));
    }
    let doc = json!({
        "label": "MEASURED on this machine's loopback with SYNTHETIC payloads. Not network latency.",
        "mean_json_message_bytes": mean(&json_msgs),
        "mean_binary_message_bytes": mean(&bin_msgs),
        "binary_tick_frame_bytes": frame.len(),
        // Ticks 50..60 are one simulated second of the whole feed.
        "synthetic_feed_messages_per_s": json_msgs.len(),
        "servers": servers,
    });
    std::fs::create_dir_all(&out_dir).unwrap();
    let path = format!("{out_dir}/broker.json");
    std::fs::write(&path, serde_json::to_string_pretty(&doc).unwrap() + "\n").unwrap();
    eprintln!("wrote {path}");
}

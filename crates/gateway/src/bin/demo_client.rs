//! Connects one JSON client and one binary client to a running gateway,
//! subscribes both to the whole feed, and reports what each received.
//! Data is SYNTHETIC.

use futures_util::{SinkExt, StreamExt};
use mdbin::msg::decode_frame;
use mdbin::replica::{Applied, Replica};
use std::time::{Duration, Instant};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message as Ws;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let url = args.next().unwrap_or_else(|| "ws://127.0.0.1:5600".into());
    let secs: u64 = args.next().and_then(|s| s.parse().ok()).unwrap_or(5);
    let names: Vec<String> = synth::Feed::new(synth::REGIMES[1], synth::FeedConfig::default())
        .instrument_names()
        .iter()
        .map(|s| s.to_string())
        .collect();
    let channels: Vec<String> = names
        .iter()
        .flat_map(|n| [format!("orderbook.{n}.1.20"), format!("ticker_slim.{n}.1000"), format!("trades.{n}")])
        .collect();
    let sub = serde_json::json!({"id": 1, "method": "subscribe", "params": {"channels": channels}}).to_string();

    let (mut js, _) = tokio_tungstenite::connect_async(url.as_str()).await?;
    let mut req = url.as_str().into_client_request()?;
    req.headers_mut().insert("sec-websocket-protocol", gateway::SUBPROTOCOL.parse()?);
    let (mut bin, resp) = tokio_tungstenite::connect_async(req).await?;
    println!("binary client negotiated: {:?}", resp.headers().get("sec-websocket-protocol"));
    js.send(Ws::Text(sub.clone().into())).await?;
    bin.send(Ws::Text(sub.into())).await?;

    let deadline = Instant::now() + Duration::from_secs(secs);
    let (mut jb, mut bb, mut jm, mut bm, mut mismatches) = (0usize, 0usize, Vec::new(), 0usize, 0usize);
    let mut rep = Replica::new();
    let mut rebuilt = Vec::new();
    while Instant::now() < deadline {
        tokio::select! {
            Some(Ok(m)) = js.next() => if let Ws::Text(t) = m {
                if t.contains("\"method\":\"subscription\"") { jb += t.len(); jm.push(t.to_string()); }
            },
            Some(Ok(m)) = bin.next() => if let Ws::Binary(b) = m {
                bb += b.len();
                for msg in decode_frame(&b)? {
                    if let Applied::Updated(Some(s)) = rep.apply(&msg, true)? { bm += 1; rebuilt.push(s); }
                }
            },
            _ = tokio::time::sleep(Duration::from_millis(50)) => {}
        }
    }
    // The clients joined at slightly different moments, and the binary
    // client starts from snapshots. Align each channel on its first common
    // message, then compare everything after it.
    let channel = |m: &str| m.split("\"channel\":\"").nth(1).and_then(|r| r.split('"').next()).unwrap_or("").to_owned();
    let mut by_chan: std::collections::HashMap<String, (Vec<&String>, Vec<&String>)> = Default::default();
    for m in &jm {
        by_chan.entry(channel(m)).or_default().0.push(m);
    }
    for m in &rebuilt {
        by_chan.entry(channel(m)).or_default().1.push(m);
    }
    let mut compared = 0usize;
    for (j, b) in by_chan.values() {
        let Some((bi, ji)) = b.iter().enumerate().find_map(|(bi, m)| j.iter().position(|x| x == m).map(|ji| (bi, ji))) else { continue };
        for (x, y) in j[ji..].iter().zip(&b[bi..]) {
            compared += 1;
            if x != y {
                mismatches += 1;
            }
        }
    }
    println!("compared {compared} messages after aligning each channel");
    println!("JSON client:   {jm_len} messages, {jb} bytes", jm_len = jm.len());
    println!("binary client: {bm} messages rebuilt, {bb} bytes on the socket ({:.1}% of JSON)", 100.0 * bb as f64 / jb.max(1) as f64);
    println!("rebuilt JSON identical to the JSON client's stream: {}", if mismatches == 0 { "yes" } else { "NO" });
    println!("(SYNTHETIC data; byte counts are WebSocket payloads on loopback)");
    Ok(())
}

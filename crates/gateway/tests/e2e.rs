//! End to end over real sockets: mock core -> ZMQ -> gateway -> WebSocket.
//! Data is SYNTHETIC.

use futures_util::{SinkExt, StreamExt};
use gateway::core_mock::{run_market_data, run_orders, CoreStats};
use gateway::{run_order_bus, run_upstream, serve, Hub, Registry, SUBPROTOCOL};
use mdbin::msg::*;
use mdbin::order::{address_of, SignedContext, CHAIN_TESTNET};
use mdbin::replica::{Applied, Replica};
use mdbin::transcode::Config;
use std::sync::atomic::Ordering::Relaxed;
use std::sync::Arc;
use std::time::Duration;
use synth::{FeedConfig, REGIMES};
use tokio::net::TcpListener;
use tokio::sync::Notify;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message as Ws;
use zeromq::Socket;

struct TestRegistry;
impl Registry for TestRegistry {
    fn context(&self, _: &str, _: i64) -> Option<SignedContext> {
        Some(SignedContext {
            asset: [0xAB; 20],
            sub_id: 39614082545622357598554435968,
            owner: address_of(synth::test_key().verifying_key()),
            chain_id: CHAIN_TESTNET,
        })
    }
}

async fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0").await.unwrap().local_addr().unwrap().port()
}

struct Rig {
    ws_url: String,
    hub: Arc<Hub>,
    core: Arc<CoreStats>,
    go: Arc<Notify>,
}

async fn rig(ticks: u64) -> Rig {
    let (pp, op) = (free_port().await, free_port().await);
    let mut publisher = zeromq::PubSocket::new();
    publisher.bind(&format!("tcp://127.0.0.1:{pp}")).await.unwrap();
    let mut router = zeromq::RouterSocket::new();
    router.bind(&format!("tcp://127.0.0.1:{op}")).await.unwrap();
    let core = Arc::new(CoreStats::default());
    let go = Arc::new(Notify::new());
    let cfg = FeedConfig { expiries: 1, strikes: 3, ..Default::default() };
    tokio::spawn(run_orders(router, core.clone()));
    tokio::spawn(run_market_data(publisher, REGIMES[2], cfg, Duration::from_millis(20), ticks, go.clone(), core.clone()));

    let hub = Hub::new(Config { deltas: true, suppress_unchanged: false });
    let bus = run_order_bus(format!("tcp://127.0.0.1:{op}")).await.unwrap();
    tokio::spawn(run_upstream(hub.clone(), format!("tcp://127.0.0.1:{pp}")));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let ws_url = format!("ws://{}", listener.local_addr().unwrap());
    tokio::spawn(serve(listener, hub.clone(), bus, Arc::new(TestRegistry)));
    // Let the ZMQ subscription settle before anything is published.
    tokio::time::sleep(Duration::from_millis(300)).await;
    Rig { ws_url, hub, core, go }
}

type Client = tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

async fn connect(url: &str, binary: bool) -> (Client, bool) {
    let mut req = url.into_client_request().unwrap();
    if binary {
        req.headers_mut().insert("sec-websocket-protocol", format!("other.v9, {SUBPROTOCOL}").parse().unwrap());
    }
    let (ws, resp) = tokio_tungstenite::connect_async(req).await.unwrap();
    let agreed = resp.headers().get("sec-websocket-protocol").is_some_and(|v| v == SUBPROTOCOL);
    (ws, agreed)
}

async fn collect_bin(ws: &mut Client) -> (Vec<String>, usize) {
    let mut rep = Replica::new();
    let (mut v, mut bytes) = (Vec::new(), 0usize);
    while let Ok(Some(Ok(m))) = tokio::time::timeout(Duration::from_millis(800), ws.next()).await {
        if let Ws::Binary(b) = m {
            bytes += b.len();
            for msg in decode_frame(&b).unwrap() {
                if let Applied::Updated(Some(s)) = rep.apply(&msg, true).unwrap() {
                    v.push(s);
                }
            }
        }
    }
    (v, bytes)
}

const CHANNELS: [&str; 3] = ["orderbook.ETH-PERP.1.20", "ticker_slim.ETH-PERP.1000", "trades.ETH-PERP"];

async fn subscribe(ws: &mut Client) {
    let req = serde_json::json!({"id": 1, "method": "subscribe", "params": {"channels": CHANNELS}});
    ws.send(Ws::Text(req.to_string().into())).await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn binary_clients_rebuild_exactly_what_json_clients_get() {
    let r = rig(60).await;
    let (mut js, agreed_js) = connect(&r.ws_url, false).await;
    let (mut bin, agreed_bin) = connect(&r.ws_url, true).await;
    let (mut bin2, _) = connect(&r.ws_url, true).await;
    assert!(!agreed_js && agreed_bin, "subprotocol negotiation");
    subscribe(&mut js).await;
    subscribe(&mut bin).await;
    subscribe(&mut bin2).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    r.go.notify_one();

    let collect_json = async {
        let mut v = Vec::new();
        while let Ok(Some(Ok(m))) = tokio::time::timeout(Duration::from_millis(800), js.next()).await {
            if let Ws::Text(t) = m {
                if t.contains("\"method\":\"subscription\"") {
                    v.push(t.to_string());
                }
            }
        }
        v
    };
    let (json_msgs, (bin_msgs, bin_bytes), (bin2_msgs, _)) =
        tokio::join!(collect_json, collect_bin(&mut bin), collect_bin(&mut bin2));

    assert!(json_msgs.len() > 60, "got {} json messages", json_msgs.len());
    assert_eq!(bin_msgs, json_msgs, "binary stream must rebuild the JSON stream byte for byte");
    assert_eq!(bin2_msgs, json_msgs);
    let json_bytes: usize = json_msgs.iter().map(String::len).sum();
    assert!(bin_bytes * 2 < json_bytes, "binary {bin_bytes} vs json {json_bytes}");

    // Encode once: one transcode per upstream message, however many clients.
    let s = &r.hub.stats;
    assert_eq!(s.transcodes.load(Relaxed), s.upstream_messages.load(Relaxed));
    // ZMQ PUB/SUB drops silently when a subscriber falls behind its high-water
    // mark, and that does happen in debug builds at this tick rate. The binary
    // stream stays correct anyway: upstream JSON books are full snapshots, so
    // the transcoder diffs against whatever it last received.
    let (seen, sent) = (s.upstream_messages.load(Relaxed), r.core.published.load(Relaxed));
    assert!(seen <= sent && seen > 0);
    if seen < sent {
        eprintln!("note: ZMQ PUB/SUB dropped {} of {sent} upstream messages", sent - seen);
    }
    assert_eq!(s.binary_clients.load(Relaxed), 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn signed_orders_flow_both_ways_and_bad_ones_stop_at_the_edge() {
    let r = rig(20).await;
    r.go.notify_one();
    tokio::time::sleep(Duration::from_millis(200)).await; // instruments now known
    let (mut bin, _) = connect(&r.ws_url, true).await;
    let (mut js, _) = connect(&r.ws_url, false).await;
    let mut rng = synth::order_rng(3);
    let (_, good, _) = synth::sample_order(&mut rng, 77);
    let instrument = r.hub.stats.upstream_messages.load(Relaxed); // non-zero: data flowed
    assert!(instrument > 0);

    // Binary order on instrument id 1 (ETH-PERP).
    let h = Header { ty: TYPE_NEW_ORDER, instrument: 1, ..Default::default() };
    let f = encode_frame(&h, &Body::NewOrder(Box::new(good.clone()))).unwrap();
    bin.send(Ws::Binary(f.into())).await.unwrap();
    let ack = loop {
        let m = tokio::time::timeout(Duration::from_secs(3), bin.next()).await.unwrap().unwrap().unwrap();
        if let Ws::Binary(b) = m {
            if let Some(Body::OrderAck(a)) = decode_frame(&b).unwrap().into_iter().map(|m| m.body).next() {
                break a;
            }
        }
    };
    assert!(ack.accepted && ack.rpc_id == 77);

    // Same order as JSON gets the same treatment.
    let json = mdbin::order::render_order_json("ETH-PERP", &good);
    js.send(Ws::Text(json.into())).await.unwrap();
    let reply = loop {
        let m = tokio::time::timeout(Duration::from_secs(3), js.next()).await.unwrap().unwrap().unwrap();
        if let Ws::Text(t) = m {
            break t.to_string();
        }
    };
    assert!(reply.contains("\"order_status\":\"open\""), "{reply}");

    // A tampered order never reaches the core.
    let mut bad = good.clone();
    bad.limit_price = mdbin::dec::Dec::parse("999").unwrap();
    let f = encode_frame(&h, &Body::NewOrder(Box::new(bad))).unwrap();
    bin.send(Ws::Binary(f.into())).await.unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(r.core.orders_received.load(Relaxed), 2);
    assert_eq!(r.hub.stats.orders_rejected.load(Relaxed), 1);

    // Garbage closes that one connection; the gateway keeps serving.
    bin.send(Ws::Binary(vec![0xB1, 0xff, 0xff, 0xff].into())).await.unwrap();
    let closed = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            match bin.next().await {
                Some(Ok(Ws::Close(_))) | None | Some(Err(_)) => return true,
                _ => {}
            }
        }
    })
    .await
    .unwrap_or(false);
    assert!(closed);
    let (mut again, agreed) = connect(&r.ws_url, true).await;
    assert!(agreed);
    again.send(Ws::Text("ping".into())).await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn late_joiner_and_resync_converge() {
    let r = rig(80).await;
    r.go.notify_one();
    tokio::time::sleep(Duration::from_millis(500)).await;
    let (mut bin, _) = connect(&r.ws_url, true).await;
    subscribe(&mut bin).await;
    let mut rep = Replica::new();
    let mut updates = 0;
    let mut asked = false;
    while let Ok(Some(Ok(m))) = tokio::time::timeout(Duration::from_millis(800), bin.next()).await {
        if let Ws::Binary(b) = m {
            for msg in decode_frame(&b).unwrap() {
                if let Applied::Updated(_) = rep.apply(&msg, false).unwrap() {
                    updates += 1;
                }
                // Ask for a resync once, as a client would after a gap.
                if !asked && matches!(msg.body, Body::BookDelta(_)) {
                    asked = true;
                    let chan = mdbin::json::book_chan(1, 20).unwrap();
                    let h = Header { ty: TYPE_RESYNC_REQUEST, instrument: msg.header.instrument, ..Default::default() };
                    let f = encode_frame(&h, &Body::ResyncRequest { family: 1, chan }).unwrap();
                    bin.send(Ws::Binary(f.into())).await.unwrap();
                }
            }
        }
    }
    assert!(asked && updates > 20, "updates {updates}");
    assert_eq!(rep.gaps, 0);
    assert!(rep.book(1, mdbin::json::book_chan(1, 20).unwrap()).is_some());
}

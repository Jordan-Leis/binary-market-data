//! Edge gateway prototype.
//!
//! ```text
//!                 ZMQ PUB (JSON, unchanged)
//!  mock core  ------------------------------>  hub: transcode ONCE per message
//!      ^                                         |
//!      |  ZMQ DEALER/ROUTER over TCP             |  broadcast of shared bytes
//!      |  (one internal order format)            v
//!      +-----------  per-client tasks: filter, batch, write  ---> WebSocket
//! ```
//!
//! A client that offers `Sec-WebSocket-Protocol: mdbin.v1` gets binary. Any
//! other client gets today's JSON. Control messages (subscribe) stay JSON in
//! text frames for both, which is the split Binance documents for its SBE
//! streams. Nothing here is hardened for production: no TLS, no auth, no
//! per-client rate limits.

use bytes::Bytes;
use futures_util::{SinkExt, StreamExt};
use mdbin::msg::*;
use mdbin::order::{parse_order_json, verify_order, SignedContext};
use mdbin::transcode::{Config, Outcome, StreamId, Transcoder};
use serde::Deserialize;
use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use std::sync::{Arc, Mutex};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{broadcast, mpsc};
use tokio_tungstenite::tungstenite::handshake::server::{Request, Response};
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::tungstenite::protocol::{CloseFrame, WebSocketConfig};
use tokio_tungstenite::tungstenite::{Message as WsMessage, Utf8Bytes};
use zeromq::{Socket, SocketRecv, SocketSend};

pub mod core_mock;

pub const SUBPROTOCOL: &str = "mdbin.v1";
/// Clients only send small control and order frames.
pub const MAX_CLIENT_MESSAGE: usize = 64 * 1024;
const BROADCAST_CAPACITY: usize = 1 << 16;
const BATCH_FLUSH_BYTES: usize = 64 * 1024;
/// Each order costs a signature recovery, so a frame may carry only a few.
pub const MAX_ORDERS_PER_FRAME: usize = 16;
/// The public docs cap a connection at 10,000 channels.
pub const MAX_CHANNELS: usize = 10_000;

#[derive(Default)]
pub struct Stats {
    pub upstream_messages: AtomicU64,
    pub transcodes: AtomicU64,
    pub raw_fallbacks: AtomicU64,
    pub binary_clients: AtomicU64,
    pub json_clients: AtomicU64,
    pub rejected_frames: AtomicU64,
    pub orders_forwarded: AtomicU64,
    pub orders_rejected: AtomicU64,
    pub lag_events: AtomicU64,
    pub resync_snapshots: AtomicU64,
    pub bytes_binary: AtomicU64,
    pub bytes_json: AtomicU64,
}

pub struct Published {
    pub channel: Arc<str>,
    pub json: Utf8Bytes,
    /// Message bytes with no frame byte, shared by every binary client.
    pub binary: Bytes,
}

#[derive(Clone)]
pub enum Item {
    Message(Arc<Published>),
    /// The core finished a publish cycle: binary clients flush their batch.
    TickEnd,
}

/// Looks up what an order signature covers but the order does not carry.
pub trait Registry: Send + Sync {
    fn context(&self, instrument: &str, subaccount_id: i64) -> Option<SignedContext>;
}

pub struct Hub {
    transcoder: Mutex<Transcoder>,
    streams: Mutex<HashMap<Arc<str>, StreamId>>,
    tx: broadcast::Sender<Item>,
    pub stats: Stats,
}

fn channel_of(json: &str) -> Option<&str> {
    let key = "\"channel\":\"";
    let start = json.find(key)? + key.len();
    let end = json[start..].find('"')?;
    Some(&json[start..start + end])
}

impl Hub {
    pub fn new(config: Config) -> Arc<Self> {
        let (tx, _) = broadcast::channel(BROADCAST_CAPACITY);
        Arc::new(Self {
            transcoder: Mutex::new(Transcoder::new(config)),
            streams: Mutex::new(HashMap::new()),
            tx,
            stats: Stats::default(),
        })
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Item> {
        self.tx.subscribe()
    }

    /// The only place JSON becomes binary. Runs once per upstream message, no
    /// matter how many clients are connected.
    pub fn publish(&self, json: String) {
        self.stats.upstream_messages.fetch_add(1, Relaxed);
        let Some(channel) = channel_of(&json).map(Arc::<str>::from) else { return };
        let mut binary = Vec::with_capacity(json.len() / 3);
        let outcome = {
            let mut t = self.transcoder.lock().expect("transcoder lock");
            self.stats.transcodes.fetch_add(1, Relaxed);
            t.push(&json, &mut binary)
        };
        match outcome {
            Ok(Outcome::Sent { stream, .. }) => {
                self.streams.lock().expect("streams lock").entry(channel.clone()).or_insert(stream);
            }
            Ok(Outcome::Raw) => {
                self.stats.raw_fallbacks.fetch_add(1, Relaxed);
            }
            Ok(Outcome::Suppressed { .. }) => {}
            Err(_) => return,
        }
        let json = Utf8Bytes::from(json);
        let msg = Published { channel, json, binary: binary.into() };
        // An error only means there are no subscribers yet.
        let _ = self.tx.send(Item::Message(Arc::new(msg)));
    }

    pub fn tick_end(&self) {
        let _ = self.tx.send(Item::TickEnd);
    }

    /// Directory plus current snapshots for these channels. Taken after the
    /// client's broadcast receiver exists, so nothing can fall between them:
    /// anything older than the snapshot is dropped by sequence number.
    fn join_frame(&self, channels: &HashSet<Arc<str>>, flags: u8) -> Option<Vec<u8>> {
        let mut out = vec![MAGIC_VERSION];
        let t = self.transcoder.lock().expect("transcoder lock");
        t.write_directory(&mut out).ok()?;
        let streams = self.streams.lock().expect("streams lock");
        for c in channels {
            if let Some(s) = streams.get(c) {
                t.snapshot_of(*s, flags, &mut out).ok()?;
            }
        }
        Some(out)
    }

    fn resync_frame(&self, instrument: u32, family: u8, chan: u8) -> Option<Vec<u8>> {
        let family = mdbin::transcode::Family::from_u8(family)?;
        let mut out = vec![MAGIC_VERSION];
        let stream = StreamId { instrument, family, chan };
        let t = self.transcoder.lock().expect("transcoder lock");
        t.snapshot_of(stream, FLAG_RESYNC, &mut out).ok()?.then_some(out)
    }

    fn instrument_name(&self, id: u32) -> Option<String> {
        let t = self.transcoder.lock().expect("transcoder lock");
        t.instruments().get((id as usize).checked_sub(1)?).cloned()
    }

    fn instrument_id(&self, name: &str) -> Option<u32> {
        let t = self.transcoder.lock().expect("transcoder lock");
        t.instruments().iter().position(|n| n == name).map(|i| i as u32 + 1)
    }
}

/// Reads market data from the core over ZMQ PUB/SUB.
pub async fn run_upstream(hub: Arc<Hub>, endpoint: String) -> Result<(), zeromq::ZmqError> {
    let mut sub = zeromq::SubSocket::new();
    sub.connect(&endpoint).await?;
    sub.subscribe("").await?;
    loop {
        let m = sub.recv().await?;
        match (m.get(0).map(|b| b.as_ref()), m.get(1)) {
            (Some(b"tick"), _) => hub.tick_end(),
            (Some(b"md"), Some(body)) => {
                if let Ok(s) = std::str::from_utf8(body) {
                    hub.publish(s.to_owned());
                }
            }
            _ => {}
        }
    }
}

/// One internal order message for the core: `[connection id][binary frame]`.
pub struct OrderOut {
    pub conn: u64,
    pub frame: Vec<u8>,
}

/// An ack routed back to the connection that sent the order.
pub enum ClientNote {
    Ack(OrderAck),
}

#[derive(Clone)]
pub struct OrderBus {
    tx: mpsc::Sender<OrderOut>,
    routes: Arc<Mutex<HashMap<u64, mpsc::Sender<ClientNote>>>>,
}

/// Connects to the core and carries orders and acks over ZMQ on TCP.
pub async fn run_order_bus(endpoint: String) -> Result<OrderBus, zeromq::ZmqError> {
    let mut dealer = zeromq::DealerSocket::new();
    dealer.connect(&endpoint).await?;
    let (mut send, mut recv) = dealer.split();
    let (tx, mut rx) = mpsc::channel::<OrderOut>(4096);
    let routes: Arc<Mutex<HashMap<u64, mpsc::Sender<ClientNote>>>> = Arc::default();
    tokio::spawn(async move {
        while let Some(o) = rx.recv().await {
            let parts = vec![Bytes::copy_from_slice(&o.conn.to_le_bytes()), Bytes::from(o.frame)];
            if let Ok(m) = zeromq::ZmqMessage::try_from(parts) {
                if send.send(m).await.is_err() {
                    break;
                }
            }
        }
    });
    let table = routes.clone();
    tokio::spawn(async move {
        while let Ok(m) = recv.recv().await {
            let (Some(conn), Some(frame)) = (m.get(0), m.get(1)) else { continue };
            let Ok(id) = <[u8; 8]>::try_from(conn.as_ref()) else { continue };
            let Ok(msgs) = decode_frame(frame) else { continue };
            let route = table.lock().expect("routes lock").get(&u64::from_le_bytes(id)).cloned();
            if let Some(route) = route {
                for msg in msgs {
                    if let Body::OrderAck(ack) = msg.body {
                        let _ = route.try_send(ClientNote::Ack(ack));
                    }
                }
            }
        }
    });
    Ok(OrderBus { tx, routes })
}

pub async fn serve(
    listener: TcpListener,
    hub: Arc<Hub>,
    bus: OrderBus,
    registry: Arc<dyn Registry>,
) -> std::io::Result<()> {
    let next = Arc::new(AtomicU64::new(1));
    loop {
        let (stream, peer) = listener.accept().await?;
        let (hub, bus, registry) = (hub.clone(), bus.clone(), registry.clone());
        let conn = next.fetch_add(1, Relaxed);
        tokio::spawn(async move {
            let _ = client_task(stream, peer, conn, hub, bus, registry).await;
        });
    }
}

#[derive(Deserialize)]
struct Control {
    id: Option<serde_json::Value>,
    method: String,
    #[serde(default)]
    params: serde_json::Value,
}

/// True if the client offered our subprotocol. The list is comma separated.
pub fn offers_binary(header: Option<&HeaderValue>) -> bool {
    header
        .and_then(|h| h.to_str().ok())
        .is_some_and(|s| s.split(',').any(|p| p.trim() == SUBPROTOCOL))
}

async fn client_task(
    stream: TcpStream,
    _peer: SocketAddr,
    conn: u64,
    hub: Arc<Hub>,
    bus: OrderBus,
    registry: Arc<dyn Registry>,
) -> Result<(), tokio_tungstenite::tungstenite::Error> {
    stream.set_nodelay(true)?;
    let binary_flag = Arc::new(AtomicBool::new(false));
    let flag = binary_flag.clone();
    let negotiate = move |req: &Request, mut resp: Response| {
        if offers_binary(req.headers().get("sec-websocket-protocol")) {
            flag.store(true, Relaxed);
            resp.headers_mut().insert("sec-websocket-protocol", HeaderValue::from_static(SUBPROTOCOL));
        }
        Ok(resp)
    };
    let config = WebSocketConfig::default()
        .max_message_size(Some(MAX_CLIENT_MESSAGE))
        .max_frame_size(Some(MAX_CLIENT_MESSAGE));
    let ws = tokio_tungstenite::accept_hdr_async_with_config(stream, negotiate, Some(config)).await?;
    let binary = binary_flag.load(Relaxed);
    let counter = if binary { &hub.stats.binary_clients } else { &hub.stats.json_clients };
    counter.fetch_add(1, Relaxed);

    let (mut sink, mut source) = ws.split();
    let mut feed = hub.subscribe();
    let (note_tx, mut notes) = mpsc::channel::<ClientNote>(256);
    bus.routes.lock().expect("routes lock").insert(conn, note_tx);

    let mut channels: HashSet<Arc<str>> = HashSet::new();
    let mut batch: Vec<u8> = vec![MAGIC_VERSION];

    let result = loop {
        tokio::select! {
            item = feed.recv() => match item {
                Ok(Item::Message(m)) => {
                    if !channels.contains(&m.channel) || m.binary.is_empty() && binary {
                        continue;
                    }
                    if binary {
                        // Shared bytes, copied into this client's frame. No re-encoding.
                        batch.extend_from_slice(&m.binary);
                        if batch.len() >= BATCH_FLUSH_BYTES {
                            flush_batch(&mut sink, &mut batch, &hub).await?;
                        }
                    } else {
                        hub.stats.bytes_json.fetch_add(m.json.len() as u64, Relaxed);
                        sink.feed(WsMessage::Text(m.json.clone())).await?;
                    }
                }
                Ok(Item::TickEnd) => {
                    if binary {
                        flush_batch(&mut sink, &mut batch, &hub).await?;
                    } else {
                        sink.flush().await?;
                    }
                }
                Err(broadcast::error::RecvError::Lagged(_)) => {
                    // This client fell behind. Binary state is now wrong, so
                    // replace it with fresh snapshots instead of a backlog.
                    hub.stats.lag_events.fetch_add(1, Relaxed);
                    if binary {
                        batch.truncate(1);
                        if let Some(f) = hub.join_frame(&channels, FLAG_RESYNC) {
                            hub.stats.resync_snapshots.fetch_add(1, Relaxed);
                            sink.send(WsMessage::Binary(f.into())).await?;
                        }
                    }
                }
                Err(broadcast::error::RecvError::Closed) => break Ok(()),
            },
            Some(note) = notes.recv() => {
                let ClientNote::Ack(ack) = note;
                send_ack(&mut sink, binary, ack).await?;
            }
            msg = source.next() => match msg {
                Some(Ok(WsMessage::Text(t))) => {
                    let action = handle_text(&t, binary, conn, &hub, &bus, &*registry, &mut channels);
                    match action {
                        TextAction::Reply(s) => sink.send(WsMessage::Text(s.into())).await?,
                        TextAction::ReplyThenFrame(s, f) => {
                            sink.send(WsMessage::Text(s.into())).await?;
                            sink.send(WsMessage::Binary(f.into())).await?;
                        }
                        TextAction::None => {}
                        TextAction::Reject => {
                            hub.stats.rejected_frames.fetch_add(1, Relaxed);
                            break close(&mut sink).await;
                        }
                    }
                }
                Some(Ok(WsMessage::Binary(b))) => {
                    match handle_binary(&b, conn, &hub, &bus, &*registry) {
                        Ok(frames) => {
                            for frame in frames {
                                sink.send(WsMessage::Binary(frame.into())).await?;
                            }
                        }
                        Err(_) => {
                            hub.stats.rejected_frames.fetch_add(1, Relaxed);
                            break close(&mut sink).await;
                        }
                    }
                }
                Some(Ok(_)) => {}
                Some(Err(e)) => {
                    hub.stats.rejected_frames.fetch_add(1, Relaxed);
                    break Err(e);
                }
                None => break Ok(()),
            },
        }
    };
    bus.routes.lock().expect("routes lock").remove(&conn);
    result
}

type Sink = futures_util::stream::SplitSink<tokio_tungstenite::WebSocketStream<TcpStream>, WsMessage>;

async fn close(sink: &mut Sink) -> Result<(), tokio_tungstenite::tungstenite::Error> {
    let frame = CloseFrame { code: CloseCode::Policy, reason: "malformed frame".into() };
    let _ = sink.send(WsMessage::Close(Some(frame))).await;
    Ok(())
}

async fn flush_batch(
    sink: &mut Sink,
    batch: &mut Vec<u8>,
    hub: &Hub,
) -> Result<(), tokio_tungstenite::tungstenite::Error> {
    if batch.len() > 1 {
        hub.stats.bytes_binary.fetch_add(batch.len() as u64, Relaxed);
        let frame = std::mem::replace(batch, vec![MAGIC_VERSION]);
        sink.send(WsMessage::Binary(frame.into())).await?;
    }
    Ok(())
}

async fn send_ack(
    sink: &mut Sink,
    binary: bool,
    ack: OrderAck,
) -> Result<(), tokio_tungstenite::tungstenite::Error> {
    if binary {
        let h = Header { ty: TYPE_ORDER_ACK, ..Default::default() };
        if let Ok(f) = encode_frame(&h, &Body::OrderAck(ack)) {
            sink.send(WsMessage::Binary(f.into())).await?;
        }
    } else {
        let id = mdbin::json::id_text(&Id::Uuid(ack.order_id));
        let status = if ack.accepted { "open" } else { "rejected" };
        let s = format!(r#"{{"id":{},"result":{{"order":{{"order_id":"{id}","order_status":"{status}"}},"trades":[]}}}}"#, ack.rpc_id);
        sink.send(WsMessage::Text(s.into())).await?;
    }
    Ok(())
}

enum TextAction {
    None,
    Reply(String),
    ReplyThenFrame(String, Vec<u8>),
    Reject,
}

fn handle_text(
    text: &str,
    binary: bool,
    conn: u64,
    hub: &Hub,
    bus: &OrderBus,
    registry: &dyn Registry,
    channels: &mut HashSet<Arc<str>>,
) -> TextAction {
    let Ok(ctl) = serde_json::from_str::<Control>(text) else {
        return if text == "ping" { TextAction::Reply("pong".to_owned()) } else { TextAction::Reject };
    };
    let id = ctl.id.unwrap_or(serde_json::Value::Null);
    match ctl.method.as_str() {
        "subscribe" => {
            let Some(list) = ctl.params.get("channels").and_then(|c| c.as_array()) else {
                return TextAction::Reject;
            };
            let mut status = serde_json::Map::new();
            if channels.len() + list.len() > MAX_CHANNELS {
                return TextAction::Reject;
            }
            for c in list.iter().filter_map(|c| c.as_str()) {
                channels.insert(Arc::from(c));
                status.insert(c.to_owned(), "ok".into());
            }
            let current: Vec<&str> = channels.iter().map(|c| &**c).collect();
            let reply = serde_json::json!({"id": id, "result": {"status": status, "current_subscriptions": current}});
            if binary {
                if let Some(frame) = hub.join_frame(channels, 0) {
                    return TextAction::ReplyThenFrame(reply.to_string(), frame);
                }
            }
            TextAction::Reply(reply.to_string())
        }
        "private/order" => match parse_order_json(text) {
            Ok((name, order)) => match hub.instrument_id(&name) {
                Some(instrument) => {
                    let verdict = forward_order(conn, instrument, &name, order, hub, bus, registry);
                    match verdict {
                        Ok(()) => TextAction::None,
                        Err(reason) => TextAction::Reply(
                            serde_json::json!({"id": id, "error": {"code": -32000, "message": reason}}).to_string(),
                        ),
                    }
                }
                None => TextAction::Reply(
                    serde_json::json!({"id": id, "error": {"code": -32602, "message": "unknown instrument"}}).to_string(),
                ),
            },
            Err(_) => TextAction::Reply(
                serde_json::json!({"id": id, "error": {"code": -32602, "message": "order not supported by this prototype"}})
                    .to_string(),
            ),
        },
        _ => TextAction::Reply(serde_json::json!({"id": id, "error": {"code": -32601, "message": "method not found"}}).to_string()),
    }
}

/// Clients may only send orders and resync requests in binary. Anything else
/// closes the connection.
fn handle_binary(
    frame: &[u8],
    conn: u64,
    hub: &Hub,
    bus: &OrderBus,
    registry: &dyn Registry,
) -> Result<Vec<Vec<u8>>, mdbin::Error> {
    let msgs = decode_frame(frame)?;
    if msgs.iter().filter(|m| matches!(m.body, Body::NewOrder(_))).count() > MAX_ORDERS_PER_FRAME {
        return Err(mdbin::Error::CountTooLarge);
    }
    let mut replies = Vec::new();
    for m in msgs {
        match m.body {
            Body::NewOrder(order) => {
                let name = hub.instrument_name(m.header.instrument).ok_or(mdbin::Error::UnknownInstrument)?;
                let rpc_id = order.rpc_id;
                if forward_order(conn, m.header.instrument, &name, *order, hub, bus, registry).is_err() {
                    let ack = OrderAck { rpc_id, accepted: false, order_id: [0; 16], reason: 1 };
                    let h = Header { ty: TYPE_ORDER_ACK, ..Default::default() };
                    replies.push(encode_frame(&h, &Body::OrderAck(ack))?);
                }
            }
            Body::ResyncRequest { family, chan } => {
                hub.stats.resync_snapshots.fetch_add(1, Relaxed);
                replies.extend(hub.resync_frame(m.header.instrument, family, chan));
            }
            _ => return Err(mdbin::Error::BadEnum),
        }
    }
    Ok(replies)
}

/// Checks the signature at the edge, then hands one internal format to the
/// core. JSON and binary clients both end up here.
fn forward_order(
    conn: u64,
    instrument: u32,
    name: &str,
    order: NewOrder,
    hub: &Hub,
    bus: &OrderBus,
    registry: &dyn Registry,
) -> Result<(), &'static str> {
    let Some(ctx) = registry.context(name, order.subaccount_id) else {
        hub.stats.orders_rejected.fetch_add(1, Relaxed);
        return Err("unknown subaccount");
    };
    if !matches!(verify_order(&order, &ctx), Ok(true)) {
        hub.stats.orders_rejected.fetch_add(1, Relaxed);
        return Err("signature does not verify");
    }
    let h = Header { ty: TYPE_NEW_ORDER, instrument, ..Default::default() };
    let frame = encode_frame(&h, &Body::NewOrder(Box::new(order))).map_err(|_| "encode failed")?;
    if bus.tx.try_send(OrderOut { conn, frame }).is_err() {
        hub.stats.orders_rejected.fetch_add(1, Relaxed);
        return Err("gateway overloaded");
    }
    hub.stats.orders_forwarded.fetch_add(1, Relaxed);
    Ok(())
}

//! A MOCK matching core. It does not match anything. It publishes synthetic
//! JSON on ZMQ PUB, the way a real engine would publish book and ticker
//! updates, and acknowledges every order it receives.

use bytes::Bytes;
use mdbin::msg::*;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::Arc;
use std::time::Duration;
use synth::{Event, Feed, FeedConfig, Regime};
use tokio::sync::Notify;
use zeromq::{SocketRecv, SocketSend};

#[derive(Default)]
pub struct CoreStats {
    pub orders_received: AtomicU64,
    pub published: AtomicU64,
}

fn parts(items: Vec<Bytes>) -> Option<zeromq::ZmqMessage> {
    zeromq::ZmqMessage::try_from(items).ok()
}

/// Publishes one synthetic tick per `period`, for `ticks` ticks (0 = forever).
pub async fn run_market_data(
    mut publisher: zeromq::PubSocket,
    regime: Regime,
    cfg: FeedConfig,
    period: Duration,
    ticks: u64,
    start: Arc<Notify>,
    stats: Arc<CoreStats>,
) {
    start.notified().await;
    let mut feed = Feed::new(regime, cfg);
    let mut events: Vec<Event> = Vec::new();
    let mut timer = tokio::time::interval(period);
    let mut n = 0u64;
    loop {
        timer.tick().await;
        events.clear();
        feed.tick(&mut events);
        for e in events.drain(..) {
            let m = parts(vec![Bytes::from_static(b"md"), Bytes::from(e.json)]);
            if let Some(m) = m {
                if publisher.send(m).await.is_err() {
                    return;
                }
                stats.published.fetch_add(1, Relaxed);
            }
        }
        if let Some(m) = parts(vec![Bytes::from_static(b"tick")]) {
            let _ = publisher.send(m).await;
        }
        n += 1;
        if ticks != 0 && n >= ticks {
            return;
        }
    }
}

/// Acknowledges orders. Router frames are `[identity][connection id][frame]`.
pub async fn run_orders(mut router: zeromq::RouterSocket, stats: Arc<CoreStats>) {
    let mut next_id = 1u128;
    while let Ok(m) = router.recv().await {
        let (Some(identity), Some(conn), Some(frame)) = (m.get(0), m.get(1), m.get(2)) else { continue };
        let Ok(msgs) = decode_frame(frame) else { continue };
        for msg in msgs {
            let Body::NewOrder(o) = msg.body else { continue };
            stats.orders_received.fetch_add(1, Relaxed);
            let ack = OrderAck { rpc_id: o.rpc_id, accepted: true, order_id: next_id.to_be_bytes(), reason: 0 };
            next_id += 1;
            let h = Header { ty: TYPE_ORDER_ACK, ..Default::default() };
            let Ok(f) = encode_frame(&h, &Body::OrderAck(ack)) else { continue };
            if let Some(reply) = parts(vec![identity.clone(), conn.clone(), Bytes::from(f)]) {
                if router.send(reply).await.is_err() {
                    return;
                }
            }
        }
    }
}

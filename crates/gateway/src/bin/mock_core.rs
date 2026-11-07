//! Runs the mock core: synthetic data out on ZMQ PUB, orders in on ZMQ ROUTER.
use gateway::core_mock::{run_market_data, run_orders, CoreStats};
use std::sync::Arc;
use std::time::Duration;
use synth::{regime, FeedConfig};
use tokio::sync::Notify;
use zeromq::Socket;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let pub_port = args.next().unwrap_or_else(|| "5601".into());
    let ord_port = args.next().unwrap_or_else(|| "5602".into());
    let period_ms: u64 = args.next().and_then(|s| s.parse().ok()).unwrap_or(100);
    let mut publisher = zeromq::PubSocket::new();
    publisher.bind(&format!("tcp://127.0.0.1:{pub_port}")).await?;
    let mut router = zeromq::RouterSocket::new();
    router.bind(&format!("tcp://127.0.0.1:{ord_port}")).await?;
    let stats = Arc::new(CoreStats::default());
    let go = Arc::new(Notify::new());
    go.notify_one();
    println!("mock core (SYNTHETIC data): PUB :{pub_port}  ROUTER :{ord_port}  tick {period_ms} ms");
    let r = synth::regime("base").unwrap_or(synth::REGIMES[1]);
    let _ = regime;
    tokio::spawn(run_orders(router, stats.clone()));
    run_market_data(publisher, r, FeedConfig::default(), Duration::from_millis(period_ms), 0, go, stats).await;
    Ok(())
}

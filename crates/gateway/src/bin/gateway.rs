//! Runs the edge gateway against a running mock core.
use gateway::{run_order_bus, run_upstream, serve, Hub, Registry};
use mdbin::order::{address_of, SignedContext, CHAIN_TESTNET};
use mdbin::transcode::Config;
use std::sync::Arc;
use tokio::net::TcpListener;

/// Stand-in for the instrument and account tables a real gateway would hold.
struct TestRegistry;

impl Registry for TestRegistry {
    fn context(&self, _instrument: &str, _subaccount: i64) -> Option<SignedContext> {
        Some(SignedContext {
            asset: [0xAB; 20],
            sub_id: 39614082545622357598554435968,
            owner: address_of(synth::test_key().verifying_key()),
            chain_id: CHAIN_TESTNET,
        })
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let ws_port = args.next().unwrap_or_else(|| "5600".into());
    let pub_port = args.next().unwrap_or_else(|| "5601".into());
    let ord_port = args.next().unwrap_or_else(|| "5602".into());
    let hub = Hub::new(Config { deltas: true, suppress_unchanged: false });
    let bus = run_order_bus(format!("tcp://127.0.0.1:{ord_port}")).await?;
    let up = hub.clone();
    tokio::spawn(run_upstream(up, format!("tcp://127.0.0.1:{pub_port}")));
    let listener = TcpListener::bind(format!("127.0.0.1:{ws_port}")).await?;
    println!("gateway: ws://127.0.0.1:{ws_port}  binary subprotocol: {}", gateway::SUBPROTOCOL);
    serve(listener, hub, bus, Arc::new(TestRegistry)).await?;
    Ok(())
}

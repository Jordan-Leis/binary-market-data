//! Compact binary encoding for exchange market data and signed orders.
//!
//! Designed to cut bytes on the wire, with exact JSON round trips.
//! See docs/format-spec.md for the layout and docs/format-research.md for why.

pub mod dec;
pub mod error;
pub mod json;
pub mod msg;
pub mod order;
pub mod replica;
pub mod transcode;
pub mod wire;

pub use error::Error;

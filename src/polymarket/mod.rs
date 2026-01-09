//! Polymarket integration module
//!
//! Provides CLOB API client and WebSocket streaming for real-time
//! orderbook and price updates.

pub mod client;
pub mod types;
pub mod websocket;

pub use client::PolymarketClient;
pub use types::*;
pub use websocket::{OrderBookUpdate, PolymarketWs, PriceUpdate};

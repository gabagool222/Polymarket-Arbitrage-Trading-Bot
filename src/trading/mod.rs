//! Trading module
//!
//! Contains strategy implementations and the trading engine
//! for executing hedging and latency arbitrage trades.

pub mod engine;
pub mod strategy;
pub mod types;

pub use engine::TradingEngine;
pub use strategy::{HedgingStrategy, LatencyStrategy, StrategyManager};
pub use types::*;

//! Bybit integration for liquidation stream and impulse detection
//!
//! Subscribes to allLiquidation WebSocket for BTC/ETH/SOL
//! Detects cascade liquidations (>$1M in 10s) for directional trading

use anyhow::{Context, Result};
use futures::{SinkExt, StreamExt};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use tokio::sync::{broadcast, RwLock};
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tracing::{debug, error, info, warn};

// =============================================================================
// Liquidation Types
// =============================================================================

/// Liquidation event from Bybit
#[derive(Debug, Clone)]
pub struct LiquidationEvent {
    pub symbol: String,
    pub side: LiquidationSide,
    pub size: Decimal,
    pub price: Decimal,
    pub value_usd: Decimal,
    pub timestamp: chrono::DateTime<chrono::Utc>,
}

/// Side of liquidation (which positions got liquidated)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LiquidationSide {
    /// Longs getting liquidated (price dropping)
    Buy,  // Bybit sends "Buy" when longs are liquidated (forced sell)
    /// Shorts getting liquidated (price rising)
    Sell, // Bybit sends "Sell" when shorts are liquidated (forced buy)
}

impl std::fmt::Display for LiquidationSide {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LiquidationSide::Buy => write!(f, "LONG_LIQ"),
            LiquidationSide::Sell => write!(f, "SHORT_LIQ"),
        }
    }
}

/// Liquidation cascade detection result
#[derive(Debug, Clone)]
pub struct LiquidationCascade {
    pub symbol: String,
    pub direction: CascadeDirection,
    pub total_value_usd: Decimal,
    pub event_count: u32,
    pub window_seconds: u32,
    pub timestamp: chrono::DateTime<chrono::Utc>,
    /// Suggested Polymarket action based on cascade
    pub suggested_action: CascadeAction,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CascadeDirection {
    /// Longs cascading = price dumping = buy "Down/No" on Polymarket
    LongCascade,
    /// Shorts cascading = price pumping = buy "Up/Yes" on Polymarket
    ShortCascade,
}

impl std::fmt::Display for CascadeDirection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CascadeDirection::LongCascade => write!(f, "LONG_CASCADE_DUMP"),
            CascadeDirection::ShortCascade => write!(f, "SHORT_CASCADE_PUMP"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CascadeAction {
    /// Buy Down/No token (expecting price drop)
    BuyDown,
    /// Buy Up/Yes token (expecting price rise)
    BuyUp,
    /// No clear signal
    Hold,
}

// =============================================================================
// Bybit WebSocket Message Types (from docs)
// =============================================================================

/// Bybit allLiquidation response
/// Docs: https://bybit-exchange.github.io/docs/v5/websocket/public/all-liquidation
#[derive(Debug, Deserialize)]
struct BybitWsResponse {
    topic: String,
    #[serde(rename = "type")]
    msg_type: String,
    ts: u64,
    data: BybitLiquidationData,
}

#[derive(Debug, Deserialize)]
struct BybitLiquidationData {
    /// Update timestamp in ms
    #[serde(rename = "updatedTime")]
    updated_time: u64,
    /// Symbol (e.g., "BTCUSDT")
    symbol: String,
    /// Liquidation side: "Buy" = longs liquidated, "Sell" = shorts liquidated
    side: String,
    /// Liquidation size (qty)
    size: String,
    /// Liquidation price
    price: String,
}

/// Subscription message for Bybit WebSocket
#[derive(Debug, Serialize)]
struct BybitSubscribe {
    op: String,
    args: Vec<String>,
}

// =============================================================================
// Liquidation Aggregator
// =============================================================================

/// Rolling window aggregator for liquidation events
pub struct LiquidationAggregator {
    /// Recent liquidations per symbol: (timestamp, value_usd, side)
    recent: HashMap<String, VecDeque<(chrono::DateTime<chrono::Utc>, Decimal, LiquidationSide)>>,
    /// Window size in seconds
    window_seconds: u32,
    /// Threshold for cascade detection in USD
    cascade_threshold_usd: Decimal,
}

impl LiquidationAggregator {
    pub fn new(window_seconds: u32, cascade_threshold_usd: Decimal) -> Self {
        Self {
            recent: HashMap::new(),
            window_seconds,
            cascade_threshold_usd,
        }
    }

    /// Add a liquidation event and check for cascade
    pub fn add_event(&mut self, event: &LiquidationEvent) -> Option<LiquidationCascade> {
        let now = chrono::Utc::now();
        let cutoff = now - chrono::Duration::seconds(self.window_seconds as i64);

        // Get or create entry for symbol
        let events = self.recent
            .entry(event.symbol.clone())
            .or_insert_with(VecDeque::new);

        // Add new event
        events.push_back((event.timestamp, event.value_usd, event.side));

        // Prune old events outside window
        while events.front().map(|(t, _, _)| *t < cutoff).unwrap_or(false) {
            events.pop_front();
        }

        // Calculate totals by side
        let mut long_liq_total = Decimal::ZERO;
        let mut short_liq_total = Decimal::ZERO;
        let mut long_count = 0u32;
        let mut short_count = 0u32;

        for (_, value, side) in events.iter() {
            match side {
                LiquidationSide::Buy => {
                    long_liq_total += *value;
                    long_count += 1;
                }
                LiquidationSide::Sell => {
                    short_liq_total += *value;
                    short_count += 1;
                }
            }
        }

        // Check for cascade
        if long_liq_total >= self.cascade_threshold_usd {
            return Some(LiquidationCascade {
                symbol: event.symbol.clone(),
                direction: CascadeDirection::LongCascade,
                total_value_usd: long_liq_total,
                event_count: long_count,
                window_seconds: self.window_seconds,
                timestamp: now,
                suggested_action: CascadeAction::BuyDown,
            });
        }

        if short_liq_total >= self.cascade_threshold_usd {
            return Some(LiquidationCascade {
                symbol: event.symbol.clone(),
                direction: CascadeDirection::ShortCascade,
                total_value_usd: short_liq_total,
                event_count: short_count,
                window_seconds: self.window_seconds,
                timestamp: now,
                suggested_action: CascadeAction::BuyUp,
            });
        }

        None
    }

    /// Get current liquidation totals for a symbol (for monitoring)
    pub fn get_totals(&self, symbol: &str) -> (Decimal, Decimal) {
        let events = match self.recent.get(symbol) {
            Some(e) => e,
            None => return (Decimal::ZERO, Decimal::ZERO),
        };

        let now = chrono::Utc::now();
        let cutoff = now - chrono::Duration::seconds(self.window_seconds as i64);

        let mut long_total = Decimal::ZERO;
        let mut short_total = Decimal::ZERO;

        for (ts, value, side) in events.iter() {
            if *ts >= cutoff {
                match side {
                    LiquidationSide::Buy => long_total += *value,
                    LiquidationSide::Sell => short_total += *value,
                }
            }
        }

        (long_total, short_total)
    }
}

// =============================================================================
// Bybit WebSocket Client
// =============================================================================

/// Bybit liquidation WebSocket client
pub struct BybitLiquidationWs {
    /// Symbols to subscribe (e.g., ["BTCUSDT", "ETHUSDT", "SOLUSDT"])
    symbols: Vec<String>,
    /// Rolling window aggregator
    aggregator: Arc<RwLock<LiquidationAggregator>>,
    /// Broadcast channel for cascade events
    cascade_tx: broadcast::Sender<LiquidationCascade>,
    /// Broadcast channel for raw liquidation events
    event_tx: broadcast::Sender<LiquidationEvent>,
}

impl BybitLiquidationWs {
    pub fn new(
        symbols: Vec<String>,
        window_seconds: u32,
        cascade_threshold_usd: Decimal,
    ) -> Self {
        let (cascade_tx, _) = broadcast::channel(100);
        let (event_tx, _) = broadcast::channel(1000);

        Self {
            symbols,
            aggregator: Arc::new(RwLock::new(LiquidationAggregator::new(
                window_seconds,
                cascade_threshold_usd,
            ))),
            cascade_tx,
            event_tx,
        }
    }

    /// Subscribe to cascade events
    pub fn subscribe_cascades(&self) -> broadcast::Receiver<LiquidationCascade> {
        self.cascade_tx.subscribe()
    }

    /// Subscribe to raw liquidation events
    pub fn subscribe_events(&self) -> broadcast::Receiver<LiquidationEvent> {
        self.event_tx.subscribe()
    }

    /// Get current liquidation totals for monitoring
    pub async fn get_totals(&self, symbol: &str) -> (Decimal, Decimal) {
        self.aggregator.read().await.get_totals(symbol)
    }

    /// Connect and run the WebSocket
    pub async fn connect(&self) -> Result<()> {
        // Bybit public linear WebSocket
        let ws_url = "wss://stream.bybit.com/v5/public/linear";

        info!("Connecting to Bybit Liquidation WebSocket: {}", ws_url);

        let (ws_stream, _) = connect_async(ws_url)
            .await
            .context("Failed to connect to Bybit WebSocket")?;

        let (mut write, mut read) = ws_stream.split();

        // Subscribe to allLiquidation for each symbol
        let args: Vec<String> = self.symbols
            .iter()
            .map(|s| format!("allLiquidation.{}", s))
            .collect();

        let subscribe_msg = BybitSubscribe {
            op: "subscribe".to_string(),
            args,
        };

        let msg_str = serde_json::to_string(&subscribe_msg)?;
        debug!("Sending Bybit subscription: {}", msg_str);
        write.send(Message::Text(msg_str)).await?;

        info!(
            "Subscribed to Bybit allLiquidation for: {:?}",
            self.symbols
        );

        // Clone for the read loop
        let aggregator = Arc::clone(&self.aggregator);
        let cascade_tx = self.cascade_tx.clone();
        let event_tx = self.event_tx.clone();

        // Spawn ping keepalive
        let write = Arc::new(tokio::sync::Mutex::new(write));
        let write_clone = Arc::clone(&write);

        tokio::spawn(async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(20)).await;
                let ping_msg = serde_json::json!({"op": "ping"}).to_string();
                let mut w = write_clone.lock().await;
                if w.send(Message::Text(ping_msg)).await.is_err() {
                    debug!("Bybit ping failed, connection may be closed");
                    break;
                }
                debug!("Sent Bybit ping");
            }
        });

        // Read loop
        while let Some(msg_result) = read.next().await {
            match msg_result {
                Ok(Message::Text(text)) => {
                    // Skip pong responses
                    if text.contains("\"op\":\"pong\"") {
                        debug!("Received Bybit pong");
                        continue;
                    }

                    // Skip subscription confirmations
                    if text.contains("\"op\":\"subscribe\"") {
                        debug!("Bybit subscription confirmed");
                        continue;
                    }

                    // Parse liquidation message
                    match serde_json::from_str::<BybitWsResponse>(&text) {
                        Ok(response) => {
                            if response.topic.starts_with("allLiquidation.") {
                                let data = &response.data;

                                // Parse values
                                let size: Decimal = data.size.parse().unwrap_or_default();
                                let price: Decimal = data.price.parse().unwrap_or_default();
                                let value_usd = size * price;

                                let side = if data.side == "Buy" {
                                    LiquidationSide::Buy
                                } else {
                                    LiquidationSide::Sell
                                };

                                let event = LiquidationEvent {
                                    symbol: data.symbol.clone(),
                                    side,
                                    size,
                                    price,
                                    value_usd,
                                    timestamp: chrono::Utc::now(),
                                };

                                // Log significant liquidations (>$100k)
                                if value_usd >= Decimal::new(100_000, 0) {
                                    info!(
                                        "🔥 LIQUIDATION: {} {} ${:.0} @ {} ({})",
                                        data.symbol,
                                        side,
                                        value_usd,
                                        price,
                                        if side == LiquidationSide::Buy {
                                            "longs rekt"
                                        } else {
                                            "shorts rekt"
                                        }
                                    );
                                }

                                // Send raw event
                                let _ = event_tx.send(event.clone());

                                // Check for cascade
                                let mut agg = aggregator.write().await;
                                if let Some(cascade) = agg.add_event(&event) {
                                    warn!(
                                        "🚨 LIQUIDATION CASCADE: {} {} ${:.0} ({} events in {}s) → {:?}",
                                        cascade.symbol,
                                        cascade.direction,
                                        cascade.total_value_usd,
                                        cascade.event_count,
                                        cascade.window_seconds,
                                        cascade.suggested_action
                                    );

                                    let _ = cascade_tx.send(cascade);
                                }
                            }
                        }
                        Err(e) => {
                            // Only log if it looks like a data message (not system)
                            if text.contains("\"data\"") {
                                debug!("Failed to parse Bybit message: {} | {}", e, &text[..200.min(text.len())]);
                            }
                        }
                    }
                }
                Ok(Message::Ping(data)) => {
                    let mut w = write.lock().await;
                    let _ = w.send(Message::Pong(data)).await;
                }
                Ok(Message::Close(frame)) => {
                    info!("Bybit WebSocket closed: {:?}", frame);
                    break;
                }
                Err(e) => {
                    error!("Bybit WebSocket error: {}", e);
                    break;
                }
                _ => {}
            }
        }

        warn!("Bybit WebSocket read loop ended");
        Ok(())
    }
}

// =============================================================================
// Auto-Reconnecting Runner
// =============================================================================

/// Run Bybit liquidation WebSocket with auto-reconnect
pub async fn run_bybit_liquidation_ws(
    symbols: Vec<String>,
    window_seconds: u32,
    cascade_threshold_usd: Decimal,
    cascade_tx: broadcast::Sender<LiquidationCascade>,
    event_tx: broadcast::Sender<LiquidationEvent>,
) -> Result<()> {
    let mut reconnect_delay = std::time::Duration::from_secs(1);
    let max_delay = std::time::Duration::from_secs(60);

    loop {
        let ws = BybitLiquidationWs::new(
            symbols.clone(),
            window_seconds,
            cascade_threshold_usd,
        );

        // Forward events from internal channels to provided channels
        let mut internal_cascade_rx = ws.subscribe_cascades();
        let mut internal_event_rx = ws.subscribe_events();

        let cascade_tx_clone = cascade_tx.clone();
        let event_tx_clone = event_tx.clone();

        // Spawn forwarder
        let forwarder = tokio::spawn(async move {
            loop {
                tokio::select! {
                    Ok(cascade) = internal_cascade_rx.recv() => {
                        let _ = cascade_tx_clone.send(cascade);
                    }
                    Ok(event) = internal_event_rx.recv() => {
                        let _ = event_tx_clone.send(event);
                    }
                    else => break,
                }
            }
        });

        match ws.connect().await {
            Ok(()) => {
                reconnect_delay = std::time::Duration::from_secs(1);
            }
            Err(e) => {
                error!("Bybit liquidation WebSocket error: {}", e);
            }
        }

        forwarder.abort();

        warn!("Reconnecting to Bybit in {:?}...", reconnect_delay);
        tokio::time::sleep(reconnect_delay).await;
        reconnect_delay = std::cmp::min(reconnect_delay * 2, max_delay);
    }
}

// =============================================================================
// Helper Functions
// =============================================================================

/// Map Bybit symbol to Polymarket asset
pub fn bybit_to_polymarket_asset(symbol: &str) -> Option<&'static str> {
    match symbol.to_uppercase().as_str() {
        "BTCUSDT" => Some("BTC"),
        "ETHUSDT" => Some("ETH"),
        "SOLUSDT" => Some("SOL"),
        _ => None,
    }
}

/// Get suggested threshold based on cascade intensity
/// Higher cascade = more confident = can use tighter threshold
pub fn get_dynamic_threshold(cascade: &LiquidationCascade) -> Decimal {
    let base_threshold = Decimal::new(98, 2); // 0.98

    // If cascade is very large (>$5M), use aggressive threshold
    if cascade.total_value_usd >= Decimal::new(5_000_000, 0) {
        return Decimal::new(99, 2); // 0.99 (very tight, only need 1% edge)
    }

    // If cascade is large (>$2M), use moderate threshold
    if cascade.total_value_usd >= Decimal::new(2_000_000, 0) {
        return Decimal::new(985, 3); // 0.985
    }

    // Standard cascade (>$1M)
    base_threshold
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_aggregator_cascade_detection() {
        let mut agg = LiquidationAggregator::new(10, Decimal::new(1_000_000, 0));

        // Add events that should trigger cascade
        for i in 0..5 {
            let event = LiquidationEvent {
                symbol: "BTCUSDT".to_string(),
                side: LiquidationSide::Buy,
                size: Decimal::new(5, 0),
                price: Decimal::new(50_000, 0),
                value_usd: Decimal::new(250_000, 0), // $250k each
                timestamp: chrono::Utc::now(),
            };

            let cascade = agg.add_event(&event);
            if i == 4 {
                // 5 * $250k = $1.25M > $1M threshold
                assert!(cascade.is_some());
                let c = cascade.unwrap();
                assert_eq!(c.direction, CascadeDirection::LongCascade);
                assert_eq!(c.suggested_action, CascadeAction::BuyDown);
            }
        }
    }
}

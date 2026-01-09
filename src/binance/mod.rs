//! Binance integration for real-time crypto price streaming
//!
//! Uses Binance public WebSocket API for low-latency price updates.
//! This is used for the hybrid latency arbitrage strategy.

use crate::config::AppConfig;
use anyhow::{Context, Result};
use futures::{SinkExt, StreamExt};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use tokio::sync::{broadcast, RwLock};
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tracing::{debug, error, info, warn};

/// Price tick from Binance
#[derive(Debug, Clone)]
pub struct BinanceTick {
    pub symbol: String,
    pub price: Decimal,
    pub timestamp: chrono::DateTime<chrono::Utc>,
}

/// Price move detection result
#[derive(Debug, Clone)]
pub struct PriceMove {
    pub symbol: String,
    pub direction: MoveDirection,
    pub change_pct: Decimal,
    pub from_price: Decimal,
    pub to_price: Decimal,
    pub window_sec: u32,
    pub timestamp: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MoveDirection {
    Up,
    Down,
}

impl std::fmt::Display for MoveDirection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MoveDirection::Up => write!(f, "UP"),
            MoveDirection::Down => write!(f, "DOWN"),
        }
    }
}

/// Binance WebSocket message (ticker)
#[derive(Debug, Deserialize)]
struct BinanceTickerMsg {
    #[serde(rename = "e")]
    event_type: String,
    #[serde(rename = "s")]
    symbol: String,
    #[serde(rename = "c")]
    close_price: String,
    #[serde(rename = "E")]
    event_time: u64,
}

/// Binance combined stream message
#[derive(Debug, Deserialize)]
struct BinanceCombinedMsg {
    stream: String,
    data: BinanceTickerMsg,
}

/// Subscribe message for combined streams
#[derive(Debug, Serialize)]
struct SubscribeMsg {
    method: String,
    params: Vec<String>,
    id: u64,
}

/// Price history for move detection
struct PriceHistory {
    prices: VecDeque<(chrono::DateTime<chrono::Utc>, Decimal)>,
    window_sec: u32,
}

impl PriceHistory {
    fn new(window_sec: u32) -> Self {
        Self {
            prices: VecDeque::new(),
            window_sec,
        }
    }

    fn add(&mut self, price: Decimal) {
        let now = chrono::Utc::now();

        // Remove old entries outside the window
        let cutoff = now - chrono::Duration::seconds(self.window_sec as i64);
        while let Some((ts, _)) = self.prices.front() {
            if *ts < cutoff {
                self.prices.pop_front();
            } else {
                break;
            }
        }

        self.prices.push_back((now, price));
    }

    fn calculate_change(&self) -> Option<(Decimal, Decimal, Decimal)> {
        if self.prices.len() < 2 {
            return None;
        }

        let (_, oldest_price) = self.prices.front()?;
        let (_, newest_price) = self.prices.back()?;

        if *oldest_price == Decimal::ZERO {
            return None;
        }

        let change = (*newest_price - *oldest_price) / *oldest_price * Decimal::new(100, 0);
        Some((*oldest_price, *newest_price, change))
    }
}

/// Binance WebSocket client for price streaming
pub struct BinanceWs {
    config: Arc<AppConfig>,
    prices: Arc<RwLock<HashMap<String, Decimal>>>,
    history: Arc<RwLock<HashMap<String, PriceHistory>>>,
    tick_tx: broadcast::Sender<BinanceTick>,
    move_tx: broadcast::Sender<PriceMove>,
}

impl BinanceWs {
    /// Create a new Binance WebSocket client
    pub fn new(config: Arc<AppConfig>) -> Self {
        let (tick_tx, _) = broadcast::channel(1000);
        let (move_tx, _) = broadcast::channel(100);

        Self {
            config,
            prices: Arc::new(RwLock::new(HashMap::new())),
            history: Arc::new(RwLock::new(HashMap::new())),
            tick_tx,
            move_tx,
        }
    }

    /// Subscribe to price ticks
    pub fn subscribe_ticks(&self) -> broadcast::Receiver<BinanceTick> {
        self.tick_tx.subscribe()
    }

    /// Subscribe to significant price moves
    pub fn subscribe_moves(&self) -> broadcast::Receiver<PriceMove> {
        self.move_tx.subscribe()
    }

    /// Get current price for a symbol
    pub async fn get_price(&self, symbol: &str) -> Option<Decimal> {
        self.prices.read().await.get(&symbol.to_lowercase()).copied()
    }

    /// Connect to Binance WebSocket
    pub async fn connect(&self) -> Result<()> {
        let symbols = &self.config.binance.symbols;
        let move_window = self.config.binance.move_window_sec;
        let trigger_pct = self.config.trading.spot_move_trigger_pct;

        // Build combined stream URL
        // Format: wss://stream.binance.com:9443/stream?streams=btcusdt@ticker/ethusdt@ticker
        let streams: Vec<String> = symbols
            .iter()
            .map(|s| format!("{}@ticker", s.to_lowercase()))
            .collect();

        let ws_url = format!(
            "{}/stream?streams={}",
            self.config.binance.ws_url.trim_end_matches('/'),
            streams.join("/")
        );

        info!("Connecting to Binance WebSocket: {}", ws_url);

        let (ws_stream, _) = connect_async(&ws_url)
            .await
            .context("Failed to connect to Binance WebSocket")?;

        let (mut _write, mut read) = ws_stream.split();

        // Initialize price history for each symbol
        {
            let mut history = self.history.write().await;
            for symbol in symbols {
                history.insert(symbol.to_lowercase(), PriceHistory::new(move_window));
            }
        }

        // Clone references for the read loop
        let prices = Arc::clone(&self.prices);
        let history = Arc::clone(&self.history);
        let tick_tx = self.tick_tx.clone();
        let move_tx = self.move_tx.clone();
        let window_sec = move_window;

        // Spawn read loop
        tokio::spawn(async move {
            while let Some(msg_result) = read.next().await {
                match msg_result {
                    Ok(Message::Text(text)) => {
                        if let Err(e) = Self::handle_message(
                            &text,
                            &prices,
                            &history,
                            &tick_tx,
                            &move_tx,
                            trigger_pct,
                            window_sec,
                        )
                        .await
                        {
                            debug!("Error handling Binance message: {}", e);
                        }
                    }
                    Ok(Message::Ping(data)) => {
                        debug!("Received Binance ping");
                        let _ = data;
                    }
                    Ok(Message::Close(frame)) => {
                        info!("Binance WebSocket closed: {:?}", frame);
                        break;
                    }
                    Err(e) => {
                        error!("Binance WebSocket error: {}", e);
                        break;
                    }
                    _ => {}
                }
            }
            warn!("Binance WebSocket read loop ended");
        });

        info!(
            "Binance WebSocket connected, streaming {} symbols",
            symbols.len()
        );
        Ok(())
    }

    /// Handle incoming WebSocket message
    async fn handle_message(
        text: &str,
        prices: &RwLock<HashMap<String, Decimal>>,
        history: &RwLock<HashMap<String, PriceHistory>>,
        tick_tx: &broadcast::Sender<BinanceTick>,
        move_tx: &broadcast::Sender<PriceMove>,
        trigger_pct: Decimal,
        window_sec: u32,
    ) -> Result<()> {
        let msg: BinanceCombinedMsg =
            serde_json::from_str(text).context("Failed to parse Binance message")?;

        if msg.data.event_type != "24hrTicker" {
            return Ok(());
        }

        let symbol = msg.data.symbol.to_lowercase();
        let price: Decimal = msg.data.close_price.parse().unwrap_or_default();
        let now = chrono::Utc::now();

        // Update current price
        prices.write().await.insert(symbol.clone(), price);

        // Broadcast tick
        let tick = BinanceTick {
            symbol: symbol.clone(),
            price,
            timestamp: now,
        };
        let _ = tick_tx.send(tick);

        // Update history and check for significant moves
        let mut hist_lock = history.write().await;
        if let Some(hist) = hist_lock.get_mut(&symbol) {
            hist.add(price);

            if let Some((from_price, to_price, change_pct)) = hist.calculate_change() {
                // Check if move is significant
                if change_pct.abs() >= trigger_pct {
                    let direction = if change_pct > Decimal::ZERO {
                        MoveDirection::Up
                    } else {
                        MoveDirection::Down
                    };

                    let price_move = PriceMove {
                        symbol: symbol.clone(),
                        direction,
                        change_pct,
                        from_price,
                        to_price,
                        window_sec,
                        timestamp: now,
                    };

                    info!(
                        "Significant move detected: {} {} {:.2}% ({} -> {})",
                        symbol.to_uppercase(),
                        direction,
                        change_pct,
                        from_price,
                        to_price
                    );

                    let _ = move_tx.send(price_move);
                }
            }
        }

        Ok(())
    }
}

/// Run Binance WebSocket with auto-reconnect
pub async fn run_binance_ws(
    config: Arc<AppConfig>,
    tick_tx: broadcast::Sender<BinanceTick>,
    move_tx: broadcast::Sender<PriceMove>,
) -> Result<()> {
    if !config.binance.enabled {
        info!("Binance integration disabled in config");
        return Ok(());
    }

    let reconnect_delay_ms = config.binance.reconnect_delay_ms;
    let max_attempts = config.binance.max_reconnect_attempts;
    let mut attempts = 0;

    loop {
        let ws = BinanceWs::new(Arc::clone(&config));

        match ws.connect().await {
            Ok(()) => {
                attempts = 0;

                // Forward events to the provided channels
                let mut ws_tick_rx = ws.subscribe_ticks();
                let mut ws_move_rx = ws.subscribe_moves();

                loop {
                    tokio::select! {
                        Ok(tick) = ws_tick_rx.recv() => {
                            let _ = tick_tx.send(tick);
                        }
                        Ok(mv) = ws_move_rx.recv() => {
                            let _ = move_tx.send(mv);
                        }
                        else => {
                            warn!("Binance WebSocket channels closed");
                            break;
                        }
                    }
                }
            }
            Err(e) => {
                error!("Binance WebSocket connection failed: {}", e);
                attempts += 1;

                if attempts >= max_attempts {
                    error!(
                        "Max reconnect attempts ({}) reached for Binance",
                        max_attempts
                    );
                    return Err(e);
                }
            }
        }

        let delay = std::time::Duration::from_millis(reconnect_delay_ms * (1 << attempts.min(5)));
        warn!("Reconnecting to Binance in {:?}...", delay);
        tokio::time::sleep(delay).await;
    }
}

/// Map Binance symbol to Polymarket asset type
pub fn symbol_to_asset(symbol: &str) -> Option<&'static str> {
    match symbol.to_lowercase().as_str() {
        "btcusdt" => Some("BTC"),
        "ethusdt" => Some("ETH"),
        "solusdt" => Some("SOL"),
        _ => None,
    }
}

/// Map Polymarket asset to Binance symbol
pub fn asset_to_symbol(asset: &str) -> Option<&'static str> {
    match asset.to_uppercase().as_str() {
        "BTC" => Some("btcusdt"),
        "ETH" => Some("ethusdt"),
        "SOL" => Some("solusdt"),
        _ => None,
    }
}

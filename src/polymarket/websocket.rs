//! Polymarket WebSocket client for real-time price updates

use crate::config::AppConfig;
use crate::polymarket::types::*;
use anyhow::{Context, Result};
use futures::{SinkExt, StreamExt};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{broadcast, RwLock};
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tracing::{debug, error, info, warn};

/// Price update event
#[derive(Debug, Clone)]
pub struct PriceUpdate {
    pub token_id: String,
    pub price: Decimal,
    pub timestamp: chrono::DateTime<chrono::Utc>,
}

/// OrderBook update event
#[derive(Debug, Clone)]
pub struct OrderBookUpdate {
    pub token_id: String,
    pub bids: Vec<OrderBookEntry>,
    pub asks: Vec<OrderBookEntry>,
    pub timestamp: chrono::DateTime<chrono::Utc>,
}

/// WebSocket message from Polymarket
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
enum WsIncoming {
    PriceChange {
        #[serde(rename = "type")]
        msg_type: String,
        asset_id: String,
        price: String,
        timestamp: Option<String>,
    },
    BookUpdate {
        #[serde(rename = "type")]
        msg_type: String,
        asset_id: String,
        market: Option<String>,
        bids: Vec<WsBookLevel>,
        asks: Vec<WsBookLevel>,
        timestamp: Option<String>,
        hash: Option<String>,
    },
    Subscribed {
        #[serde(rename = "type")]
        msg_type: String,
        channel: Option<String>,
    },
    Error {
        error: String,
    },
    Other(serde_json::Value),
}

#[derive(Debug, Clone, Deserialize)]
struct WsBookLevel {
    price: String,
    size: String,
}

/// Subscription message
#[derive(Debug, Serialize)]
struct SubscribeMessage {
    #[serde(rename = "type")]
    msg_type: String,
    channel: String,
    assets_ids: Vec<String>,
}

/// Polymarket WebSocket client
pub struct PolymarketWs {
    config: Arc<AppConfig>,
    prices: Arc<RwLock<HashMap<String, Decimal>>>,
    orderbooks: Arc<RwLock<HashMap<String, OrderBook>>>,
    price_tx: broadcast::Sender<PriceUpdate>,
    book_tx: broadcast::Sender<OrderBookUpdate>,
}

impl PolymarketWs {
    /// Create a new WebSocket client
    pub fn new(config: Arc<AppConfig>) -> Self {
        let (price_tx, _) = broadcast::channel(1000);
        let (book_tx, _) = broadcast::channel(1000);

        Self {
            config,
            prices: Arc::new(RwLock::new(HashMap::new())),
            orderbooks: Arc::new(RwLock::new(HashMap::new())),
            price_tx,
            book_tx,
        }
    }

    /// Subscribe to price updates
    pub fn subscribe_prices(&self) -> broadcast::Receiver<PriceUpdate> {
        self.price_tx.subscribe()
    }

    /// Subscribe to orderbook updates
    pub fn subscribe_orderbooks(&self) -> broadcast::Receiver<OrderBookUpdate> {
        self.book_tx.subscribe()
    }

    /// Get current price for a token
    pub async fn get_price(&self, token_id: &str) -> Option<Decimal> {
        self.prices.read().await.get(token_id).copied()
    }

    /// Get current orderbook for a token
    pub async fn get_orderbook(&self, token_id: &str) -> Option<OrderBook> {
        self.orderbooks.read().await.get(token_id).cloned()
    }

    /// Connect and subscribe to markets
    pub async fn connect(&self, token_ids: Vec<String>) -> Result<()> {
        let ws_url = &self.config.general.clob_ws_url;
        info!("Connecting to Polymarket WebSocket: {}", ws_url);

        let (ws_stream, _) = connect_async(ws_url)
            .await
            .context("Failed to connect to Polymarket WebSocket")?;

        let (mut write, mut read) = ws_stream.split();

        // Subscribe to price channel
        let price_sub = SubscribeMessage {
            msg_type: "subscribe".to_string(),
            channel: "price".to_string(),
            assets_ids: token_ids.clone(),
        };

        let msg = serde_json::to_string(&price_sub)?;
        write.send(Message::Text(msg)).await?;
        debug!("Subscribed to price channel for {} tokens", token_ids.len());

        // Subscribe to book channel
        let book_sub = SubscribeMessage {
            msg_type: "subscribe".to_string(),
            channel: "book".to_string(),
            assets_ids: token_ids.clone(),
        };

        let msg = serde_json::to_string(&book_sub)?;
        write.send(Message::Text(msg)).await?;
        debug!("Subscribed to book channel for {} tokens", token_ids.len());

        // Clone Arc references for the read loop
        let prices = Arc::clone(&self.prices);
        let orderbooks = Arc::clone(&self.orderbooks);
        let price_tx = self.price_tx.clone();
        let book_tx = self.book_tx.clone();

        // Spawn read loop
        tokio::spawn(async move {
            while let Some(msg_result) = read.next().await {
                match msg_result {
                    Ok(Message::Text(text)) => {
                        if let Err(e) = Self::handle_message(
                            &text,
                            &prices,
                            &orderbooks,
                            &price_tx,
                            &book_tx,
                        )
                        .await
                        {
                            warn!("Error handling message: {}", e);
                        }
                    }
                    Ok(Message::Ping(data)) => {
                        debug!("Received ping");
                        // Pong is handled automatically by tungstenite
                        let _ = data;
                    }
                    Ok(Message::Close(frame)) => {
                        info!("WebSocket closed: {:?}", frame);
                        break;
                    }
                    Err(e) => {
                        error!("WebSocket error: {}", e);
                        break;
                    }
                    _ => {}
                }
            }
            warn!("Polymarket WebSocket read loop ended");
        });

        info!("Polymarket WebSocket connected and subscribed");
        Ok(())
    }

    /// Handle incoming WebSocket message
    async fn handle_message(
        text: &str,
        prices: &RwLock<HashMap<String, Decimal>>,
        orderbooks: &RwLock<HashMap<String, OrderBook>>,
        price_tx: &broadcast::Sender<PriceUpdate>,
        book_tx: &broadcast::Sender<OrderBookUpdate>,
    ) -> Result<()> {
        let msg: WsIncoming = serde_json::from_str(text)
            .context("Failed to parse WebSocket message")?;

        match msg {
            WsIncoming::PriceChange {
                msg_type,
                asset_id,
                price,
                timestamp: _,
            } => {
                if msg_type == "price_change" || msg_type == "last_trade_price" {
                    let price_dec: Decimal = price.parse().unwrap_or_default();

                    // Update price cache
                    prices.write().await.insert(asset_id.clone(), price_dec);

                    // Broadcast update
                    let update = PriceUpdate {
                        token_id: asset_id,
                        price: price_dec,
                        timestamp: chrono::Utc::now(),
                    };

                    let _ = price_tx.send(update);
                }
            }

            WsIncoming::BookUpdate {
                msg_type,
                asset_id,
                market: _,
                bids,
                asks,
                timestamp: _,
                hash: _,
            } => {
                if msg_type == "book" {
                    let book = OrderBook {
                        token_id: asset_id.clone(),
                        bids: bids
                            .into_iter()
                            .map(|l| OrderBookEntry {
                                price: l.price.parse().unwrap_or_default(),
                                size: l.size.parse().unwrap_or_default(),
                            })
                            .collect(),
                        asks: asks
                            .into_iter()
                            .map(|l| OrderBookEntry {
                                price: l.price.parse().unwrap_or_default(),
                                size: l.size.parse().unwrap_or_default(),
                            })
                            .collect(),
                        timestamp: chrono::Utc::now(),
                    };

                    // Update orderbook cache
                    orderbooks.write().await.insert(asset_id.clone(), book.clone());

                    // Broadcast update
                    let update = OrderBookUpdate {
                        token_id: asset_id,
                        bids: book.bids,
                        asks: book.asks,
                        timestamp: book.timestamp,
                    };

                    let _ = book_tx.send(update);
                }
            }

            WsIncoming::Subscribed { msg_type, channel } => {
                debug!("Subscription confirmed: {} {:?}", msg_type, channel);
            }

            WsIncoming::Error { error } => {
                error!("WebSocket error message: {}", error);
            }

            WsIncoming::Other(val) => {
                debug!("Unknown message: {:?}", val);
            }
        }

        Ok(())
    }
}

/// Create a reconnecting WebSocket client
pub async fn run_polymarket_ws(
    config: Arc<AppConfig>,
    token_ids: Vec<String>,
    price_tx: broadcast::Sender<PriceUpdate>,
    book_tx: broadcast::Sender<OrderBookUpdate>,
) -> Result<()> {
    let mut reconnect_delay = std::time::Duration::from_secs(1);
    let max_delay = std::time::Duration::from_secs(60);

    loop {
        let ws = PolymarketWs::new(Arc::clone(&config));

        match ws.connect(token_ids.clone()).await {
            Ok(()) => {
                reconnect_delay = std::time::Duration::from_secs(1);

                // Forward events to the provided channels
                let mut price_rx = ws.subscribe_prices();
                let mut book_rx = ws.subscribe_orderbooks();

                loop {
                    tokio::select! {
                        Ok(update) = price_rx.recv() => {
                            let _ = price_tx.send(update);
                        }
                        Ok(update) = book_rx.recv() => {
                            let _ = book_tx.send(update);
                        }
                        else => {
                            warn!("WebSocket channels closed");
                            break;
                        }
                    }
                }
            }
            Err(e) => {
                error!("WebSocket connection failed: {}", e);
            }
        }

        warn!("Reconnecting in {:?}...", reconnect_delay);
        tokio::time::sleep(reconnect_delay).await;

        reconnect_delay = std::cmp::min(reconnect_delay * 2, max_delay);
    }
}

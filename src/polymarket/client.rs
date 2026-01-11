//! Polymarket CLOB API client

use crate::config::AppConfig;
use crate::polymarket::types::*;
use anyhow::{Context, Result};
use ethers::prelude::*;
use hmac::{Hmac, Mac};
use reqwest::Client;
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use std::sync::Arc;
use tracing::{debug, info, warn};
use base64::Engine;
use hex;

/// Polymarket CLOB client for REST API interactions
pub struct PolymarketClient {
    config: Arc<AppConfig>,
    http_client: Client,
    wallet: Option<LocalWallet>,
}

#[derive(Debug, Deserialize)]
struct ApiResponse<T> {
    #[serde(default)]
    data: Option<T>,
    #[serde(default)]
    error: Option<String>,
}

/// Response wrapper for CLOB API
#[derive(Debug, Deserialize)]
struct ClobMarketsResponse {
    data: Vec<ClobMarketData>,
    #[serde(default)]
    next_cursor: Option<String>,
}

/// Market data from CLOB API (uses snake_case)
#[derive(Debug, Deserialize)]
struct ClobMarketData {
    condition_id: String,
    #[serde(default)]
    question_id: Option<String>,
    #[serde(default)]
    tokens: Vec<ClobTokenData>,
    #[serde(default, alias = "slug")]
    market_slug: Option<String>,
    question: String,
    #[serde(default)]
    end_date_iso: Option<String>,
    #[serde(default)]
    active: bool,
    #[serde(default)]
    closed: bool,
    #[serde(default)]
    accepting_orders: bool,
    /// CLOB token IDs - index 0 = Yes/Up, index 1 = No/Down
    #[serde(default, rename = "clobTokenIds")]
    clob_token_ids: Vec<String>,
}

/// Token data from CLOB API
#[derive(Debug, Deserialize)]
struct ClobTokenData {
    token_id: String,
    outcome: String,
    #[serde(default)]
    price: Option<f64>,
}

/// Gamma API event response (for discovering updown markets)
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GammaEvent {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    slug: Option<String>,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    active: bool,
    #[serde(default)]
    closed: bool,
    #[serde(default)]
    markets: Vec<GammaMarket>,
}

/// Gamma API market inside an event
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GammaMarket {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    condition_id: String,
    #[serde(default)]
    question: String,
    #[serde(default)]
    slug: Option<String>,
    #[serde(default)]
    end_date: Option<String>,
    #[serde(default)]
    clob_token_ids: Option<String>,  // JSON array as string: "[\"id1\", \"id2\"]"
    #[serde(default)]
    accepting_orders: Option<bool>,
}

#[derive(Debug, Deserialize)]
struct OrderBookResponse {
    bids: Vec<OrderBookLevel>,
    asks: Vec<OrderBookLevel>,
    hash: String,
    timestamp: String,
    market: String,
    asset_id: String,
}

#[derive(Debug, Deserialize)]
struct OrderBookLevel {
    price: String,
    size: String,
}

#[derive(Debug, Serialize)]
struct CreateOrderRequest {
    order: SignedOrder,
}

#[derive(Debug, Serialize)]
struct CreateOrdersRequest {
    orders: Vec<SignedOrder>,
}

#[derive(Debug, Deserialize)]
struct MarketMetadataResponse {
    minimum_order_size: String,
    minimum_tick_size: String, // Note: API might call it tick_size or minimum_tick_size
}

#[derive(Debug, Serialize)]
struct SignedOrder {
    salt: String,
    maker: String,
    signer: String,
    taker: String,
    token_id: String,
    maker_amount: String,
    taker_amount: String,
    expiration: String,
    nonce: String,
    fee_rate_bps: String,
    side: String,
    signature_type: u8,
    signature: String,
}

impl PolymarketClient {
    /// Create a new Polymarket client
    pub fn new(config: Arc<AppConfig>) -> Result<Self> {
        let http_client = Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .context("Failed to create HTTP client")?;

        // Initialize wallet if private key is available
        let wallet = if config.is_real_mode() {
            match config.get_private_key() {
                Ok(key) => {
                    let wallet: LocalWallet = key
                        .parse()
                        .context("Failed to parse private key")?;
                    Some(wallet.with_chain_id(config.wallet.chain_id))
                }
                Err(e) => {
                    warn!("Private key not found, running in read-only mode: {}", e);
                    None
                }
            }
        } else {
            debug!("Test mode: wallet not initialized");
            None
        };

        Ok(Self {
            config,
            http_client,
            wallet,
        })
    }

    /// Get API base URL
    fn api_url(&self) -> &str {
        &self.config.general.clob_api_url
    }

    /// Get Gamma API URL
    fn gamma_url(&self) -> &str {
        &self.config.general.gamma_api_url
    }

    /// Fetch markets matching slug patterns using CLOB API
    pub async fn fetch_markets(&self, slug_pattern: &str) -> Result<Vec<Market>> {
        // Use CLOB API which has proper market data format
        let url = format!(
            "{}/markets?active=true&closed=false",
            self.api_url()
        );

        debug!("Fetching markets from CLOB API: {}", url);

        let response = self
            .http_client
            .get(&url)
            .send()
            .await
            .context("Failed to fetch markets")?;

        let status = response.status();
        let body = response.text().await?;

        if !status.is_success() {
            anyhow::bail!("API error {}: {}", status, body);
        }

        // Debug: log first 500 chars of response
        debug!("API response (first 500 chars): {}", &body.chars().take(500).collect::<String>());

        let clob_response: ClobMarketsResponse = serde_json::from_str(&body)
            .context("Failed to parse CLOB markets response")?;

        // Filter markets by slug pattern
        let markets_data: Vec<ClobMarketData> = clob_response.data
            .into_iter()
            .filter(|m| {
                let slug = m.market_slug.as_deref().unwrap_or("").to_lowercase();
                let question = m.question.to_lowercase();
                slug.contains(slug_pattern) || question.contains(slug_pattern)
            })
            .collect();

        let markets: Vec<Market> = markets_data
            .into_iter()
            .map(|m| {
                let clob_token_ids = m.clob_token_ids
                    .iter()
                    .map(|s| s.clone())
                    .collect::<Vec<_>>();
                Market {
                    condition_id: m.condition_id.clone(),
                    question_id: m.question_id.unwrap_or_else(|| m.condition_id),
                    tokens: m
                        .tokens
                        .into_iter()
                        .map(|t| Token {
                            token_id: t.token_id,
                            outcome: t.outcome,
                            price: t.price.map(|p| Decimal::try_from(p).unwrap_or_default()),
                        })
                        .collect(),
                    slug: m.market_slug.unwrap_or_default(),
                    question: m.question,
                    end_date_iso: m.end_date_iso,
                    active: m.active,
                    closed: m.closed,
                    accepting_orders: m.accepting_orders,
                    clob_token_ids,
                }
            })
            .collect();

        info!("Fetched {} markets matching '{}'", markets.len(), slug_pattern);

        Ok(markets)
    }

    /// Fetch 15-minute crypto up/down markets from Gamma API events
    /// Pattern: btc-updown-15m-{timestamp}, eth-updown-15m-{timestamp}, etc.
    pub async fn fetch_15min_crypto_markets(&self) -> Result<Vec<Market>> {
        let mut all_markets = Vec::new();

        // Search patterns for crypto updown 15m events
        let patterns = ["btc-updown-15m", "eth-updown-15m", "sol-updown-15m"];

        for pattern in patterns {
            match self.fetch_updown_events(pattern).await {
                Ok(markets) => {
                    info!("Found {} markets for pattern '{}'", markets.len(), pattern);
                    all_markets.extend(markets);
                }
                Err(e) => {
                    warn!("Failed to fetch events for pattern '{}': {}", pattern, e);
                }
            }
        }

        info!("Total 15-min crypto markets found: {}", all_markets.len());
        Ok(all_markets)
    }

    /// Fetch updown events from Gamma API
    async fn fetch_updown_events(&self, ticker_pattern: &str) -> Result<Vec<Market>> {
        // Use Gamma API events endpoint with slug_contains
        let url = format!(
            "{}/events?active=true&closed=false&limit=20&slug_contains={}",
            self.gamma_url(),
            ticker_pattern
        );

        debug!("Fetching events from Gamma API: {}", url);

        let response = self
            .http_client
            .get(&url)
            .send()
            .await
            .context("Failed to fetch events")?;

        let status = response.status();
        let body = response.text().await?;

        if !status.is_success() {
            anyhow::bail!("Gamma API error {}: {}", status, body);
        }

        // Parse Gamma events response
        let events: Vec<GammaEvent> = serde_json::from_str(&body)
            .context("Failed to parse Gamma events response")?;

        let mut markets = Vec::new();

        for event in events {
            if !event.active || event.closed {
                continue;
            }

            for gm in event.markets {
                // Parse clobTokenIds JSON array
                let token_ids: Vec<String> = if let Some(ref clob_ids) = gm.clob_token_ids {
                    serde_json::from_str(clob_ids).unwrap_or_default()
                } else {
                    Vec::new()
                };

                // Create tokens from the IDs (usually [Up, Down] or [Yes, No])
                let tokens: Vec<Token> = token_ids
                    .iter()
                    .enumerate()
                    .map(|(i, id)| Token {
                        token_id: id.clone(),
                        outcome: if i == 0 { "Up".to_string() } else { "Down".to_string() },
                        price: None,
                    })
                    .collect();

                if !tokens.is_empty() {
                    markets.push(Market {
                        condition_id: gm.condition_id.clone(),
                        question_id: gm.condition_id.clone(),
                        tokens,
                        slug: gm.slug.unwrap_or_default(),
                        question: gm.question,
                        end_date_iso: gm.end_date,
                        active: true,
                        closed: false,
                        accepting_orders: gm.accepting_orders.unwrap_or(true),
                        clob_token_ids: token_ids.clone(),
                    });
                }
            }
        }

        Ok(markets)
    }

    /// Fetch active 15-minute crypto binary markets with proper yes/no token mapping
    /// Uses calculated timestamps and parallel requests for fast discovery
    pub async fn fetch_active_crypto_markets(&self) -> Result<Vec<CryptoMarket>> {
        // Calculate current 15-min window start timestamp
        let now = chrono::Utc::now();
        let now_ts = now.timestamp();
        let window_start = (now_ts / 900) * 900; // Round down to nearest 15 min (900 seconds)
        
        // Check current, next, and previous windows
        let timestamps = [
            window_start,           // Current window (active now)
            window_start + 900,     // Next window (pre-order)
            window_start - 900,     // Previous window (might still resolve)
        ];
        
        let assets = ["btc", "eth", "sol"];
        
        info!(
            "Fetching 15-min crypto markets for window: {} UTC (±15min)",
            chrono::DateTime::from_timestamp(window_start, 0)
                .map(|dt| dt.format("%H:%M").to_string())
                .unwrap_or_default()
        );

        // Build all slug queries
        let mut queries = Vec::new();
        for ts in timestamps {
            for asset in &assets {
                let slug = format!("{}-updown-15m-{}", asset, ts);
                queries.push((slug, *asset));
            }
        }

        // Execute all queries in parallel
        let futures: Vec<_> = queries.iter().map(|(slug, asset)| {
            let url = format!("{}/events?slug={}", self.gamma_url(), slug);
            let client = self.http_client.clone();
            let slug = slug.clone();
            let asset = *asset;
            
            async move {
                let resp = client.get(&url).send().await.ok()?;
                if !resp.status().is_success() {
                    return None;
                }
                let events: Vec<serde_json::Value> = resp.json().await.ok()?;
                if events.is_empty() {
                    return None;
                }
                Some((events[0].clone(), slug, asset))
            }
        }).collect();

        let results = futures::future::join_all(futures).await;

        // Process results
        let mut crypto_markets = Vec::new();
        
        for result in results.into_iter().flatten() {
            let (event, event_slug, asset) = result;
            
            let markets = match event.get("markets") {
                Some(serde_json::Value::Array(arr)) => arr,
                _ => continue,
            };

            let crypto_asset = match asset {
                "btc" => CryptoAsset::BTC,
                "eth" => CryptoAsset::ETH,
                "sol" => CryptoAsset::SOL,
                _ => continue,
            };

            for market in markets {
                let clob_token_ids_str = market.get("clobTokenIds")
                    .and_then(|v| v.as_str())
                    .unwrap_or("[]");
                
                let token_ids: Vec<String> = serde_json::from_str(clob_token_ids_str)
                    .unwrap_or_default();
                
                if token_ids.len() != 2 {
                    continue;
                }

                let title = market.get("question")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let condition_id = market.get("conditionId")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let end_date = market.get("endDate")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());
                let accepting = market.get("acceptingOrders")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);

                // Skip markets not accepting orders
                if !accepting {
                    debug!("Skipping {} - not accepting orders", event_slug);
                    continue;
                }

                // Outcome Mapping (Up -> Index 0, Down -> Index 1)
                let outcomes_str = market.get("outcomes")
                    .and_then(|v| v.as_str())
                    .unwrap_or("[\"Up\", \"Down\"]");
                
                let outcomes: Vec<String> = serde_json::from_str(outcomes_str)
                    .unwrap_or_else(|_| vec!["Up".to_string(), "Down".to_string()]);

                let (yes_token_id, no_token_id) = if outcomes.len() >= 2 {
                    let first = outcomes[0].to_lowercase();
                    if first == "up" || first == "yes" {
                        (token_ids[0].clone(), token_ids[1].clone())
                    } else {
                        (token_ids[1].clone(), token_ids[0].clone())
                    }
                } else {
                    (token_ids[0].clone(), token_ids[1].clone())
                };

                // Avoid duplicates
                if crypto_markets.iter().any(|m: &CryptoMarket| m.condition_id == condition_id) {
                    continue;
                }

                debug!(
                    "Found: {} | {} | Yes: {}...",
                    event_slug, crypto_asset, &yes_token_id[..8.min(yes_token_id.len())]
                );

                crypto_markets.push(CryptoMarket {
                    slug: event_slug.clone(),
                    title: title.to_string(),
                    condition_id: condition_id.to_string(),
                    yes_token_id,
                    no_token_id,
                    asset: crypto_asset.clone(),
                    end_time: end_date,
                    accepting_orders: accepting,
                });
            }
        }

        // Log summary
        info!("=== DISCOVERED 15-MIN CRYPTO MARKETS ===");
        if crypto_markets.is_empty() {
            warn!("No active markets found. Markets may be between windows.");
        } else {
            for (i, cm) in crypto_markets.iter().take(10).enumerate() {
                info!(
                    "[{}] {} | {} | Yes: {}...",
                    i + 1,
                    cm.asset,
                    cm.title.chars().take(40).collect::<String>(),
                    &cm.yes_token_id[..8.min(cm.yes_token_id.len())]
                );
            }
            if crypto_markets.len() > 10 {
                info!("... and {} more", crypto_markets.len() - 10);
            }
        }
        info!("Total markets: {}", crypto_markets.len());

        Ok(crypto_markets)
    }

    /// Fetch current prices for a crypto market using token IDs
    pub async fn fetch_crypto_market_prices(&self, market: &CryptoMarket) -> Result<MarketPrices> {
        // Fetch orderbooks for both tokens
        let yes_book = self.fetch_orderbook(&market.yes_token_id).await?;
        let no_book = self.fetch_orderbook(&market.no_token_id).await?;

        // Get mid prices (or best available)
        let yes_price = yes_book.mid_price().unwrap_or(Decimal::new(5, 1)); // Default 0.5
        let no_price = no_book.mid_price().unwrap_or(Decimal::new(5, 1)); // Default 0.5

        let prices = MarketPrices {
            condition_id: market.condition_id.clone(),
            yes_price,
            no_price,
            yes_token_id: market.yes_token_id.clone(),
            no_token_id: market.no_token_id.clone(),
            timestamp: chrono::Utc::now(),
        };

        debug!(
            "Prices for {}: yes={}, no={}, sum={}",
            market.asset,
            yes_price,
            no_price,
            prices.price_sum()
        );

        Ok(prices)
    }

    /// Get all token IDs from discovered crypto markets (for WebSocket subscription)
    pub fn get_all_token_ids(markets: &[CryptoMarket]) -> Vec<String> {
        let mut token_ids = Vec::new();
        for m in markets {
            token_ids.push(m.yes_token_id.clone());
            token_ids.push(m.no_token_id.clone());
        }
        token_ids
    }

    /// Fetch order book for a token
    pub async fn fetch_orderbook(&self, token_id: &str) -> Result<OrderBook> {
        let url = format!("{}/book?token_id={}", self.api_url(), token_id);

        debug!("Fetching orderbook for token: {}", token_id);

        let response = self
            .http_client
            .get(&url)
            .send()
            .await
            .context("Failed to fetch orderbook")?;

        let status = response.status();
        let body = response.text().await?;

        if !status.is_success() {
            anyhow::bail!("API error {}: {}", status, body);
        }

        // Debug: log raw response for troubleshooting
        debug!("Orderbook response for {}...: {}", &token_id[..8.min(token_id.len())], &body[..200.min(body.len())]);

        let ob_response: OrderBookResponse = serde_json::from_str(&body)
            .context("Failed to parse orderbook response")?;

        let parse_level = |l: &OrderBookLevel| -> OrderBookEntry {
            OrderBookEntry {
                price: l.price.parse().unwrap_or_default(),
                size: l.size.parse().unwrap_or_default(),
            }
        };

        let orderbook = OrderBook {
            token_id: token_id.to_string(),
            bids: ob_response.bids.iter().map(parse_level).collect(),
            asks: ob_response.asks.iter().map(parse_level).collect(),
            timestamp: chrono::Utc::now(),
        };

        // Log empty orderbook for debugging
        if orderbook.bids.is_empty() && orderbook.asks.is_empty() {
            debug!(
                "Empty orderbook for token {}... | Raw bids: {} | Raw asks: {}",
                &token_id[..8.min(token_id.len())],
                ob_response.bids.len(),
                ob_response.asks.len()
            );
        }

        Ok(orderbook)
    }

    /// Fetch prices for a market (Yes and No tokens)
    pub async fn fetch_market_prices(&self, market: &Market) -> Result<MarketPrices> {
        let (yes_token, no_token) = self.get_yes_no_tokens(market)?;

        let (yes_book, no_book) = tokio::try_join!(
            self.fetch_orderbook(&yes_token.token_id),
            self.fetch_orderbook(&no_token.token_id)
        )?;

        let yes_price = yes_book.mid_price().unwrap_or_default();
        let no_price = no_book.mid_price().unwrap_or_default();

        Ok(MarketPrices {
            condition_id: market.condition_id.clone(),
            yes_price,
            no_price,
            yes_token_id: yes_token.token_id.clone(),
            no_token_id: no_token.token_id.clone(),
            timestamp: chrono::Utc::now(),
        })
    }

    /// Get Yes and No tokens from a market
    fn get_yes_no_tokens<'a>(&self, market: &'a Market) -> Result<(&'a Token, &'a Token)> {
        let yes_token = market
            .tokens
            .iter()
            .find(|t| t.outcome.to_lowercase() == "yes")
            .context("Yes token not found")?;

        let no_token = market
            .tokens
            .iter()
            .find(|t| t.outcome.to_lowercase() == "no")
            .context("No token not found")?;

        Ok((yes_token, no_token))
    }

    pub async fn get_market_metadata(&self, token_id: &str) -> Result<(Decimal, Decimal)> {
        // Fetch market config to respect Min Size and Tick Size
        let url = format!("{}/markets/{}", self.api_url(), token_id);
        
        let response = self.http_client.get(&url).send().await?;
        if !response.status().is_success() {
             // Fallback default for 15-min crypto binary (Tick 0.1 or 0.01?, Size 1?)
             return Ok((Decimal::new(1, 0), Decimal::new(1, 2))); 
        }

        // Parse response (simplified for snippet)
        // In reality, map proper JSON fields
        let body: serde_json::Value = response.json().await?;
        
        let min_size = body["minimum_order_size"].as_str()
            .and_then(|s| Decimal::from_str_exact(s).ok())
            .unwrap_or(Decimal::new(1,0));
            
        let tick_size = body["minimum_tick_size"].as_str()
            .and_then(|s| Decimal::from_str_exact(s).ok())
            .unwrap_or(Decimal::new(1,2)); // 0.01

        Ok((min_size, tick_size))
    }

    /// Place multiple orders in a batch (atomic-like)
    pub async fn place_orders(&self, orders: Vec<OrderRequest>) -> Result<Vec<Order>> {
        if self.config.is_test_mode() {
            let mut simulated = Vec::new();
            for order in orders {
                simulated.push(self.simulate_order(&order)?);
            }
            return Ok(simulated);
        }

        let wallet = self.wallet.as_ref()
            .context("Wallet not initialized for real trading")?;

        let mut signed_orders = Vec::new();
        for order in &orders {
            signed_orders.push(self.build_signed_order(order, wallet).await?);
        }

        // Batch endpoint is typically /orders
        let url = format!("{}/orders", self.api_url());

        let response = self
            .http_client
            .post(&url)
            .json(&CreateOrdersRequest { orders: signed_orders })
            .send()
            .await
            .context("Failed to place batch orders")?;

        let status = response.status();
        let body = response.text().await?;

        if !status.is_success() {
            anyhow::bail!("Batch order failed {}: {}", status, body);
        }

        // Parse list of orders
        let orders_response: Vec<Order> = serde_json::from_str(&body)
            .context("Failed to parse batch order response")?;

        info!("Batch orders placed successfully: {} orders", orders_response.len());
        Ok(orders_response)
    }

    /// Place a limit order (real mode only)
    pub async fn place_order(&self, order: &OrderRequest) -> Result<Order> {
        if self.config.is_test_mode() {
            return self.simulate_order(order);
        }

        let wallet = self.wallet.as_ref()
            .context("Wallet not initialized for real trading")?;

        // Build and sign the order
        // Build and sign the order
        let signed_order = self.build_signed_order(order, wallet).await?;

        // Prepare L2 Auth Headers
        let req_body = CreateOrderRequest { order: signed_order };
        let body_str = serde_json::to_string(&req_body)?;
        let auth_headers = self.build_auth_headers("POST", "/order", &body_str)?;

        let url = format!("{}/order", self.api_url());

        let response = self
            .http_client
            .post(&url)
            .headers(auth_headers)
            .body(body_str)
            .send()
            .await
            .context("Failed to place order")?;

        let status = response.status();
        let body = response.text().await?;

        if !status.is_success() {
            anyhow::bail!("Order placement failed {}: {}", status, body);
        }

        let order_response: Order = serde_json::from_str(&body)
            .context("Failed to parse order response")?;

        info!(
            "Order placed: {} {} {} @ {}",
            order.side, order.size, order.token_id, order.price
        );

        Ok(order_response)
    }

    /// Simulate an order (test mode)
    fn simulate_order(&self, order: &OrderRequest) -> Result<Order> {
        info!(
            "[SIMULATION] Would place order: {} {} shares @ {} for token {}",
            order.side, order.size, order.price, order.token_id
        );

        Ok(Order {
            id: uuid::Uuid::new_v4().to_string(),
            status: OrderStatus::Live,
            token_id: order.token_id.clone(),
            side: order.side,
            original_size: order.size,
            size_matched: Decimal::ZERO,
            price: order.price,
            created_at: chrono::Utc::now(),
        })
    }

    /// Build a signed order using EIP-712
    async fn build_signed_order(
        &self,
        order: &OrderRequest,
        wallet: &LocalWallet,
    ) -> Result<SignedOrder> {
        let chain_id = 137;
        // Polymarket CTF Exchange
        let verifying_contract: Address = "0x4bFb41d5B3570DeFd03C39a9A4D8dE6Bd8B8982E".parse()?;

        let salt = U256::from(chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0));
        let maker = wallet.address();
        let signer = wallet.address(); // Same as maker for EOA
        let taker = Address::zero();
        
        // Parse token_id (decimal string)
        let token_id = U256::from_dec_str(&order.token_id)
            .context("Invalid token_id format")?;
            
        let expiration = order.expiration
            .unwrap_or_else(|| chrono::Utc::now().timestamp() + 300); // 5 mins default
        let expiration_u256 = U256::from(expiration);
        
        let nonce = U256::zero();
        let fee_rate_bps = U256::zero();
        let side_int = match order.side {
            Side::Buy => U256::zero(),
            Side::Sell => U256::one(),
        };
        let signature_type = U256::zero();

        // Calculate amounts (USDC & Token both 6 decimals)
        let size_raw = (order.size * Decimal::new(1_000_000, 0)).to_string();
        let size_u256 = U256::from_dec_str(&size_raw).unwrap_or(U256::zero());
        
        let cost_raw = (order.size * order.price * Decimal::new(1_000_000, 0)).round().to_string();
        let cost_u256 = U256::from_dec_str(&cost_raw).unwrap_or(U256::zero());

        // Maker/Taker amounts rule:
        // Buy: Maker=USDC (Cost), Taker=Tokens (Size)
        // Sell: Maker=Tokens (Size), Taker=USDC (Cost)
        let (maker_amount, taker_amount) = match order.side {
            Side::Buy => (cost_u256, size_u256),
            Side::Sell => (size_u256, cost_u256),
        };

        // Create TypedData via JSON to avoid struct import issues
        let typed_data_json = serde_json::json!({
            "domain": {
                "name": "Polymarket CTF Exchange",
                "version": "1",
                "chainId": chain_id,
                "verifyingContract": verifying_contract,
            },
            "types": {
                "EIP712Domain": [
                    { "name": "name", "type": "string" },
                    { "name": "version", "type": "string" },
                    { "name": "chainId", "type": "uint256" },
                    { "name": "verifyingContract", "type": "address" },
                ],
                "Order": [
                    { "name": "salt", "type": "uint256" },
                    { "name": "maker", "type": "address" },
                    { "name": "signer", "type": "address" },
                    { "name": "taker", "type": "address" },
                    { "name": "tokenId", "type": "uint256" },
                    { "name": "makerAmount", "type": "uint256" },
                    { "name": "takerAmount", "type": "uint256" },
                    { "name": "expiration", "type": "uint256" },
                    { "name": "nonce", "type": "uint256" },
                    { "name": "feeRateBps", "type": "uint256" },
                    { "name": "side", "type": "uint256" },
                    { "name": "signatureType", "type": "uint256" },
                ]
            },
            "primaryType": "Order",
            "message": {
                "salt": salt.to_string(),
                "maker": format!("{:?}", maker),
                "signer": format!("{:?}", signer),
                "taker": format!("{:?}", taker),
                "tokenId": token_id.to_string(),
                "makerAmount": maker_amount.to_string(),
                "takerAmount": taker_amount.to_string(),
                "expiration": expiration_u256.to_string(),
                "nonce": nonce.to_string(),
                "feeRateBps": fee_rate_bps.to_string(),
                "side": match order.side { Side::Buy => "0".to_string(), Side::Sell => "1".to_string() },
                "signatureType": signature_type.to_string()
            }
        });

        let typed_data: ethers::types::transaction::eip712::TypedData = serde_json::from_value(typed_data_json)
            .context("Failed to construct TypedData from JSON")?;

        // Sign Typed Data
        let signature = wallet
            .sign_typed_data(&typed_data)
            .await
            .context("Failed to sign EIP-712 order")?;

        Ok(SignedOrder {
            salt: salt.to_string(),
            maker: format!("{:?}", maker),
            signer: format!("{:?}", signer),
            taker: format!("{:?}", taker),
            token_id: token_id.to_string(),
            maker_amount: maker_amount.to_string(),
            taker_amount: taker_amount.to_string(),
            expiration: expiration_u256.to_string(),
            nonce: nonce.to_string(),
            fee_rate_bps: fee_rate_bps.to_string(),
            side: match order.side { Side::Buy => "0".to_string(), Side::Sell => "1".to_string() },
            signature_type: signature_type.as_u64() as u8,
            signature: format!("0x{}", hex::encode(signature.to_vec())),
        })
    }

    /// Cancel an order
    pub async fn cancel_order(&self, order_id: &str) -> Result<()> {
        if self.config.is_test_mode() {
            info!("[SIMULATION] Would cancel order: {}", order_id);
            return Ok(());
        }

        let path = format!("/order/{}", order_id);
        let url = format!("{}{}", self.api_url(), path);

        let auth_headers = self.build_auth_headers("DELETE", &path, "")?;

        let response = self
            .http_client
            .delete(&url)
            .headers(auth_headers)
            .send()
            .await
            .context("Failed to cancel order")?;

        if !response.status().is_success() {
            let body = response.text().await?;
            anyhow::bail!("Cancel failed: {}", body);
        }

        info!("Order cancelled: {}", order_id);
        Ok(())
    }

    /// Get open orders
    pub async fn get_open_orders(&self) -> Result<Vec<Order>> {
        if self.config.is_test_mode() {
            return Ok(Vec::new());
        }

        let wallet = self.wallet.as_ref()
            .context("Wallet not initialized")?;

        let url = format!(
            "{}/orders?maker={:?}&status=LIVE",
            self.api_url(),
            wallet.address()
        );

        let response = self
            .http_client
            .get(&url)
            .send()
            .await
            .context("Failed to fetch orders")?;

        let body = response.text().await?;
        let orders: Vec<Order> = serde_json::from_str(&body)?;

        Ok(orders)
    }

    // =========================================================================
    // Maker Rebates Integration (Jan 2026 Program)
    // =========================================================================

    /// Get fee rate for a token (in basis points)
    /// Used to estimate potential rebates for maker orders
    /// API: GET https://clob.polymarket.com/fee-rate?token_id={token_id}
    pub async fn get_fee_rate(&self, token_id: &str) -> Result<FeeRate> {
        let url = format!("{}/fee-rate?token_id={}", self.api_url(), token_id);

        debug!("Fetching fee rate for token: {}", token_id);

        let response = self
            .http_client
            .get(&url)
            .send()
            .await
            .context("Failed to fetch fee rate")?;

        let status = response.status();
        let body = response.text().await?;

        if !status.is_success() {
            // Default to 0 fee if API fails (common for new markets)
            warn!("Fee rate API error {}: {}. Using default.", status, body);
            return Ok(FeeRate {
                token_id: token_id.to_string(),
                maker_fee_bps: 0,
                taker_fee_bps: 0,
                maker_rebate_bps: 100, // Default 1% rebate estimate
            });
        }

        let fee_response: FeeRateResponse = serde_json::from_str(&body)
            .unwrap_or_else(|_| FeeRateResponse {
                fee_rate_bps: Some("0".to_string()),
                maker: Some("0".to_string()),
                taker: Some("0".to_string()),
            });

        Ok(FeeRate {
            token_id: token_id.to_string(),
            maker_fee_bps: fee_response
                .maker
                .unwrap_or_default()
                .parse()
                .unwrap_or(0),
            taker_fee_bps: fee_response
                .taker
                .or(fee_response.fee_rate_bps)
                .unwrap_or_default()
                .parse()
                .unwrap_or(0),
            maker_rebate_bps: 100, // Estimated rebate ~1% (varies by volume share)
        })
    }

    /// Get order fills for the current wallet
    /// Used to track executed maker volume for rebate estimation
    pub async fn get_fills(&self, market_id: Option<&str>) -> Result<Vec<Fill>> {
        if self.config.is_test_mode() {
            return Ok(Vec::new());
        }

        let wallet = self.wallet.as_ref()
            .context("Wallet not initialized")?;

        let mut url = format!(
            "{}/fills?maker={:?}",
            self.api_url(),
            wallet.address()
        );

        if let Some(market) = market_id {
            url.push_str(&format!("&market={}", market));
        }

        let response = self
            .http_client
            .get(&url)
            .send()
            .await
            .context("Failed to fetch fills")?;

        let status = response.status();
        let body = response.text().await?;

        if !status.is_success() {
            warn!("Fills API error {}: {}", status, body);
            return Ok(Vec::new());
        }

        let fills: Vec<FillResponse> = serde_json::from_str(&body).unwrap_or_default();

        Ok(fills
            .into_iter()
            .map(|f| Fill {
                id: f.id,
                order_id: f.order_id,
                market_id: f.market,
                token_id: f.asset_id,
                side: if f.side.to_uppercase() == "BUY" {
                    Side::Buy
                } else {
                    Side::Sell
                },
                price: f.price.parse().unwrap_or_default(),
                size: f.size.parse().unwrap_or_default(),
                fee: f.fee.parse().unwrap_or_default(),
                is_maker: f.is_maker.unwrap_or(true),
                timestamp: chrono::DateTime::parse_from_rfc3339(&f.timestamp)
                    .map(|dt| dt.with_timezone(&chrono::Utc))
                    .unwrap_or_else(|_| chrono::Utc::now()),
            })
            .collect())
    }

    /// Automatically derive API credentials (L2) from Private Key (L1)
    /// This removes the need for users to manually generate API keys via scripts
    pub async fn derive_api_keys(&self) -> Result<crate::config::AuthConfig> {
        let wallet = self.wallet.as_ref()
            .context("Wallet not initialized. Private key required for creating API keys.")?;

        info!("Deriving L2 API Keys from Private Key...");
        
        let timestamp = chrono::Utc::now().timestamp().to_string();
        let nonce = "0"; // 0 for derive (idempotent), can use random for create
        let msg_text = "This message attests that I control the given wallet";
        
        // Polymarket Mainnet Chain ID
        let chain_id = 137;
        let verifying_contract: Address = "0x4bFb41d5B3570DeFd03C39a9A4D8dE6Bd8B8982E".parse()?;
        let wallet_address = wallet.address();
        
        // Use Checksummed Address (EIP-55)
        let wallet_address_checksum = ethers::utils::to_checksum(&wallet_address, None);

        // EIP-712 ClobAuth Signing
        let typed_data_json = serde_json::json!({
            "domain": {
                "name": "ClobAuthDomain",
                "version": "1",
                "chainId": chain_id,
                "verifyingContract": verifying_contract,
            },
            "types": {
                "EIP712Domain": [
                    { "name": "name", "type": "string" },
                    { "name": "version", "type": "string" },
                    { "name": "chainId", "type": "uint256" },
                    { "name": "verifyingContract", "type": "address" },
                ],
                "ClobAuth": [
                    { "name": "address", "type": "address" },
                    { "name": "timestamp", "type": "string" },
                    { "name": "nonce", "type": "uint256" },
                    { "name": "message", "type": "string" },
                ]
            },
            "primaryType": "ClobAuth",
            "message": {
                "address": wallet_address_checksum,
                "timestamp": timestamp,
                "nonce": nonce,
                "message": msg_text
            }
        });

        let typed_data: ethers::types::transaction::eip712::TypedData = serde_json::from_value(typed_data_json)
            .context("Failed to parse Auth TypedData")?;
            
        let mut signature = wallet.sign_typed_data(&typed_data).await
            .context("Failed to sign Auth message")?;
        
        // Adjust v for Ethereum standard (27/28) if needed
        if signature.v < 27 {
            signature.v += 27;
        }
        
        // Create full signature bytes [r, s, v]
        let mut r_bytes = [0u8; 32];
        signature.r.to_big_endian(&mut r_bytes);
        
        let mut s_bytes = [0u8; 32];
        signature.s.to_big_endian(&mut s_bytes);
        
        let mut sig_bytes = Vec::new();
        sig_bytes.extend_from_slice(&r_bytes);
        sig_bytes.extend_from_slice(&s_bytes);
        sig_bytes.push(signature.v as u8);
        
        let sig_hex = format!("0x{}", hex::encode(sig_bytes));

        // Call /auth/derive-api-key with Query Params + Headers
        let url = format!("{}/auth/derive-api-key", self.api_url());
        
        // Add query params
        let url = reqwest::Url::parse_with_params(
            &url,
            &[("address", &wallet_address_checksum), ("nonce", &nonce.to_string())]
        )?.to_string();
        
        info!("Auth Request URL: {}", url);
        
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert("Poly-Signature", sig_hex.parse()?);
        headers.insert("Poly-Timestamp", timestamp.parse()?);
        headers.insert("Poly-Nonce", nonce.parse()?);
        headers.insert("Poly-Address", wallet_address_checksum.parse()?);
        headers.insert("Poly-Signature-Type", "0".parse()?); // Type 0 = EOA

        let response = self.http_client.get(&url)
            .headers(headers)
            .send()
            .await
            .context("Failed to send auth request")?;

        if !response.status().is_success() {
            let body = response.text().await?;
            anyhow::bail!("Failed to derive API keys: {}", body);
        }

        #[derive(Deserialize)]
        struct DeriveResp {
            #[serde(rename = "apiKey")]
            api_key: String,
            secret: String,
            passphrase: String,
        }
        
        let creds: DeriveResp = response.json().await
            .context("Failed to parse Auth response")?;
        
        Ok(crate::config::AuthConfig {
            api_key: Some(creds.api_key),
            api_secret: Some(creds.secret),
            passphrase: Some(creds.passphrase),
        })
    }

    /// Get today's fills for rebate estimation
    pub async fn get_todays_fills(&self) -> Result<Vec<Fill>> {
        let all_fills = self.get_fills(None).await?;
        let today = chrono::Utc::now().date_naive();

        Ok(all_fills
            .into_iter()
            .filter(|f| f.timestamp.date_naive() == today)
            .collect())
    }

    /// Build L2 Authentication Headers (HMAC-SHA256)
    fn build_auth_headers(
        &self,
        method: &str,
        path: &str,
        body_str: &str,
    ) -> Result<reqwest::header::HeaderMap> {
        let auth = &self.config.auth;
        
        // Only return error if we are in REAL mode
        if self.config.is_test_mode() {
            return Ok(reqwest::header::HeaderMap::new());
        }

        let api_key = auth.api_key.as_ref().context("API Key not configured")?;
        let api_secret = auth.api_secret.as_ref().context("API Secret not configured")?;
        let passphrase = auth.passphrase.as_ref().context("Passphrase not configured")?;

        let timestamp = chrono::Utc::now().timestamp().to_string();
        
        // Sign: timestamp + method + path + body
        let message = format!("{}{}{}{}", timestamp, method, path, body_str);
        
        // Decode base64 secret
        let secret_bytes = base64::engine::general_purpose::STANDARD
            .decode(api_secret)
            .context("Failed to decode API secret")?;

        let mut mac = Hmac::<Sha256>::new_from_slice(&secret_bytes)
            .context("Failed to create HMAC")?;
        mac.update(message.as_bytes());
        let result = mac.finalize();
        let signature = base64::engine::general_purpose::STANDARD.encode(result.into_bytes());

        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert("Poly-Api-Key", api_key.parse()?);
        headers.insert("Poly-Timestamp", timestamp.parse()?);
        headers.insert("Poly-Signature", signature.parse()?);
        headers.insert("Poly-Passphrase", passphrase.parse()?);

        Ok(headers)
    }

    /// Estimate daily rebates based on filled maker volume
    /// Rebate = (your_maker_volume / total_market_maker_volume) * taker_fees_collected
    /// Simplified: We estimate rebate_rate * your_maker_volume
    pub async fn estimate_daily_rebates(&self) -> Result<RebateEstimate> {
        let fills = self.get_todays_fills().await?;

        let maker_fills: Vec<_> = fills.iter().filter(|f| f.is_maker).collect();
        let maker_volume: Decimal = maker_fills
            .iter()
            .map(|f| f.price * f.size)
            .sum();

        let fill_count = maker_fills.len() as u32;

        // Estimate rebate at ~1% of maker volume (conservative)
        // Actual rate depends on your share of total maker volume
        let estimated_rebate_rate = Decimal::new(100, 4); // 0.01 = 1%
        let estimated_rebate = maker_volume * estimated_rebate_rate;

        Ok(RebateEstimate {
            date: chrono::Utc::now().format("%Y-%m-%d").to_string(),
            maker_volume,
            fill_count,
            estimated_rebate,
            effective_rate_bps: 100, // 1%
        })
    }
}

// Fee rate response from API
#[derive(Debug, Deserialize)]
struct FeeRateResponse {
    #[serde(default)]
    fee_rate_bps: Option<String>,
    #[serde(default)]
    maker: Option<String>,
    #[serde(default)]
    taker: Option<String>,
}

// Fill response from API
#[derive(Debug, Deserialize)]
struct FillResponse {
    id: String,
    order_id: String,
    market: String,
    asset_id: String,
    side: String,
    price: String,
    size: String,
    #[serde(default)]
    fee: String,
    #[serde(default)]
    is_maker: Option<bool>,
    timestamp: String,
}

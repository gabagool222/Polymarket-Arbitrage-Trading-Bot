//! Polymarket CLOB API client

use crate::config::AppConfig;
use crate::polymarket::types::*;
use anyhow::{Context, Result};
use ethers::prelude::*;
use hmac::{Hmac, Mac};
use reqwest::Client;
use rust_decimal::Decimal;
use serde::Deserialize;
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

// CreateOrderRequest and SignedOrder removed
#[derive(Debug, Deserialize)]
struct MarketMetadataResponse {
     minimum_order_size: String,
     minimum_tick_size: String,
}

impl PolymarketClient {
    /// Create a new Polymarket client
    pub fn new(config: Arc<AppConfig>) -> Result<Self> {
        // Fake User-Agent to bypass Cloudflare
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            reqwest::header::USER_AGENT, 
            "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36".parse().unwrap()
        );

        let mut client_builder = Client::builder()
            .default_headers(headers)
            .timeout(std::time::Duration::from_secs(30));

        // PROXY SUPPORT
        if let Ok(proxy_url) = std::env::var("POLY_PROXY") {
            if !proxy_url.is_empty() {
                info!("Running behind Proxy: {}...", proxy_url.chars().take(15).collect::<String>());
                let proxy = reqwest::Proxy::all(&proxy_url).context("Failed to configure proxy")?;
                client_builder = client_builder.proxy(proxy);
            }
        }

        let http_client = client_builder.build().context("Failed to create HTTP client")?;

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

    // place_orders (batch) removed - unused and outdated

    /// Place a limit order (real mode only) - Uses Python SDK for signing
    pub async fn place_order(&self, order: &OrderRequest) -> Result<Order> {
        if self.config.is_test_mode() {
            return self.simulate_order(order);
        }

        // Use Python SDK for order signing and placement
        // Use Python SDK for order signing and placement
        info!("Using Python SDK for order placement...");
        
        let side_str = match order.side {
            Side::Buy => "BUY",
            Side::Sell => "SELL",
        };

        // Call Python script with inherited and explicit environment variables
        let mut command = std::process::Command::new("/app/venv/bin/python3");
        command.arg("/app/scripts/sign_order.py")
            .arg(&order.token_id)
            .arg(side_str)
            .arg(order.price.to_string())
            .arg(order.size.to_string())
            .arg(if order.neg_risk { "true" } else { "false" });
            
        // Explicitly pass sensitive environment variables from Config/Env
        if let Ok(pk) = std::env::var("POLY_PRIVATE_KEY") {
            command.env("POLY_PRIVATE_KEY", pk);
        }
        if let Some(fa) = &self.config.auth.funder_address {
            command.env("POLY_FUNDER_ADDRESS", fa);
        }
        if let Some(ak) = &self.config.auth.api_key {
            command.env("POLY_API_KEY", ak);
        }
        if let Some(as_key) = &self.config.auth.api_secret {
            command.env("POLY_API_SECRET", as_key);
        }
        if let Some(ap) = &self.config.auth.passphrase {
            command.env("POLY_PASSPHRASE", ap);
        }

        // PROXY SUPPORT FOR PYTHON
        if let Ok(proxy_url) = std::env::var("POLY_PROXY") {
            if !proxy_url.is_empty() {
                command.env("HTTPS_PROXY", &proxy_url);
                command.env("HTTP_PROXY", &proxy_url);
            }
        }

        let output = command.output()
            .context("Failed to execute Python signing script")?;

        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);

        if !stderr.is_empty() {
            warn!("Python script stderr: {}", stderr);
        }

        info!("Python SDK response: {}", stdout);

        // Parse response
        let response: serde_json::Value = serde_json::from_str(&stdout)
            .context(format!("Failed to parse Python response: {}", stdout))?;

        if response.get("success").and_then(|v| v.as_bool()).unwrap_or(false) {
            let order_id = response.get("order_id")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();

            info!("Order placed via Python SDK: {}", order_id);

            Ok(Order {
                id: order_id,
                status: OrderStatus::Live,
                token_id: order.token_id.clone(),
                side: order.side,
                original_size: order.size,
                size_matched: Decimal::ZERO,
                price: order.price,
                created_at: chrono::Utc::now(),
            })
        } else {
            let error = response.get("error")
                .and_then(|v| v.as_str())
                .unwrap_or("Unknown error");
            anyhow::bail!("Python SDK error: {}", error);
        }
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

    // build_signed_order removed (using Python SDK)

    /// Cancel an order
    pub async fn cancel_order(&self, order_id: &str) -> Result<()> {
        if self.config.is_test_mode() {
            info!("[SIMULATION] Would cancel order: {}", order_id);
            return Ok(());
        }

        let path = format!("/order/{}", order_id);
        let url = format!("{}{}", self.api_url(), path);

        let auth_headers = self.build_auth_headers("DELETE", &path, "").await?;

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
    // Balance & Account Info
    // =========================================================================

    /// Get USDC balance for the trading wallet
    /// Uses the CLOB API to get the current balance
    pub async fn get_balance(&self) -> Result<Decimal> {
        if self.config.is_test_mode() {
            // In simulation mode, return a reasonable test balance
            return Ok(Decimal::new(100, 0)); // $100 test balance
        }

        // Use funder address if in POLY_PROXY mode, otherwise wallet address
        let address = if let Some(funder) = &self.config.auth.funder_address {
            funder.clone()
        } else {
            let wallet = self.wallet.as_ref()
                .context("Wallet not initialized")?;
            format!("{:#x}", wallet.address())
        };

        // Call CLOB API for balance-allowance (Authenticated)
        // Endpoint: GET /balance-allowance?asset_type=COLLATERAL
        let path = "/balance-allowance";
        let query = "?asset_type=COLLATERAL";
        let url = format!("{}{}{}", self.api_url(), path, query);

        debug!("Fetching balance allowance for: {}", address);

        // Build L2 Auth Headers (Required for this endpoint)
        let auth_headers = self.build_auth_headers("GET", &format!("{}{}", path, query), "").await
            .unwrap_or_else(|e| {
                warn!("Failed to build auth headers for balance: {}", e);
                reqwest::header::HeaderMap::new()
            });

        let mut request = self.http_client.get(&url);
        
        // Add headers
        for (k, v) in &auth_headers {
            if let Ok(val) = reqwest::header::HeaderValue::from_str(v.to_str().unwrap_or_default()) {
                 request = request.header(k, val);
            }
        }
        
        // Add API Key headers manually if not in auth_headers (build_auth_headers adds POLY_ prefix)
        // But the endpoint usually expects standard headers OR poly headers. Let's use standard too if needed.
        if let Some(api_key) = &self.config.auth.api_key {
            request = request.header("POLY_API_KEY", api_key);
        }
        if let Some(passphrase) = &self.config.auth.passphrase {
            request = request.header("POLY_PASSPHRASE", passphrase);
        }

        let response = request.send().await;

        match response {
            Ok(resp) => {
                if resp.status().is_success() {
                    // Response format: { "allowance": "...", "balance": "..." }
                    #[derive(Deserialize)]
                    struct BalanceAllowanceResponse {
                        #[serde(default)]
                        balance: String, // String decimal
                    }

                    let body = resp.text().await.unwrap_or_default();
                    let balance_resp: Result<BalanceAllowanceResponse, _> = serde_json::from_str(&body);

                    match balance_resp {
                        Ok(b) => {
                            let balance: Decimal = b.balance.parse().unwrap_or(Decimal::ZERO);
                            debug!("Balance: ${}", balance);
                            Ok(balance)
                        }
                        Err(e) => {
                            warn!("Failed to parse balance response: {} | body: {}", e, body);
                            Ok(Decimal::ZERO)
                        }
                    }
                } else {
                    let status = resp.status();
                    let text = resp.text().await.unwrap_or_default();
                    // Don't warn on 404/401 aggressively to avoid spam log, just debug
                    if status.as_u16() == 404 || status.as_u16() == 401 {
                         debug!("Balance API returned {}: {}", status, text);
                    } else {
                         warn!("Balance API returned {}: {}", status, text);
                    }
                    Ok(Decimal::ZERO)
                }
            }
            Err(e) => {
                warn!("Failed to fetch balance: {}", e);
                Ok(Decimal::ZERO)
            }
        }
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

        // Correctly format address (lowercase hex with 0x)
        let mut url = format!(
            "{}/data/fills?maker={:#x}", 
            self.api_url(),
            wallet.address()
        );

        if let Some(market) = market_id {
            url.push_str(&format!("&market={}", market));
        }

        // Add Auth Headers (Required for /fills)
        let auth_headers = self.build_auth_headers("GET", "/data/fills", "").await?;
        
        let mut request = self.http_client.get(&url);
        
        // Add L2 Auth headers
        for (k, v) in &auth_headers {
            if let Ok(val) = reqwest::header::HeaderValue::from_str(v.to_str().unwrap_or_default()) {
                 request = request.header(k, val);
            }
        }

        // Add API Key headers
        if let Some(api_key) = &self.config.auth.api_key {
            request = request.header("POLY_API_KEY", api_key);
        }
        if let Some(passphrase) = &self.config.auth.passphrase {
            request = request.header("POLY_PASSPHRASE", passphrase);
        }

        let response = request.send().await.context("Failed to fetch fills")?;

        if response.status().as_u16() == 404 {
             return Ok(Vec::new()); // Return empty if not found endpoint or empty data
        }
        
        if !response.status().is_success() {
             let status = response.status();
             let text = response.text().await.unwrap_or_default();
             warn!("Fills API Error {}: {}", status, text);
             return Ok(Vec::new());
        }

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

    /// Fetch Server Time to avoid Clock Skew
    pub async fn get_server_time(&self) -> Result<i64> {
        let url = format!("{}/time", self.api_url());
        let resp = self.http_client.get(&url).send().await;
        
        match resp {
            Ok(r) => {
                #[derive(Deserialize)]
                struct TimeResp { timestamp: i64 } // server returns int timestamp
                // Or sometimes just "iso". Let's try simple json.
                // If fails, fallback to local.
                let t_json: serde_json::Value = r.json().await?;
                if let Some(ts) = t_json.get("timestamp").and_then(|v| v.as_i64()) {
                   return Ok(ts);
                }
            }
            Err(_) => {}
        }
        // Fallback to local time
        Ok(chrono::Utc::now().timestamp())
    }

    /// Verify that current API credentials are valid for trading
    /// This calls a simple authenticated endpoint to check if credentials work
    pub async fn verify_api_credentials(&self) -> Result<bool> {
        if self.config.is_test_mode() {
            return Ok(true);
        }

        // Use funder address if in POLY_PROXY mode, otherwise wallet address
        let maker_address = if let Some(funder) = &self.config.auth.funder_address {
            if let Ok(addr) = funder.parse::<Address>() {
                ethers::utils::to_checksum(&addr, None)
            } else {
                funder.clone()
            }
        } else {
            let wallet = self.wallet.as_ref().context("Wallet not initialized")?;
            ethers::utils::to_checksum(&wallet.address(), None)
        };

        let path = "/orders";
        let query = format!("?maker={}&status=LIVE&limit=1", maker_address);
        let url = format!("{}{}{}", self.api_url(), path, query);

        let auth_headers = self.build_auth_headers("GET", &format!("{}{}", path, query), "").await?;

        info!("Verifying credentials for address: {}", maker_address);

        let response = self.http_client
            .get(&url)
            .headers(auth_headers)
            .send()
            .await
            .context("Failed to verify credentials")?;

        let status = response.status();
        let body = response.text().await.unwrap_or_default();

        if status.is_success() {
            info!("API credentials verified successfully");
            Ok(true)
        } else if status.as_u16() == 401 {
            warn!("❌ API credentials INVALID: {}", body);
            warn!("💡 Credentials may not match the wallet. Try re-deriving.");
            Ok(false)
        } else {
            warn!("⚠️ Credentials verification returned {}: {}", status, body);
            Ok(false)
        }
    }

    /// Automatically derive API credentials...
    pub async fn derive_api_keys(&self) -> Result<crate::config::AuthConfig> {
        let wallet = self.wallet.as_ref()
            .context("Wallet not initialized. Private key required for creating API keys.")?;

        info!("Deriving L2 API Keys from Private Key...");
        
        // Retry Loop
        let mut last_error = String::new();
        for attempt in 1..=3 {
            info!("Derive attempt {}/3...", attempt);
            
            // 1. Get accurate timestamp (Server Time preferred)
            let timestamp_val = self.get_server_time().await.unwrap_or_else(|_| chrono::Utc::now().timestamp());
            let timestamp = timestamp_val.to_string();
            
            info!("Using Timestamp: {}", timestamp);
            
            let nonce = "0"; 
            let msg_text = "This message attests that I control the given wallet";
            
            // Always use SIGNER address for derive (from private key)
            // This is the address that signs the EIP-712 message
            // Funder address is only used for order building, not for auth
            let chain_id = 137;
            let wallet_address_checksum = ethers::utils::to_checksum(&wallet.address(), None);
            let is_proxy_mode = self.config.auth.funder_address.is_some();

            info!("Deriving for signer address: {} (proxy_mode: {})", wallet_address_checksum, is_proxy_mode);
            if is_proxy_mode {
                info!("Funder address: {}", self.config.auth.funder_address.as_deref().unwrap_or("?"));
            }

            let typed_data_json = serde_json::json!({
                "domain": {
                    "name": "ClobAuthDomain",
                    "version": "1",
                    "chainId": chain_id,
                },
                "types": {
                    "EIP712Domain": [
                        { "name": "name", "type": "string" },
                        { "name": "version", "type": "string" },
                        { "name": "chainId", "type": "uint256" },
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
                    "nonce": 0,
                    "message": msg_text
                }
            });

            // Sign
            let typed_data: ethers::types::transaction::eip712::TypedData = match serde_json::from_value(typed_data_json) {
                Ok(td) => td,
                Err(e) => { last_error = e.to_string(); continue; }
            };
                
            let mut signature = match wallet.sign_typed_data(&typed_data).await {
                Ok(s) => s,
                Err(e) => { last_error = e.to_string(); continue; }
            };
            
            if signature.v < 27 { signature.v += 27; }
            let mut r_bytes = [0u8; 32]; signature.r.to_big_endian(&mut r_bytes);
            let mut s_bytes = [0u8; 32]; signature.s.to_big_endian(&mut s_bytes);
            let mut sig_bytes = Vec::new();
            sig_bytes.extend_from_slice(&r_bytes);
            sig_bytes.extend_from_slice(&s_bytes);
            sig_bytes.push(signature.v as u8);
            let sig_hex = format!("0x{}", hex::encode(sig_bytes));

            // Build L1 headers
            let mut headers = reqwest::header::HeaderMap::new();
            headers.insert("POLY_SIGNATURE", sig_hex.parse().unwrap());
            headers.insert("POLY_TIMESTAMP", timestamp.parse().unwrap());
            headers.insert("POLY_NONCE", nonce.parse().unwrap());
            headers.insert("POLY_ADDRESS", wallet_address_checksum.parse().unwrap());

            info!("L1 Headers: POLY_ADDRESS={}, POLY_TIMESTAMP={}", wallet_address_checksum, timestamp);

            // Try CREATE first (POST /auth/api-key), then DERIVE (GET /auth/derive-api-key)
            let create_url = format!("{}/auth/api-key", self.api_url());
            let derive_url = format!("{}/auth/derive-api-key", self.api_url());

            // Try CREATE
            info!("Attempting to CREATE new API key...");
            let create_response = self.http_client
                .post(&create_url)
                .headers(headers.clone())
                .send()
                .await;

            let response = match create_response {
                Ok(r) if r.status().is_success() => {
                    info!("CREATE succeeded!");
                    r
                },
                Ok(r) => {
                    let create_err = r.text().await.unwrap_or_default();
                    info!("CREATE failed ({}), trying DERIVE...", create_err);

                    // Try DERIVE
                    match self.http_client.get(&derive_url).headers(headers.clone()).send().await {
                        Ok(r) => r,
                        Err(e) => {
                            last_error = e.to_string();
                            tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;
                            continue;
                        }
                    }
                },
                Err(e) => {
                    last_error = e.to_string();
                    tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;
                    continue;
                }
            };

            if response.status().is_success() {
                #[derive(Deserialize)]
                struct DeriveResp { #[serde(rename = "apiKey")] api_key: String, secret: String, passphrase: String }
                let creds: DeriveResp = response.json().await.context("Parse error")?;

                info!("✅ API Key obtained: {}...", &creds.api_key[..8.min(creds.api_key.len())]);

                return Ok(crate::config::AuthConfig {
                    api_key: Some(creds.api_key),
                    api_secret: Some(creds.secret),
                    passphrase: Some(creds.passphrase),
                    funder_address: self.config.auth.funder_address.clone(),
                });
            } else {
                let body = response.text().await.unwrap_or_default();
                warn!("Attempt {} failed: {}", attempt, body);
                last_error = body;
                tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;
            }
        }
        
        anyhow::bail!("All derive attempts failed. Last error: {}", last_error);
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

    /// Build L2 Authentication Headers (HMAC-SHA256 + ETH Sign)
    async fn build_auth_headers(
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

        // Step 1: Build HMAC message exactly like Python SDK
        // Python: message = str(timestamp) + str(method) + str(requestPath) + str(body).replace("'", '"')
        let message = if body_str.is_empty() {
            format!("{}{}{}", timestamp, method, path)
        } else {
            format!("{}{}{}{}", timestamp, method, path, body_str)
        };

        // Decode base64 secret - URL_SAFE_NO_PAD handles secrets with _ and -
        let secret_bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(api_secret.trim_end_matches('='))
            .or_else(|_| base64::engine::general_purpose::URL_SAFE.decode(api_secret))
            .or_else(|_| base64::engine::general_purpose::STANDARD.decode(api_secret))
            .context("Failed to decode API secret")?;

        let mut mac = Hmac::<Sha256>::new_from_slice(&secret_bytes)
            .context("Failed to create HMAC")?;
        mac.update(message.as_bytes());
        let result = mac.finalize();

        // Encode with URL_SAFE (matches Python's urlsafe_b64encode)
        let signature = base64::engine::general_purpose::URL_SAFE.encode(result.into_bytes());

        // IMPORTANT: POLY_ADDRESS for L2 auth ALWAYS uses the SIGNER address (from wallet)
        // NOT the funder address! The funder_address is only for order signing/building.
        // Tested and confirmed: L2 auth works with signer address, fails with funder address.
        let wallet = self.wallet.as_ref().context("Wallet not initialized")?;
        let wallet_address = ethers::utils::to_checksum(&wallet.address(), None);

        // Log auth headers being sent
        // info!("🔐 L2 Auth Headers:");
        // info!("   POLY_ADDRESS: {}", wallet_address);
        // info!("   POLY_API_KEY: {}", api_key);
        // info!("   POLY_PASSPHRASE: {}...", &passphrase[..8.min(passphrase.len())]);

        // L2 headers also use POLY_ prefix (uppercase with underscore)
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert("POLY_ADDRESS", wallet_address.parse()?);
        headers.insert("POLY_API_KEY", api_key.parse()?);
        headers.insert("POLY_TIMESTAMP", timestamp.parse()?);
        headers.insert("POLY_SIGNATURE", signature.parse()?);
        headers.insert("POLY_PASSPHRASE", passphrase.parse()?);

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

//! Polymarket CLOB API client

use crate::config::AppConfig;
use crate::polymarket::types::*;
use anyhow::{Context, Result};
use ethers::prelude::*;
use reqwest::Client;
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tracing::{debug, info, warn};

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
            .map(|m| Market {
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
                    });
                }
            }
        }

        Ok(markets)
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

    /// Place a limit order (real mode only)
    pub async fn place_order(&self, order: &OrderRequest) -> Result<Order> {
        if self.config.is_test_mode() {
            return self.simulate_order(order);
        }

        let wallet = self.wallet.as_ref()
            .context("Wallet not initialized for real trading")?;

        // Build and sign the order
        let signed_order = self.build_signed_order(order, wallet).await?;

        let url = format!("{}/order", self.api_url());

        let response = self
            .http_client
            .post(&url)
            .json(&CreateOrderRequest { order: signed_order })
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

    /// Build a signed order
    async fn build_signed_order(
        &self,
        order: &OrderRequest,
        wallet: &LocalWallet,
    ) -> Result<SignedOrder> {
        let maker = format!("{:?}", wallet.address());
        let salt = format!("{}", chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0));
        let nonce = "0".to_string();
        let expiration = order
            .expiration
            .unwrap_or_else(|| chrono::Utc::now().timestamp() + 3600)
            .to_string();

        // Calculate amounts based on price and size
        let price_decimal = order.price;
        let size_decimal = order.size;
        let maker_amount = (size_decimal * Decimal::new(1_000_000, 0)).to_string();
        let taker_amount = (size_decimal * price_decimal * Decimal::new(1_000_000, 0)).to_string();

        // Create message hash for signing
        let message = format!(
            "{}{}{}{}{}{}{}",
            salt, maker, order.token_id, maker_amount, taker_amount, expiration, nonce
        );

        let signature = wallet
            .sign_message(message.as_bytes())
            .await
            .context("Failed to sign order")?;

        Ok(SignedOrder {
            salt,
            maker: maker.clone(),
            signer: maker,
            taker: "0x0000000000000000000000000000000000000000".to_string(),
            token_id: order.token_id.clone(),
            maker_amount,
            taker_amount,
            expiration,
            nonce,
            fee_rate_bps: "0".to_string(), // Maker gets rebates
            side: order.side.to_string(),
            signature_type: 0,
            signature: format!("0x{}", hex::encode(signature.to_vec())),
        })
    }

    /// Cancel an order
    pub async fn cancel_order(&self, order_id: &str) -> Result<()> {
        if self.config.is_test_mode() {
            info!("[SIMULATION] Would cancel order: {}", order_id);
            return Ok(());
        }

        let url = format!("{}/order/{}", self.api_url(), order_id);

        let response = self
            .http_client
            .delete(&url)
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

    /// Get today's fills for rebate estimation
    pub async fn get_todays_fills(&self) -> Result<Vec<Fill>> {
        let all_fills = self.get_fills(None).await?;
        let today = chrono::Utc::now().date_naive();

        Ok(all_fills
            .into_iter()
            .filter(|f| f.timestamp.date_naive() == today)
            .collect())
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

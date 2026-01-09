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

#[derive(Debug, Deserialize)]
struct MarketsResponse {
    data: Vec<MarketData>,
    #[serde(default)]
    next_cursor: Option<String>,
}

#[derive(Debug, Deserialize)]
struct MarketData {
    condition_id: String,
    question_id: String,
    tokens: Vec<TokenData>,
    slug: Option<String>,
    question: String,
    end_date_iso: Option<String>,
    active: bool,
    closed: bool,
    accepting_orders: bool,
}

#[derive(Debug, Deserialize)]
struct TokenData {
    token_id: String,
    outcome: String,
    price: Option<f64>,
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

    /// Fetch markets matching slug patterns
    pub async fn fetch_markets(&self, slug_pattern: &str) -> Result<Vec<Market>> {
        let url = format!(
            "{}/markets?slug_filter={}&active=true&closed=false",
            self.gamma_url(),
            slug_pattern
        );

        debug!("Fetching markets from: {}", url);

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

        let markets_data: Vec<MarketData> = serde_json::from_str(&body)
            .context("Failed to parse markets response")?;

        let markets: Vec<Market> = markets_data
            .into_iter()
            .map(|m| Market {
                condition_id: m.condition_id,
                question_id: m.question_id,
                tokens: m
                    .tokens
                    .into_iter()
                    .map(|t| Token {
                        token_id: t.token_id,
                        outcome: t.outcome,
                        price: t.price.map(|p| Decimal::try_from(p).unwrap_or_default()),
                    })
                    .collect(),
                slug: m.slug.unwrap_or_default(),
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

    /// Fetch 15-minute crypto markets (BTC/ETH/SOL Up/Down)
    pub async fn fetch_15min_crypto_markets(&self) -> Result<Vec<Market>> {
        let mut all_markets = Vec::new();

        for pattern in ["15-min", "15min"] {
            match self.fetch_markets(pattern).await {
                Ok(markets) => {
                    for market in markets {
                        let q = market.question.to_lowercase();
                        if (q.contains("btc") || q.contains("eth") || q.contains("sol"))
                            && (q.contains("up") || q.contains("down"))
                        {
                            all_markets.push(market);
                        }
                    }
                }
                Err(e) => {
                    warn!("Failed to fetch markets for pattern '{}': {}", pattern, e);
                }
            }
        }

        Ok(all_markets)
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
}

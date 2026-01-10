//! Polymarket CLOB API client
//!
//! Implements proper EIP-712 typed data signing per Polymarket CLOB spec.
//! Reference: https://docs.polymarket.com/

use crate::config::AppConfig;
use crate::polymarket::types::*;
use anyhow::{Context, Result};
use ethers::prelude::*;
use ethers::types::transaction::eip712::{EIP712Domain, Eip712};
use reqwest::header::{HeaderMap, HeaderValue, AUTHORIZATION};
use reqwest::Client;
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tracing::{debug, info, warn};

// Polymarket CTF Exchange chain ID (Polygon mainnet)
const POLYGON_CHAIN_ID: u64 = 137;

// Polymarket CTF Exchange contract address
const CTF_EXCHANGE_ADDRESS: &str = "0x4bFb41d5B3570DeFd03C39a9A4D8dE6Bd8B8982E";

// Negation Risk CTF Exchange (for conditional tokens)
const NEG_RISK_CTF_EXCHANGE: &str = "0xC5d563A36AE78145C45a50134d48A1215220f80a";

/// Polymarket CLOB client for REST API interactions
pub struct PolymarketClient {
    config: Arc<AppConfig>,
    http_client: Client,
    wallet: Option<LocalWallet>,
    /// L2 API key (derived from signing a message with wallet)
    api_creds: Option<ApiCredentials>,
}

/// API credentials for authenticated requests
#[derive(Debug, Clone)]
struct ApiCredentials {
    api_key: String,
    api_secret: String,
    api_passphrase: String,
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

/// EIP-712 Order struct for Polymarket CTF Exchange
/// Reference: https://docs.polymarket.com/
#[derive(Debug, Clone, Serialize, Deserialize, Eip712, EthAbiType)]
#[eip712(
    name = "Polymarket CTF Exchange",
    version = "1",
    chain_id = 137,
    verifying_contract = "0x4bFb41d5B3570DeFd03C39a9A4D8dE6Bd8B8982E"
)]
struct Order712 {
    /// Random salt for uniqueness
    salt: U256,
    /// Maker address
    maker: Address,
    /// Signer address (usually same as maker)
    signer: Address,
    /// Taker address (0x0 for any taker)
    taker: Address,
    /// Token ID being traded
    token_id: U256,
    /// Amount of tokens maker is selling/buying
    maker_amount: U256,
    /// Amount of USDC the maker wants
    taker_amount: U256,
    /// Order expiration timestamp
    expiration: U256,
    /// Nonce for order uniqueness
    nonce: U256,
    /// Fee rate in basis points
    fee_rate_bps: U256,
    /// 0 = BUY, 1 = SELL
    side: U256,
    /// Signature type: 0 = EOA, 1 = POLY_PROXY, 2 = POLY_GNOSIS_SAFE
    signature_type: U256,
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
                    Some(wallet.with_chain_id(POLYGON_CHAIN_ID))
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

        // API credentials are derived from wallet signing (set via env or config)
        let api_creds = if let (Ok(key), Ok(secret), Ok(passphrase)) = (
            std::env::var("POLY_API_KEY"),
            std::env::var("POLY_API_SECRET"),
            std::env::var("POLY_API_PASSPHRASE"),
        ) {
            Some(ApiCredentials {
                api_key: key,
                api_secret: secret,
                api_passphrase: passphrase,
            })
        } else {
            None
        };

        Ok(Self {
            config,
            http_client,
            wallet,
            api_creds,
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

    /// Build authentication headers for L2 (Poly) API requests
    /// Uses HMAC-SHA256 signature with timestamp
    fn build_auth_headers(&self, method: &str, path: &str, body: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();

        if let Some(ref creds) = self.api_creds {
            let timestamp = chrono::Utc::now().timestamp_millis().to_string();

            // Build the signature message: timestamp + method + path + body
            let message = format!("{}{}{}{}", timestamp, method, path, body);

            // HMAC-SHA256 signature
            use hmac::{Hmac, Mac};
            use sha2::Sha256;
            type HmacSha256 = Hmac<Sha256>;

            if let Ok(mut mac) = HmacSha256::new_from_slice(creds.api_secret.as_bytes()) {
                mac.update(message.as_bytes());
                let signature = base64::Engine::encode(
                    &base64::engine::general_purpose::STANDARD,
                    mac.finalize().into_bytes()
                );

                headers.insert(
                    "POLY-API-KEY",
                    HeaderValue::from_str(&creds.api_key).unwrap_or(HeaderValue::from_static("")),
                );
                headers.insert(
                    "POLY-SIGNATURE",
                    HeaderValue::from_str(&signature).unwrap_or(HeaderValue::from_static("")),
                );
                headers.insert(
                    "POLY-TIMESTAMP",
                    HeaderValue::from_str(&timestamp).unwrap_or(HeaderValue::from_static("")),
                );
                headers.insert(
                    "POLY-PASSPHRASE",
                    HeaderValue::from_str(&creds.api_passphrase).unwrap_or(HeaderValue::from_static("")),
                );
            }
        }

        headers
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

        // Build and sign the order with EIP-712
        let signed_order = self.build_signed_order(order, wallet).await?;
        let create_req = CreateOrderRequest { order: signed_order };
        let body_json = serde_json::to_string(&create_req)?;

        // Build URL and authentication headers
        let path = "/order";
        let url = format!("{}{}", self.api_url(), path);
        let auth_headers = self.build_auth_headers("POST", path, &body_json);

        let response = self
            .http_client
            .post(&url)
            .headers(auth_headers)
            .header("Content-Type", "application/json")
            .body(body_json)
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

    /// Build a signed order using EIP-712 typed data signing
    /// Reference: https://docs.polymarket.com/
    async fn build_signed_order(
        &self,
        order: &OrderRequest,
        wallet: &LocalWallet,
    ) -> Result<SignedOrder> {
        let maker_address = wallet.address();
        let maker = format!("{:?}", maker_address);

        // Generate random salt
        let salt_bytes: [u8; 32] = rand::random();
        let salt = U256::from_big_endian(&salt_bytes);

        let nonce = U256::zero();
        let expiration = U256::from(
            order.expiration.unwrap_or_else(|| chrono::Utc::now().timestamp() + 3600)
        );

        // Calculate amounts based on price and size
        // Polymarket uses 6 decimal places (USDC)
        let price_decimal = order.price;
        let size_decimal = order.size;
        let scale = Decimal::new(1_000_000, 0); // 10^6

        // maker_amount = number of tokens (in base units)
        let maker_amount_dec = size_decimal * scale;
        let maker_amount = U256::from_dec_str(&maker_amount_dec.to_string())
            .unwrap_or(U256::zero());

        // taker_amount = USDC to pay/receive (in base units)
        let taker_amount_dec = size_decimal * price_decimal * scale;
        let taker_amount = U256::from_dec_str(&taker_amount_dec.to_string())
            .unwrap_or(U256::zero());

        // Parse token_id to U256
        let token_id = if order.token_id.starts_with("0x") {
            U256::from_str_radix(&order.token_id[2..], 16).unwrap_or(U256::zero())
        } else {
            U256::from_dec_str(&order.token_id).unwrap_or(U256::zero())
        };

        // Side: 0 = BUY, 1 = SELL
        let side = match order.side {
            Side::Buy => U256::zero(),
            Side::Sell => U256::one(),
        };

        // Build the EIP-712 typed data order
        let order_712 = Order712 {
            salt,
            maker: maker_address,
            signer: maker_address,
            taker: Address::zero(),
            token_id,
            maker_amount,
            taker_amount,
            expiration,
            nonce,
            fee_rate_bps: U256::zero(), // Maker gets rebates
            side,
            signature_type: U256::zero(), // EOA signature
        };

        // Sign using EIP-712 typed data
        let signature = wallet
            .sign_typed_data(&order_712)
            .await
            .context("Failed to sign EIP-712 order")?;

        Ok(SignedOrder {
            salt: salt.to_string(),
            maker: maker.clone(),
            signer: maker,
            taker: "0x0000000000000000000000000000000000000000".to_string(),
            token_id: order.token_id.clone(),
            maker_amount: maker_amount.to_string(),
            taker_amount: taker_amount.to_string(),
            expiration: expiration.to_string(),
            nonce: nonce.to_string(),
            fee_rate_bps: "0".to_string(),
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

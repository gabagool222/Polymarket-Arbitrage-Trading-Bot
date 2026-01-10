//! Polymarket data types and structures

use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use chrono::{DateTime, Utc};

/// Order side
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum Side {
    Buy,
    Sell,
}

impl std::fmt::Display for Side {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Side::Buy => write!(f, "BUY"),
            Side::Sell => write!(f, "SELL"),
        }
    }
}

/// Token type (Yes or No outcome)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TokenType {
    Yes,
    No,
}

impl std::fmt::Display for TokenType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TokenType::Yes => write!(f, "Yes"),
            TokenType::No => write!(f, "No"),
        }
    }
}

/// Market information
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Market {
    pub condition_id: String,
    pub question_id: String,
    pub tokens: Vec<Token>,
    pub slug: String,
    pub question: String,
    pub end_date_iso: Option<String>,
    pub active: bool,
    pub closed: bool,
    pub accepting_orders: bool,
    /// CLOB token IDs from Gamma API - index 0 = Yes/Up, index 1 = No/Down
    #[serde(default)]
    pub clob_token_ids: Vec<String>,
}

impl Market {
    /// Get Yes/Up token ID (index 0)
    pub fn yes_token_id(&self) -> Option<&str> {
        self.clob_token_ids.get(0).map(|s| s.as_str())
    }

    /// Get No/Down token ID (index 1)
    pub fn no_token_id(&self) -> Option<&str> {
        self.clob_token_ids.get(1).map(|s| s.as_str())
    }
}

/// Crypto asset type for 15-min markets
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CryptoAsset {
    BTC,
    ETH,
    SOL,
}

impl std::fmt::Display for CryptoAsset {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CryptoAsset::BTC => write!(f, "BTC"),
            CryptoAsset::ETH => write!(f, "ETH"),
            CryptoAsset::SOL => write!(f, "SOL"),
        }
    }
}

impl CryptoAsset {
    /// Get Binance symbol for this asset
    pub fn binance_symbol(&self) -> &'static str {
        match self {
            CryptoAsset::BTC => "btcusdt",
            CryptoAsset::ETH => "ethusdt",
            CryptoAsset::SOL => "solusdt",
        }
    }
}

/// 15-minute crypto Up/Down binary market
/// Specifically designed for BTC/ETH/SOL 15-min markets on Polymarket
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CryptoMarket {
    /// Market slug/identifier
    pub slug: String,
    /// Market title (e.g. "Bitcoin Up or Down - January 9, 7:00PM-7:15PM ET")
    pub title: String,
    /// Condition ID from Polymarket
    pub condition_id: String,
    /// Token ID for "Up" or "Yes" outcome (first positive outcome)
    pub yes_token_id: String,
    /// Token ID for "Down" or "No" outcome (second outcome)
    pub no_token_id: String,
    /// Crypto asset (BTC, ETH, SOL)
    pub asset: CryptoAsset,
    /// Market end time
    pub end_time: Option<String>,
    /// Whether market is accepting orders
    pub accepting_orders: bool,
}

/// Token information
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Token {
    pub token_id: String,
    pub outcome: String,
    pub price: Option<Decimal>,
}

/// Order book entry
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrderBookEntry {
    pub price: Decimal,
    pub size: Decimal,
}

/// Order book for a token
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrderBook {
    pub token_id: String,
    pub bids: Vec<OrderBookEntry>,
    pub asks: Vec<OrderBookEntry>,
    pub timestamp: DateTime<Utc>,
}

impl OrderBook {
    /// Get best bid price
    pub fn best_bid(&self) -> Option<Decimal> {
        self.bids.first().map(|e| e.price)
    }

    /// Get best ask price
    pub fn best_ask(&self) -> Option<Decimal> {
        self.asks.first().map(|e| e.price)
    }

    /// Get mid price
    pub fn mid_price(&self) -> Option<Decimal> {
        match (self.best_bid(), self.best_ask()) {
            (Some(bid), Some(ask)) => Some((bid + ask) / Decimal::new(2, 0)),
            (Some(bid), None) => Some(bid),
            (None, Some(ask)) => Some(ask),
            (None, None) => None,
        }
    }

    /// Get spread in basis points
    pub fn spread_bps(&self) -> Option<u32> {
        match (self.best_bid(), self.best_ask()) {
            (Some(bid), Some(ask)) if bid > Decimal::ZERO => {
                let spread = (ask - bid) / bid * Decimal::new(10000, 0);
                Some(spread.to_string().parse().unwrap_or(0))
            }
            _ => None,
        }
    }
}

/// Market prices for Yes and No tokens
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MarketPrices {
    pub condition_id: String,
    pub yes_price: Decimal,
    pub no_price: Decimal,
    pub yes_token_id: String,
    pub no_token_id: String,
    pub timestamp: DateTime<Utc>,
}

impl MarketPrices {
    /// Sum of Yes and No prices (should be close to 1.0 in efficient market)
    pub fn price_sum(&self) -> Decimal {
        self.yes_price + self.no_price
    }

    /// Check if arbitrage opportunity exists (sum < threshold)
    pub fn has_arb_opportunity(&self, threshold: Decimal) -> bool {
        self.price_sum() < threshold
    }

    /// Get the cheaper side
    pub fn cheaper_side(&self) -> TokenType {
        if self.yes_price < self.no_price {
            TokenType::Yes
        } else {
            TokenType::No
        }
    }
}

/// Order request for placing orders
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrderRequest {
    pub token_id: String,
    pub side: Side,
    pub price: Decimal,
    pub size: Decimal,
    pub order_type: OrderType,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expiration: Option<i64>,
}

/// Order type
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum OrderType {
    #[serde(rename = "GTC")]
    GoodTilCancelled,
    #[serde(rename = "GTD")]
    GoodTilDate,
    #[serde(rename = "FOK")]
    FillOrKill,
}

/// Order status
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum OrderStatus {
    Live,
    Matched,
    Cancelled,
    Expired,
}

/// Order response from CLOB
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Order {
    pub id: String,
    pub status: OrderStatus,
    pub token_id: String,
    pub side: Side,
    pub original_size: Decimal,
    pub size_matched: Decimal,
    pub price: Decimal,
    pub created_at: DateTime<Utc>,
}

/// Trade execution result
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TradeResult {
    pub order_id: String,
    pub token_id: String,
    pub side: Side,
    pub price: Decimal,
    pub size: Decimal,
    pub fee: Decimal,
    pub timestamp: DateTime<Utc>,
    pub is_simulated: bool,
}

/// WebSocket message types
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum WsMessage {
    #[serde(rename = "subscribe")]
    Subscribe {
        channel: String,
        assets_ids: Vec<String>,
    },

    #[serde(rename = "price_change")]
    PriceChange {
        asset_id: String,
        price: String,
        timestamp: String,
    },

    #[serde(rename = "book")]
    OrderBookUpdate {
        asset_id: String,
        market: String,
        bids: Vec<OrderBookEntry>,
        asks: Vec<OrderBookEntry>,
        timestamp: String,
        hash: String,
    },

    #[serde(rename = "last_trade_price")]
    LastTradePrice {
        asset_id: String,
        price: String,
    },

    #[serde(other)]
    Unknown,
}

/// Subscription channel types
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WsChannel {
    Price,
    Book,
    Trade,
}

impl WsChannel {
    pub fn as_str(&self) -> &'static str {
        match self {
            WsChannel::Price => "price",
            WsChannel::Book => "book",
            WsChannel::Trade => "trade",
        }
    }
}

/// Hedging leg status
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LegStatus {
    Pending,
    Filled,
    PartiallyFilled,
    Cancelled,
    Failed,
}

/// Hedging trade with two legs
#[derive(Debug, Clone)]
pub struct HedgeTrade {
    pub id: String,
    pub market_condition_id: String,
    pub leg1_token: TokenType,
    pub leg1_order_id: Option<String>,
    pub leg1_price: Decimal,
    pub leg1_size: Decimal,
    pub leg1_status: LegStatus,
    pub leg2_token: TokenType,
    pub leg2_order_id: Option<String>,
    pub leg2_price: Decimal,
    pub leg2_size: Decimal,
    pub leg2_status: LegStatus,
    pub created_at: DateTime<Utc>,
    pub timeout_at: DateTime<Utc>,
    pub pnl: Option<Decimal>,
}

// =============================================================================
// Maker Rebates Types (Jan 2026 Program)
// =============================================================================

/// Fee rate information for a token
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FeeRate {
    pub token_id: String,
    /// Maker fee in basis points (typically 0 for makers)
    pub maker_fee_bps: u32,
    /// Taker fee in basis points (funds rebate pool)
    pub taker_fee_bps: u32,
    /// Estimated maker rebate in basis points
    pub maker_rebate_bps: u32,
}

impl FeeRate {
    /// Calculate effective rebate rate at a given price
    /// Rebate is symmetric around $0.50, lower at extremes
    pub fn effective_rebate_at_price(&self, price: Decimal) -> Decimal {
        // Max rebate at $0.50, decreasing towards $0 and $1
        // Formula: rebate_rate * 4 * price * (1 - price)
        let base_rate = Decimal::new(self.maker_rebate_bps as i64, 4);
        let adjustment = Decimal::new(4, 0) * price * (Decimal::ONE - price);
        base_rate * adjustment
    }
}

/// Order fill information
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Fill {
    pub id: String,
    pub order_id: String,
    pub market_id: String,
    pub token_id: String,
    pub side: Side,
    pub price: Decimal,
    pub size: Decimal,
    pub fee: Decimal,
    /// True if this fill was from a maker (limit) order
    pub is_maker: bool,
    pub timestamp: DateTime<Utc>,
}

impl Fill {
    /// Calculate volume (price * size) in USD
    pub fn volume(&self) -> Decimal {
        self.price * self.size
    }

    /// Estimate rebate for this fill
    pub fn estimated_rebate(&self, rebate_rate_bps: u32) -> Decimal {
        if !self.is_maker {
            return Decimal::ZERO;
        }
        let rate = Decimal::new(rebate_rate_bps as i64, 4);
        self.volume() * rate
    }
}

/// Daily rebate estimate
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RebateEstimate {
    pub date: String,
    /// Total maker volume executed today
    pub maker_volume: Decimal,
    /// Number of maker fills
    pub fill_count: u32,
    /// Estimated rebate amount (USDC)
    pub estimated_rebate: Decimal,
    /// Effective rebate rate in basis points
    pub effective_rate_bps: u32,
}

impl std::fmt::Display for RebateEstimate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Rebate Est {}: ${:.4} from {} fills (${:.2} volume, {:.2}% rate)",
            self.date,
            self.estimated_rebate,
            self.fill_count,
            self.maker_volume,
            Decimal::new(self.effective_rate_bps as i64, 2)
        )
    }
}

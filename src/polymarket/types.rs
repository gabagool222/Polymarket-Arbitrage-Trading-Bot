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

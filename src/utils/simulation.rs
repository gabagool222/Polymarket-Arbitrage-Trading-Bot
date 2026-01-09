//! Simulation engine for test mode
//!
//! Simulates trade execution without real transactions.
//! Uses real-time price data from Polymarket and Binance.

use crate::config::AppConfig;
use crate::polymarket::types::{MarketPrices, OrderRequest, Side, TokenType};
use crate::trading::types::*;
use anyhow::Result;
use chrono::{DateTime, Duration, Utc};
use rust_decimal::Decimal;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{debug, info};

/// Virtual order in simulation
#[derive(Debug, Clone)]
pub struct VirtualOrder {
    pub id: String,
    pub token_id: String,
    pub side: Side,
    pub price: Decimal,
    pub size: Decimal,
    pub filled: Decimal,
    pub created_at: DateTime<Utc>,
    pub status: VirtualOrderStatus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VirtualOrderStatus {
    Open,
    PartiallyFilled,
    Filled,
    Cancelled,
}

/// Virtual position tracking
#[derive(Debug, Clone, Default)]
pub struct VirtualPosition {
    pub token_id: String,
    pub shares: Decimal,
    pub avg_price: Decimal,
    pub cost_basis: Decimal,
}

/// Simulation engine
pub struct SimulationEngine {
    config: Arc<AppConfig>,
    orders: Arc<RwLock<HashMap<String, VirtualOrder>>>,
    positions: Arc<RwLock<HashMap<String, VirtualPosition>>>,
    balance: Arc<RwLock<Decimal>>,
    pnl: Arc<RwLock<Decimal>>,
    trade_count: Arc<RwLock<u32>>,
}

impl SimulationEngine {
    /// Create a new simulation engine
    pub fn new(config: Arc<AppConfig>, initial_balance: Decimal) -> Self {
        Self {
            config,
            orders: Arc::new(RwLock::new(HashMap::new())),
            positions: Arc::new(RwLock::new(HashMap::new())),
            balance: Arc::new(RwLock::new(initial_balance)),
            pnl: Arc::new(RwLock::new(Decimal::ZERO)),
            trade_count: Arc::new(RwLock::new(0)),
        }
    }

    /// Simulate placing an order
    pub async fn place_order(&self, request: &OrderRequest) -> Result<VirtualOrder> {
        let order_id = uuid::Uuid::new_v4().to_string();

        let order = VirtualOrder {
            id: order_id.clone(),
            token_id: request.token_id.clone(),
            side: request.side,
            price: request.price,
            size: request.size,
            filled: Decimal::ZERO,
            created_at: Utc::now(),
            status: VirtualOrderStatus::Open,
        };

        info!(
            "[SIM] Order placed: {} {} {} @ {} (ID: {})",
            request.side, request.size, request.token_id, request.price, order_id
        );

        self.orders.write().await.insert(order_id.clone(), order.clone());

        // Increment trade count
        *self.trade_count.write().await += 1;

        Ok(order)
    }

    /// Simulate order execution based on market prices
    pub async fn update_orders(&self, market_prices: &MarketPrices) -> Vec<VirtualOrder> {
        let mut filled_orders = Vec::new();
        let mut orders = self.orders.write().await;

        for (order_id, order) in orders.iter_mut() {
            if order.status == VirtualOrderStatus::Filled {
                continue;
            }

            // Determine if order can be filled based on current prices
            let market_price = if order.token_id.contains("yes") || order.token_id.ends_with("_yes")
            {
                market_prices.yes_price
            } else {
                market_prices.no_price
            };

            let can_fill = match order.side {
                Side::Buy => order.price >= market_price,
                Side::Sell => order.price <= market_price,
            };

            if can_fill && order.status != VirtualOrderStatus::Filled {
                // Simulate immediate fill for simplicity
                order.filled = order.size;
                order.status = VirtualOrderStatus::Filled;

                info!(
                    "[SIM] Order filled: {} {} @ {} (market: {})",
                    order.id, order.size, order.price, market_price
                );

                // Update position
                self.update_position(order).await;

                filled_orders.push(order.clone());
            }
        }

        filled_orders
    }

    /// Update virtual position after fill
    async fn update_position(&self, order: &VirtualOrder) {
        let mut positions = self.positions.write().await;
        let mut balance = self.balance.write().await;

        let position = positions
            .entry(order.token_id.clone())
            .or_insert_with(|| VirtualPosition {
                token_id: order.token_id.clone(),
                shares: Decimal::ZERO,
                avg_price: Decimal::ZERO,
                cost_basis: Decimal::ZERO,
            });

        match order.side {
            Side::Buy => {
                let cost = order.price * order.filled;
                *balance -= cost;

                let new_shares = position.shares + order.filled;
                if new_shares > Decimal::ZERO {
                    position.avg_price =
                        (position.cost_basis + cost) / new_shares;
                }
                position.shares = new_shares;
                position.cost_basis += cost;
            }
            Side::Sell => {
                let proceeds = order.price * order.filled;
                *balance += proceeds;

                // Calculate realized PNL
                let cost_per_share = if position.shares > Decimal::ZERO {
                    position.cost_basis / position.shares
                } else {
                    Decimal::ZERO
                };
                let realized_pnl = (order.price - cost_per_share) * order.filled;

                *self.pnl.write().await += realized_pnl;

                position.shares -= order.filled;
                position.cost_basis -= cost_per_share * order.filled;

                debug!(
                    "[SIM] Realized PNL: {} (total: {})",
                    realized_pnl,
                    *self.pnl.read().await
                );
            }
        }
    }

    /// Simulate market resolution (for hedged positions)
    pub async fn resolve_market(&self, token_id: &str, winning_outcome: bool) -> Result<Decimal> {
        let mut positions = self.positions.write().await;
        let mut balance = self.balance.write().await;
        let mut pnl = self.pnl.write().await;

        let is_yes = token_id.contains("yes") || token_id.ends_with("_yes");
        let wins = (is_yes && winning_outcome) || (!is_yes && !winning_outcome);

        if let Some(position) = positions.get_mut(token_id) {
            let payout = if wins {
                position.shares * Decimal::ONE // $1 per share if win
            } else {
                Decimal::ZERO // $0 if lose
            };

            let realized = payout - position.cost_basis;
            *pnl += realized;
            *balance += payout;

            info!(
                "[SIM] Market resolved: {} {} | Payout: {} | PNL: {}",
                token_id,
                if wins { "WON" } else { "LOST" },
                payout,
                realized
            );

            // Clear position
            position.shares = Decimal::ZERO;
            position.cost_basis = Decimal::ZERO;

            return Ok(realized);
        }

        Ok(Decimal::ZERO)
    }

    /// Cancel an order
    pub async fn cancel_order(&self, order_id: &str) -> Result<()> {
        let mut orders = self.orders.write().await;

        if let Some(order) = orders.get_mut(order_id) {
            order.status = VirtualOrderStatus::Cancelled;
            info!("[SIM] Order cancelled: {}", order_id);
        }

        Ok(())
    }

    /// Get current balance
    pub async fn get_balance(&self) -> Decimal {
        *self.balance.read().await
    }

    /// Get total PNL
    pub async fn get_pnl(&self) -> Decimal {
        *self.pnl.read().await
    }

    /// Get trade count
    pub async fn get_trade_count(&self) -> u32 {
        *self.trade_count.read().await
    }

    /// Get all positions
    pub async fn get_positions(&self) -> HashMap<String, VirtualPosition> {
        self.positions.read().await.clone()
    }

    /// Get open orders
    pub async fn get_open_orders(&self) -> Vec<VirtualOrder> {
        self.orders
            .read()
            .await
            .values()
            .filter(|o| o.status == VirtualOrderStatus::Open)
            .cloned()
            .collect()
    }

    /// Print simulation summary
    pub async fn print_summary(&self) {
        let balance = self.get_balance().await;
        let pnl = self.get_pnl().await;
        let trades = self.get_trade_count().await;
        let positions = self.get_positions().await;

        info!("=== SIMULATION SUMMARY ===");
        info!("Balance: ${}", balance);
        info!("Total PNL: ${}", pnl);
        info!("Total Trades: {}", trades);
        info!("Open Positions: {}", positions.len());

        for (token_id, pos) in positions.iter() {
            if pos.shares > Decimal::ZERO {
                info!(
                    "  {} : {} shares @ {} avg (cost: {})",
                    token_id, pos.shares, pos.avg_price, pos.cost_basis
                );
            }
        }

        info!("==========================");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_simulation_buy() {
        let config = Arc::new(AppConfig::default());
        let engine = SimulationEngine::new(config, Decimal::new(1000, 0));

        let order = OrderRequest {
            token_id: "test_yes".to_string(),
            side: Side::Buy,
            price: Decimal::new(50, 2), // 0.50
            size: Decimal::new(10, 0),  // 10 shares
            order_type: crate::polymarket::types::OrderType::GoodTilCancelled,
            expiration: None,
        };

        let placed = engine.place_order(&order).await.unwrap();
        assert_eq!(placed.status, VirtualOrderStatus::Open);

        // Simulate market update that fills the order
        let prices = MarketPrices {
            condition_id: "test".to_string(),
            yes_price: Decimal::new(45, 2), // 0.45 (below our bid)
            no_price: Decimal::new(55, 2),
            yes_token_id: "test_yes".to_string(),
            no_token_id: "test_no".to_string(),
            timestamp: Utc::now(),
        };

        let filled = engine.update_orders(&prices).await;
        assert_eq!(filled.len(), 1);

        let balance = engine.get_balance().await;
        assert_eq!(balance, Decimal::new(9950, 1)); // 1000 - (10 * 0.50) = 995
    }
}

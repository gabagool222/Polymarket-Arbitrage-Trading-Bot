//! Trading engine - orchestrates strategies and execution

use crate::binance::PriceMove;
use crate::config::AppConfig;
use crate::polymarket::types::{Market, MarketPrices, OrderRequest, OrderType, Side, TokenType};
use crate::polymarket::PolymarketClient;
use crate::trading::strategy::StrategyManager;
use crate::trading::types::*;
use anyhow::{Context, Result};
use chrono::{Duration, Utc};
use rust_decimal::Decimal;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{broadcast, mpsc, RwLock};
use tracing::{debug, error, info, warn};

/// Trading engine state
pub struct TradingEngine {
    config: Arc<AppConfig>,
    client: Arc<PolymarketClient>,
    strategy: StrategyManager,
    positions: Arc<RwLock<HashMap<String, Position>>>,
    active_trades: Arc<RwLock<HashMap<String, HedgeTrade>>>,
    risk_state: Arc<RwLock<RiskState>>,
    daily_stats: Arc<RwLock<DailyStats>>,
    signal_tx: mpsc::Sender<Signal>,
    signal_rx: mpsc::Receiver<Signal>,
    /// Cached markets data with real clobTokenIds
    cached_markets: Arc<RwLock<HashMap<String, Market>>>,
}

impl TradingEngine {
    /// Create a new trading engine
    pub fn new(config: Arc<AppConfig>, client: Arc<PolymarketClient>) -> Self {
        let (signal_tx, signal_rx) = mpsc::channel(100);

        Self {
            strategy: StrategyManager::new(Arc::clone(&config)),
            config,
            client,
            positions: Arc::new(RwLock::new(HashMap::new())),
            active_trades: Arc::new(RwLock::new(HashMap::new())),
            risk_state: Arc::new(RwLock::new(RiskState::default())),
            daily_stats: Arc::new(RwLock::new(DailyStats::default())),
            signal_tx,
            signal_rx,
            cached_markets: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Update cached markets with fresh data from API
    pub async fn update_market_cache(&self, markets: Vec<Market>) {
        let mut cache = self.cached_markets.write().await;
        cache.clear();

        for market in markets {
            info!(
                "Caching market: {} with token IDs: {:?}",
                market.slug, market.clob_token_ids
            );
            cache.insert(market.condition_id.clone(), market);
        }

        info!("Market cache updated with {} markets", cache.len());
    }

    /// Get cached market by condition_id
    pub async fn get_cached_market(&self, condition_id: &str) -> Option<Market> {
        self.cached_markets.read().await.get(condition_id).cloned()
    }

    /// Process market price update
    pub async fn on_market_update(
        &self,
        prices: &MarketPrices,
        binance_move: Option<&PriceMove>,
    ) -> Result<()> {
        // Check risk limits
        if !self.can_trade().await {
            debug!("Trading paused due to risk limits");
            return Ok(());
        }

        // Check if within entry window
        if !self.is_within_entry_window() {
            debug!("Outside entry window, skipping");
            return Ok(());
        }

        // Get current position for this market
        let position = self
            .positions
            .read()
            .await
            .get(&prices.condition_id)
            .cloned();

        // Evaluate strategies
        let signals = self
            .strategy
            .evaluate(prices, binance_move, position.as_ref());

        // Process signals
        for signal in signals {
            if let Err(e) = self.process_signal(signal).await {
                error!("Failed to process signal: {}", e);
            }
        }

        // Check active trades for timeout/completion
        self.check_active_trades(prices).await?;

        Ok(())
    }

    /// Check if trading is allowed based on risk limits
    async fn can_trade(&self) -> bool {
        let risk = self.risk_state.read().await;

        if risk.is_paused {
            warn!("Trading paused: {:?}", risk.pause_reason);
            return false;
        }

        if risk.in_cooldown {
            if let Some(until) = risk.cooldown_until {
                if Utc::now() < until {
                    debug!("In cooldown until {}", until);
                    return false;
                }
            }
        }

        if risk.consecutive_losses >= self.config.risk.max_consecutive_losses {
            warn!(
                "Max consecutive losses ({}) reached",
                risk.consecutive_losses
            );
            return false;
        }

        let max_loss = self.config.trading.max_daily_risk_pct;
        if risk.daily_loss > max_loss {
            warn!("Daily loss limit exceeded: {} > {}", risk.daily_loss, max_loss);
            return false;
        }

        true
    }

    /// Check if within the entry window (first N minutes of round)
    fn is_within_entry_window(&self) -> bool {
        // For 15-min markets, rounds start at :00, :15, :30, :45
        let now = Utc::now();
        let minute = now.minute();
        let round_minute = minute % 15;

        round_minute < self.config.trading.entry_window_min
    }

    /// Process a trading signal
    async fn process_signal(&self, signal: Signal) -> Result<()> {
        info!(
            "Processing signal: {} for {} (confidence: {:.2})",
            signal.signal_type, signal.market_id, signal.confidence
        );

        match signal.signal_type {
            SignalType::HedgeEntry | SignalType::LatencyEntry => {
                self.open_trade(signal).await?;
            }
            SignalType::AverageDown => {
                self.average_down(signal).await?;
            }
            SignalType::HedgeExit | SignalType::StopLoss => {
                self.close_position(signal).await?;
            }
        }

        Ok(())
    }

    /// Open a new hedge trade
    async fn open_trade(&self, signal: Signal) -> Result<()> {
        let is_simulated = self.config.is_test_mode();

        // Resolve token ID from signal or fallback
        let token_id = if let Some(id) = &signal.token_id {
            id.clone()
        } else {
            warn!("Signal missing token_id, using fallback");
            self.get_token_id(&signal.market_id, signal.token_type).await?
        };

        // Create order request
        let order = OrderRequest {
            token_id,
            side: Side::Buy,
            price: signal.suggested_price,
            size: signal.suggested_size,
            order_type: OrderType::GoodTilCancelled,
            expiration: None,
        };

        // Log simulation or execute
        if is_simulated {
            info!(
                "[SIMULATION] Would buy {} {} shares at {} for market {}",
                signal.token_type, order.size, order.price, signal.market_id
            );
        }

        // Place order (client handles test mode internally)
        let placed_order = self.client.place_order(&order).await?;

        // Create hedge trade record
        let trade_id = uuid::Uuid::new_v4().to_string();
        let timeout_secs = self.config.trading.max_legs_timeout_sec as i64;

        let trade = HedgeTrade {
            id: trade_id.clone(),
            market_id: signal.market_id.clone(),
            asset: self.extract_asset(&signal.market_id),
            leg1: TradeLeg {
                order_id: Some(placed_order.id),
                token_id: order.token_id,
                token_type: signal.token_type,
                side: Side::Buy,
                price: order.price,
                size: order.size,
                filled_size: Decimal::ZERO,
                status: LegStatus::Open,
                filled_at: None,
            },
            leg2: None,
            status: HedgeStatus::Leg1Pending,
            entry_reason: signal.reason,
            created_at: Utc::now(),
            timeout_at: Utc::now() + Duration::seconds(timeout_secs),
            closed_at: None,
            pnl: None,
            is_simulated,
        };

        // Store trade
        self.active_trades
            .write()
            .await
            .insert(trade_id.clone(), trade);

        info!(
            "Opened {} trade {}: {} {} @ {}",
            signal.signal_type,
            trade_id,
            signal.token_type,
            signal.suggested_size,
            signal.suggested_price
        );

        Ok(())
    }

    /// Average down on existing position
    async fn average_down(&self, signal: Signal) -> Result<()> {
        // Resolve token ID
        let token_id = if let Some(id) = &signal.token_id {
            id.clone()
        } else {
            self.get_token_id(&signal.market_id, signal.token_type).await?
        };

        let order = OrderRequest {
            token_id,
            side: Side::Buy,
            price: signal.suggested_price,
            size: signal.suggested_size,
            order_type: OrderType::GoodTilCancelled,
            expiration: None,
        };

        if self.config.is_test_mode() {
            info!(
                "[SIMULATION] Would average down: buy {} {} shares at {}",
                signal.token_type, order.size, order.price
            );
        }

        self.client.place_order(&order).await?;

        // Update position average
        let mut positions = self.positions.write().await;
        if let Some(pos) = positions.get_mut(&signal.market_id) {
            match signal.token_type {
                TokenType::Yes => {
                    let total_cost =
                        pos.yes_avg_price * pos.yes_shares + order.price * order.size;
                    pos.yes_shares += order.size;
                    pos.yes_avg_price = total_cost / pos.yes_shares;
                }
                TokenType::No => {
                    let total_cost =
                        pos.no_avg_price * pos.no_shares + order.price * order.size;
                    pos.no_shares += order.size;
                    pos.no_avg_price = total_cost / pos.no_shares;
                }
            }
        }

        Ok(())
    }

    /// Close a position
    async fn close_position(&self, signal: Signal) -> Result<()> {
        let positions = self.positions.read().await;
        let position = positions
            .get(&signal.market_id)
            .context("No position to close")?;

        // Sell the side we have more of
        let (token_type, size) = if position.yes_shares > position.no_shares {
            (TokenType::Yes, position.yes_shares)
        } else {
            (TokenType::No, position.no_shares)
        };

        drop(positions);

        let order = OrderRequest {
            token_id: self.get_token_id(&signal.market_id, token_type).await?,
            side: Side::Sell,
            price: signal.suggested_price,
            size,
            order_type: OrderType::GoodTilCancelled,
            expiration: None,
        };

        if self.config.is_test_mode() {
            info!(
                "[SIMULATION] Would close position: sell {} {} shares at {}",
                token_type, size, order.price
            );
        }

        self.client.place_order(&order).await?;

        Ok(())
    }

    /// Check active trades for completion or timeout
    /// FIX: Added fill detection for Leg1 to trigger Leg2 placement
    async fn check_active_trades(&self, prices: &MarketPrices) -> Result<()> {
        let mut trades = self.active_trades.write().await;
        let now = Utc::now();

        let trade_ids: Vec<_> = trades.keys().cloned().collect();

        for trade_id in trade_ids {
            if let Some(trade) = trades.get_mut(&trade_id) {
                // Skip if not for this market
                if trade.market_id != prices.condition_id {
                    continue;
                }

                // Check timeout
                if now > trade.timeout_at && trade.status != HedgeStatus::Complete {
                    warn!("Trade {} timed out", trade_id);
                    trade.status = HedgeStatus::TimedOut;
                    trade.closed_at = Some(now);

                    // Cancel any open orders
                    if let Some(ref order_id) = trade.leg1.order_id {
                        let _ = self.client.cancel_order(order_id).await;
                    }
                    if let Some(ref leg2) = trade.leg2 {
                        if let Some(ref order_id) = leg2.order_id {
                            let _ = self.client.cancel_order(order_id).await;
                        }
                    }
                    continue;
                }

                // FIX: Check if Leg1 is filled (for pending Leg1 trades)
                if trade.status == HedgeStatus::Leg1Pending {
                    let leg1_filled = self.check_leg_fill_status(trade, true).await;

                    if leg1_filled {
                        trade.leg1.status = LegStatus::Filled;
                        trade.leg1.filled_size = trade.leg1.size;
                        trade.leg1.filled_at = Some(now);
                        trade.status = HedgeStatus::Leg1Filled;
                        info!(
                            "Trade {} Leg1 FILLED: {} {} @ {}",
                            trade_id, trade.leg1.token_type, trade.leg1.filled_size, trade.leg1.price
                        );
                    }
                }

                // Check if Leg 1 filled and we need to place Leg 2
                if trade.status == HedgeStatus::Leg1Filled && trade.leg2.is_none() {
                    if let Some((token_type, price, size)) = self.calculate_leg2(trade, prices) {
                        let order = OrderRequest {
                            token_id: self.get_token_id(&trade.market_id, token_type).await?,
                            side: Side::Buy,
                            price,
                            size,
                            order_type: OrderType::GoodTilCancelled,
                            expiration: None,
                        };

                        match self.client.place_order(&order).await {
                            Ok(placed) => {
                                trade.leg2 = Some(TradeLeg {
                                    order_id: Some(placed.id),
                                    token_id: order.token_id,
                                    token_type,
                                    side: Side::Buy,
                                    price,
                                    size,
                                    filled_size: Decimal::ZERO,
                                    status: LegStatus::Open,
                                    filled_at: None,
                                });
                                trade.status = HedgeStatus::Leg2Pending;
                                info!("Placed Leg 2 for trade {}", trade_id);
                            }
                            Err(e) => {
                                error!("Failed to place Leg 2: {}", e);
                            }
                        }
                    }
                }

                // FIX: Check if Leg2 is filled (for pending Leg2 trades)
                if trade.status == HedgeStatus::Leg2Pending && trade.leg2.is_some() {
                    let leg2_filled = self.check_leg_fill_status(trade, false).await;

                    if leg2_filled {
                        if let Some(ref mut leg2) = trade.leg2 {
                            leg2.status = LegStatus::Filled;
                            leg2.filled_size = leg2.size;
                            leg2.filled_at = Some(now);
                        }
                        trade.status = HedgeStatus::Complete;
                        trade.closed_at = Some(now);

                        // Calculate PNL: guaranteed $1 payout - (leg1 cost + leg2 cost)
                        let leg1_cost = trade.leg1.price * trade.leg1.filled_size;
                        let leg2_cost = trade.leg2.as_ref()
                            .map(|l| l.price * l.filled_size)
                            .unwrap_or(Decimal::ZERO);
                        let total_cost = leg1_cost + leg2_cost;
                        let payout = trade.leg1.filled_size.min(
                            trade.leg2.as_ref().map(|l| l.filled_size).unwrap_or(Decimal::ZERO)
                        );
                        let pnl = payout - total_cost;
                        trade.pnl = Some(pnl);

                        info!(
                            "🎉 Trade {} COMPLETE! Leg2 filled. PNL: ${:.4} (cost: ${:.4}, payout: ${:.4})",
                            trade_id, pnl, total_cost, payout
                        );
                    }
                }
            }
        }

        Ok(())
    }

    /// Check if a leg's order has been filled
    /// FIX: This function checks fill status via API or simulates fill in test mode
    async fn check_leg_fill_status(&self, trade: &HedgeTrade, is_leg1: bool) -> bool {
        let leg = if is_leg1 { &trade.leg1 } else {
            match &trade.leg2 {
                Some(l) => l,
                None => return false,
            }
        };

        // Skip if already filled
        if leg.status == LegStatus::Filled {
            return true;
        }

        // In test mode, simulate fill after a short delay
        // (in reality we'd check if price crossed our limit)
        if trade.is_simulated {
            // Simulate: assume fill after 2 seconds for testing
            let time_since_created = Utc::now() - trade.created_at;
            if time_since_created.num_seconds() >= 2 {
                info!(
                    "[SIMULATION] Simulating {} fill for trade {}",
                    if is_leg1 { "Leg1" } else { "Leg2" },
                    trade.id
                );
                return true;
            }
            return false;
        }

        // Real mode: check fills via API
        if let Some(ref order_id) = leg.order_id {
            match self.client.get_fills(Some(&trade.market_id)).await {
                Ok(fills) => {
                    // Check if any fill matches our order
                    let total_filled: Decimal = fills
                        .iter()
                        .filter(|f| f.order_id == *order_id)
                        .map(|f| f.size)
                        .sum();

                    if total_filled >= leg.size {
                        info!(
                            "Order {} fully filled: {} / {}",
                            order_id, total_filled, leg.size
                        );
                        return true;
                    } else if total_filled > Decimal::ZERO {
                        debug!(
                            "Order {} partially filled: {} / {}",
                            order_id, total_filled, leg.size
                        );
                    }
                }
                Err(e) => {
                    warn!("Failed to fetch fills for order {}: {}", order_id, e);
                }
            }
        }

        false
    }

    /// Calculate Leg 2 parameters
    fn calculate_leg2(
        &self,
        trade: &HedgeTrade,
        prices: &MarketPrices,
    ) -> Option<(TokenType, Decimal, Decimal)> {
        let leg1 = &trade.leg1;

        // Leg 2 is opposite of Leg 1
        let (token_type, price) = match leg1.token_type {
            TokenType::Yes => (TokenType::No, prices.no_price),
            TokenType::No => (TokenType::Yes, prices.yes_price),
        };

        // Match Leg 1 size for perfect hedge
        let size = leg1.filled_size;

        // Verify profitability
        let total_cost = leg1.price + price;
        if total_cost >= Decimal::ONE {
            warn!(
                "Leg 2 would not be profitable: {} + {} = {}",
                leg1.price, price, total_cost
            );
            return None;
        }

        Some((token_type, price, size))
    }

    /// Get token ID for a market and outcome
    /// FIX: Now uses real clobTokenIds from cached market data
    async fn get_token_id(&self, market_id: &str, token_type: TokenType) -> Result<String> {
        let cache = self.cached_markets.read().await;

        if let Some(market) = cache.get(market_id) {
            // clobTokenIds: index 0 = Yes/Up, index 1 = No/Down
            let token_id = match token_type {
                TokenType::Yes => market.clob_token_ids.get(0),
                TokenType::No => market.clob_token_ids.get(1),
            };

            if let Some(id) = token_id {
                debug!("Got token_id for {} {:?}: {}", market_id, token_type, id);
                return Ok(id.clone());
            }

            // Fallback to tokens array if clobTokenIds not available
            let token = market.tokens.iter().find(|t| {
                let outcome = t.outcome.to_lowercase();
                match token_type {
                    TokenType::Yes => outcome == "yes" || outcome == "up",
                    TokenType::No => outcome == "no" || outcome == "down",
                }
            });

            if let Some(t) = token {
                debug!("Got token_id from tokens array for {} {:?}: {}", market_id, token_type, t.token_id);
                return Ok(t.token_id.clone());
            }

            anyhow::bail!(
                "Token {} not found in market {}. Available: {:?}",
                token_type,
                market_id,
                market.clob_token_ids
            );
        }

        anyhow::bail!(
            "Market {} not in cache. Call update_market_cache() first. Cache has {} markets.",
            market_id,
            cache.len()
        );
    }

    /// Extract asset name from market ID
    fn extract_asset(&self, market_id: &str) -> String {
        let lower = market_id.to_lowercase();
        if lower.contains("btc") {
            "BTC".to_string()
        } else if lower.contains("eth") {
            "ETH".to_string()
        } else if lower.contains("sol") {
            "SOL".to_string()
        } else {
            "UNKNOWN".to_string()
        }
    }

    /// Record trade result
    pub async fn record_trade_result(&self, trade_id: &str, pnl: Decimal) -> Result<()> {
        let mut stats = self.daily_stats.write().await;
        let mut risk = self.risk_state.write().await;

        stats.trades_count += 1;
        stats.gross_pnl += pnl;
        stats.net_pnl += pnl; // TODO: deduct fees

        if pnl > Decimal::ZERO {
            stats.wins += 1;
            risk.consecutive_losses = 0;
            if stats.best_trade.map(|b| pnl > b).unwrap_or(true) {
                stats.best_trade = Some(pnl);
            }
        } else {
            stats.losses += 1;
            risk.consecutive_losses += 1;
            risk.daily_loss += pnl.abs();
            if stats.worst_trade.map(|w| pnl < w).unwrap_or(true) {
                stats.worst_trade = Some(pnl);
            }

            // Check cooldown
            if risk.consecutive_losses >= 2 {
                risk.in_cooldown = true;
                risk.cooldown_until =
                    Some(Utc::now() + Duration::seconds(self.config.risk.loss_cooldown_sec as i64));
                warn!(
                    "Entering cooldown after {} consecutive losses",
                    risk.consecutive_losses
                );
            }
        }

        if risk.consecutive_losses > stats.max_consecutive_losses {
            stats.max_consecutive_losses = risk.consecutive_losses;
        }

        info!(
            "Trade {} result: PNL={}, Total: W{}/L{}, Net PNL: {}",
            trade_id, pnl, stats.wins, stats.losses, stats.net_pnl
        );

        Ok(())
    }

    /// Get current daily stats
    pub async fn get_daily_stats(&self) -> DailyStats {
        self.daily_stats.read().await.clone()
    }

    /// Pause trading
    pub async fn pause(&self, reason: &str) {
        let mut risk = self.risk_state.write().await;
        risk.is_paused = true;
        risk.pause_reason = Some(reason.to_string());
        warn!("Trading paused: {}", reason);
    }

    /// Resume trading
    pub async fn resume(&self) {
        let mut risk = self.risk_state.write().await;
        risk.is_paused = false;
        risk.pause_reason = None;
        info!("Trading resumed");
    }

    /// Reset daily stats (call at start of new day)
    pub async fn reset_daily_stats(&self) {
        let mut stats = self.daily_stats.write().await;
        let mut risk = self.risk_state.write().await;

        *stats = DailyStats {
            date: Utc::now().format("%Y-%m-%d").to_string(),
            ..Default::default()
        };

        risk.daily_loss = Decimal::ZERO;
        risk.consecutive_losses = 0;
        risk.in_cooldown = false;
        risk.cooldown_until = None;

        info!("Daily stats reset for {}", stats.date);
    }
}

use chrono::Timelike;

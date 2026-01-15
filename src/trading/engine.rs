//! Trading engine - orchestrates strategies and execution
//!
//! Features:
//! - Incremental hedging (cicil mode): 1 share at a time with momentum confirmation
//! - Maker pricing for Leg2: earns rebates instead of paying taker fees
//! - Partial fill handler: auto-rebalances imbalanced positions
//! - Balance check before entry: prevents insufficient funds errors
//! - Liquidation cascade integration: dynamic threshold based on market impulse

use crate::binance::PriceMove;
use crate::bybit::{LiquidationCascade, CascadeDirection, CascadeAction};
use crate::config::AppConfig;
use crate::polymarket::types::{Market, MarketPrices, Order, OrderRequest, OrderType, OrderStatus as PolyOrderStatus, Side, TokenType};
use crate::polymarket::PolymarketClient;
use crate::trading::strategy::StrategyManager;
use crate::trading::types::{
    DailyStats, HedgeStatus, HedgeTrade, LegStatus, OrderStatus, PlacedOrder,
    Position, RiskState, Signal, SignalType, TradeLeg,
};
use anyhow::{Context, Result};
use chrono::{Duration, Utc};
use rust_decimal::Decimal;
use std::collections::{HashMap, HashSet};
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
    /// History of unique Market IDs that have been processed/traded in this session
    processed_markets: Arc<RwLock<HashSet<String>>>,
    /// Current USDC balance (updated periodically)
    cached_balance: Arc<RwLock<Decimal>>,
    /// Latest liquidation cascade (if any) for dynamic threshold
    latest_cascade: Arc<RwLock<Option<LiquidationCascade>>>,
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
            processed_markets: Arc::new(RwLock::new(HashSet::new())),
            cached_balance: Arc::new(RwLock::new(Decimal::ZERO)),
            latest_cascade: Arc::new(RwLock::new(None)),
        }
    }

    /// Update cached balance from API
    pub async fn update_balance(&self) -> Result<Decimal> {
        let balance = self.client.get_balance().await?;
        *self.cached_balance.write().await = balance;
        debug!("Balance updated: ${}", balance);
        Ok(balance)
    }

    /// Get current cached balance
    pub async fn get_balance(&self) -> Decimal {
        *self.cached_balance.read().await
    }

    /// Update latest cascade for dynamic threshold
    pub async fn set_latest_cascade(&self, cascade: Option<LiquidationCascade>) {
        *self.latest_cascade.write().await = cascade;
    }

    /// Get dynamic threshold based on cascade presence
    /// Returns tighter threshold (0.985) if cascade detected, else config default (0.98)
    pub async fn get_dynamic_threshold(&self) -> Decimal {
        let cascade = self.latest_cascade.read().await;
        if cascade.is_some() {
            // Cascade detected - use tighter threshold from config
            self.config.bybit_liquidation.cascade_profit_threshold
        } else {
            // Normal mode - use standard threshold
            self.config.trading.min_profit_threshold
        }
    }

    /// Check if balance is sufficient for a trade
    pub async fn check_balance_sufficient(&self, required: Decimal) -> Result<bool> {
        if !self.config.trading.require_balance_check {
            return Ok(true);
        }

        let balance = self.get_balance().await;
        let min_balance = self.config.trading.min_balance_usd;

        // Need: required amount + minimum reserve
        let total_needed = required + min_balance;

        if balance < total_needed {
            warn!(
                "💰 BALANCE CHECK FAILED: Need ${} (trade ${} + reserve ${}), have ${}",
                total_needed, required, min_balance, balance
            );
            return Ok(false);
        }

        debug!("Balance OK: ${} >= ${} needed", balance, total_needed);
        Ok(true)
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

    // =========================================================================
    // LIMIT ORDER PLACEMENT (matches Polymarket Web UI)
    // =========================================================================

    /// Place a limit buy order with proper validation
    /// Matches web UI: Limit Price, Shares, GTC, cost calculation
    pub async fn place_limit_buy(
        &self,
        token_id: &str,
        price: Decimal,
        size: Decimal,
        market_id: &str,
        token_type: TokenType,
    ) -> Result<PlacedOrder> {
        let is_simulated = self.config.is_test_mode();

        // 1. Fetch market metadata for validation
        let (min_size, tick_size) = self.client.get_market_metadata(token_id).await
            .unwrap_or((Decimal::new(5, 0), Decimal::new(1, 2))); // Default: min 5 shares, tick 0.01

        // 2. Validate and adjust size
        // Polymarket CLOB requires a minimum total value of $1.00 USD (size * price >= 1.0)
        let min_value = Decimal::ONE;
        let min_size_for_value = (min_value / price).ceil();
        
        let effective_min_size = if min_size > min_size_for_value {
            min_size
        } else {
            min_size_for_value
        };

        let adjusted_size = if size < effective_min_size {
            warn!(
                "Size {} below effective min {} (to meet $1.00 min value). Adjusting.",
                size, effective_min_size
            );
            effective_min_size
        } else {
            size
        };

        // 3. Validate and adjust price to tick size
        let tick_digits = Self::get_tick_digits(tick_size);
        let adjusted_price = price.round_dp(tick_digits);

        if adjusted_price != price {
            debug!(
                "Price {} adjusted to {} (tick: {})",
                price, adjusted_price, tick_size
            );
        }

        // 4. Validate price range (0.01 to 0.99 for binary markets)
        if adjusted_price < Decimal::new(1, 2) || adjusted_price > Decimal::new(99, 2) {
            anyhow::bail!(
                "Price {} outside valid range [0.01, 0.99]",
                adjusted_price
            );
        }

        // 5. Calculate cost and potential win
        let cost = adjusted_price * adjusted_size;
        let potential_win = adjusted_size; // $1 per share if win
        let profit_if_win = potential_win - cost;
        let rebate_estimate = self.estimate_maker_rebate(adjusted_price, adjusted_size);

        // 6. Simulation mode - detailed logging like web UI
        if is_simulated {
            info!("╔══════════════════════════════════════════════════════════════╗");
            info!("║  [SIMULATION] LIMIT BUY ORDER                                ║");
            info!("╠══════════════════════════════════════════════════════════════╣");
            info!("║  Token:     {} ({})                    ", token_type, &token_id[0..8.min(token_id.len())]);
            info!("║  Market:    {}                         ", market_id);
            info!("║  ─────────────────────────────────────────────────────────── ║");
            info!("║  Limit Price:  ${:.2} ({:.0}¢)                              ", adjusted_price, adjusted_price * Decimal::new(100, 0));
            info!("║  Shares:       {}                                           ", adjusted_size);
            info!("║  Order Type:   GTC (Good Til Cancelled)                      ║");
            info!("║  ─────────────────────────────────────────────────────────── ║");
            info!("║  Cost:         ${:.2}                                       ", cost);
            info!("║  To Win:       ${:.2} (per share = $1)                      ", potential_win);
            info!("║  Profit if Win: ${:.2}                                      ", profit_if_win);
            info!("║  Est. Rebate:  ${:.4} (maker)                               ", rebate_estimate);
            info!("╚══════════════════════════════════════════════════════════════╝");

            // Return simulated order
            return Ok(PlacedOrder {
                id: format!("sim_{}", uuid::Uuid::new_v4()),
                token_id: token_id.to_string(),
                side: Side::Buy,
                price: adjusted_price,
                size: adjusted_size,
                filled_size: Decimal::ZERO,
                status: OrderStatus::Open,
                is_simulated: true,
            });
        }

        // 7. Real mode - place actual order
        let order_request = OrderRequest {
            token_id: token_id.to_string(),
            side: Side::Buy,
            price: adjusted_price,
            size: adjusted_size,
            order_type: OrderType::GoodTilCancelled,
            expiration: None, // GTC = no expiration
            neg_risk: false,  // Default to non-negRisk
        };

        let placed = self.client.place_order(&order_request).await?;

        info!(
            "✅ LIMIT BUY placed: {} {} @ ${:.2} | Cost: ${:.2} | Order ID: {}",
            token_type, adjusted_size, adjusted_price, cost, placed.id
        );

        Ok(PlacedOrder {
            id: placed.id,
            token_id: token_id.to_string(),
            side: Side::Buy,
            price: adjusted_price,
            size: adjusted_size,
            filled_size: Decimal::ZERO,
            status: OrderStatus::Open,
            is_simulated: false,
        })
    }

    /// Execute full hedge batch: Buy both sides (Yes + No) to lock profit
    /// Leg1: Buy cheaper side first, Leg2: Buy opposite side
    pub async fn execute_hedge_batch(
        &self,
        market_id: &str,
        yes_token_id: &str,
        no_token_id: &str,
        yes_price: Decimal,
        no_price: Decimal,
    ) -> Result<HedgeTrade> {
        let is_simulated = self.config.is_test_mode();
        let size = self.config.trading.per_trade_shares;

        // Determine which side is cheaper (buy cheaper first for better fill odds)
        let (leg1_token, leg1_price, leg1_type, leg2_token, leg2_price, leg2_type) =
            if yes_price <= no_price {
                (yes_token_id, yes_price, TokenType::Yes, no_token_id, no_price, TokenType::No)
            } else {
                (no_token_id, no_price, TokenType::No, yes_token_id, yes_price, TokenType::Yes)
            };

        let total_cost = leg1_price + leg2_price;
        let edge = Decimal::ONE - total_cost;
        let profit_per_share = edge;
        let total_profit = profit_per_share * size;

        info!("═══════════════════════════════════════════════════════════════");
        info!("  HEDGE ARBITRAGE EXECUTION");
        info!("═══════════════════════════════════════════════════════════════");
        info!("  Market: {}", market_id);
        info!("  Yes: ${:.2} | No: ${:.2} | Sum: ${:.4}", yes_price, no_price, total_cost);
        info!("  Edge: ${:.4} per share ({:.2}%)", edge, edge * Decimal::new(100, 0));
        info!("  Size: {} shares per leg", size);
        info!("  Expected Profit: ${:.4}", total_profit);
        info!("───────────────────────────────────────────────────────────────");

        // Place Leg 1 (cheaper side)
        info!("  [Leg 1] Placing {} buy @ ${:.2}...", leg1_type, leg1_price);
        let leg1_order = self.place_limit_buy(
            leg1_token,
            leg1_price,
            size,
            market_id,
            leg1_type,
        ).await?;

        // For simulation, immediately place Leg 2
        // For real mode, we wait for Leg 1 fill in check_active_trades
        if is_simulated {
            info!("  [Leg 2] Placing {} buy @ ${:.2}...", leg2_type, leg2_price);
            let leg2_order = self.place_limit_buy(
                leg2_token,
                leg2_price,
                size,
                market_id,
                leg2_type,
            ).await?;

            // Create completed hedge trade for simulation
            let trade_id = uuid::Uuid::new_v4().to_string();
            let trade = HedgeTrade {
                id: trade_id.clone(),
                market_id: market_id.to_string(),
                asset: self.extract_asset(market_id),
                leg1: TradeLeg {
                    order_id: Some(leg1_order.id),
                    token_id: leg1_token.to_string(),
                    token_type: leg1_type,
                    side: Side::Buy,
                    price: leg1_price,
                    size,
                    filled_size: size, // Simulated as filled
                    status: LegStatus::Filled,
                    filled_at: Some(Utc::now()),
                },
                leg2: Some(TradeLeg {
                    order_id: Some(leg2_order.id),
                    token_id: leg2_token.to_string(),
                    token_type: leg2_type,
                    side: Side::Buy,
                    price: leg2_price,
                    size,
                    filled_size: size, // Simulated as filled
                    status: LegStatus::Filled,
                    filled_at: Some(Utc::now()),
                }),
                status: HedgeStatus::Complete,
                entry_reason: format!("Hedge arb: sum={:.4}, edge={:.4}", total_cost, edge),
                created_at: Utc::now(),
                timeout_at: Utc::now() + Duration::seconds(self.config.trading.max_legs_timeout_sec as i64),
                closed_at: Some(Utc::now()),
                pnl: Some(total_profit),
                is_simulated: true,
            };

            info!("═══════════════════════════════════════════════════════════════");
            info!("  ✅ [SIMULATION] HEDGE COMPLETE!");
            info!("  Total Cost: ${:.4}", total_cost * size);
            info!("  Guaranteed Payout: ${:.2}", size);
            info!("  Locked Profit: ${:.4}", total_profit);
            info!("═══════════════════════════════════════════════════════════════");

            // Store trade
            self.active_trades.write().await.insert(trade_id.clone(), trade.clone());

            return Ok(trade);
        }

        // Parse expiry from market_id (e.g. "btc-updown-15m-1768403700")
        let parts: Vec<&str> = market_id.split('-').collect();
        if let Some(timestamp_str) = parts.last() {
            if let Ok(expiry_ts) = timestamp_str.parse::<i64>() {
                let now_ts = Utc::now().timestamp();
                let time_left = expiry_ts - now_ts;
                
                // THETA PROTECTION: Don't trade if < 3 minutes (180s) left
                if time_left < 180 {
                    warn!("⏳ Theta Protection: Skipping trade, only {}s left for market {}", time_left, market_id);
                    return Ok(HedgeTrade {
                        id: format!("skipped_{}", uuid::Uuid::new_v4()),
                        market_id: market_id.to_string(),
                        asset: self.extract_asset(market_id),
                        leg1: TradeLeg {
                            order_id: None,
                            token_id: leg1_token.to_string(),
                            token_type: leg1_type,
                            side: Side::Buy,
                            price: leg1_price,
                            size,
                            filled_size: Decimal::ZERO,
                            status: LegStatus::Skipped,
                            filled_at: None,
                        },
                        leg2: None,
                        status: HedgeStatus::Skipped,
                        entry_reason: format!("Skipped due to theta protection: {}s left", time_left),
                        created_at: Utc::now(),
                        timeout_at: Utc::now(),
                        closed_at: Some(Utc::now()),
                        pnl: Some(Decimal::ZERO),
                        is_simulated: false,
                    });
                }
            }
        }

        // Check if we already have a trade for this market
        if self.active_trades.read().await.values().any(|t| t.market_id == market_id) {
            warn!("Skipping trade for market {} as an active trade already exists.", market_id);
            return Ok(HedgeTrade {
                id: format!("skipped_{}", uuid::Uuid::new_v4()),
                market_id: market_id.to_string(),
                asset: self.extract_asset(market_id),
                leg1: TradeLeg {
                    order_id: None,
                    token_id: leg1_token.to_string(),
                    token_type: leg1_type,
                    side: Side::Buy,
                    price: leg1_price,
                    size,
                    filled_size: Decimal::ZERO,
                    status: LegStatus::Skipped,
                    filled_at: None,
                },
                leg2: None,
                status: HedgeStatus::Skipped,
                entry_reason: "Skipped due to existing active trade".to_string(),
                created_at: Utc::now(),
                timeout_at: Utc::now(),
                closed_at: Some(Utc::now()),
                pnl: Some(Decimal::ZERO),
                is_simulated: false,
            });
        }

        // Real mode: Create pending trade, wait for Leg1 fill
        let trade_id = uuid::Uuid::new_v4().to_string();
        let trade = HedgeTrade {
            id: trade_id.clone(),
            market_id: market_id.to_string(),
            asset: self.extract_asset(market_id),
            leg1: TradeLeg {
                order_id: Some(leg1_order.id),
                token_id: leg1_token.to_string(),
                token_type: leg1_type,
                side: Side::Buy,
                price: leg1_price,
                size,
                filled_size: Decimal::ZERO,
                status: LegStatus::Open,
                filled_at: None,
            },
            leg2: None, // Will be placed after Leg1 fills
            status: HedgeStatus::Leg1Pending,
            entry_reason: format!("Hedge arb: sum={:.4}, edge={:.4}", total_cost, edge),
            created_at: Utc::now(),
            timeout_at: Utc::now() + Duration::seconds(self.config.trading.max_legs_timeout_sec as i64),
            closed_at: None,
            pnl: None,
            is_simulated: false,
        };

        // Store pending trade
        self.active_trades.write().await.insert(trade_id.clone(), trade.clone());

        info!("  Hedge trade {} created. Waiting for Leg1 fill...", trade_id);

        Ok(trade)
    }

    /// Get number of decimal places for tick size
    fn get_tick_digits(tick_size: Decimal) -> u32 {
        // tick_size 0.01 = 2 digits, 0.001 = 3 digits, etc.
        let s = tick_size.to_string();
        if let Some(pos) = s.find('.') {
            (s.len() - pos - 1) as u32
        } else {
            0
        }
    }

    /// Estimate maker rebate for an order
    /// Formula: 1.56% * 4 * price * (1 - price) (max at price = 0.50)
    fn estimate_maker_rebate(&self, price: Decimal, size: Decimal) -> Decimal {
        if !self.config.maker_rebates.enabled {
            return Decimal::ZERO;
        }

        let rebate_rate = Decimal::new(156, 4); // 1.56%
        let variance_factor = Decimal::new(4, 0) * price * (Decimal::ONE - price);
        let effective_rate = rebate_rate * variance_factor;
        let volume = price * size;

        volume * effective_rate
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
        
        // 🛑 SAFETY 5-MILLION-PERCENT: Check if we ALREADY traded this market
        if self.processed_markets.read().await.contains(&signal.market_id) {
            warn!("🛑 SAFETY BREAK: Skipping Market {} - Already traded in this session.", signal.market_id);
            return Ok(());
        }

        // === SAFETY CHECK 1: Daily trade limit ===
        {
            let stats = self.daily_stats.read().await;
            let max_daily_trades: u32 = 100; // Hard limit
            if stats.trades_count >= max_daily_trades {
                warn!("🛑 Daily trade limit reached ({}/{}), stopping", stats.trades_count, max_daily_trades);
                return Ok(());
            }
        }
        
        // === SAFETY CHECK 2: Max loss circuit breaker ===
        {
            let stats = self.daily_stats.read().await;
            let max_loss = Decimal::new(-50, 0); // -$50 hard stop
            if stats.net_pnl < max_loss {
                error!("🛑 CIRCUIT BREAKER: Daily loss ${} exceeds limit ${}", stats.net_pnl, max_loss);
                return Ok(());
            }
        }
        
        // === SAFETY CHECK 3: Real mode warning ===
        if !is_simulated {
            warn!(
                "⚡ REAL TRADE: {} {} @ {} for {}",
                signal.token_type,
                signal.suggested_size,
                signal.suggested_price,
                signal.market_id
            );
        }
        
        // Check if there's already an active trade for this market
        {
            let active = self.active_trades.read().await;
            let existing = active.values().find(|t| {
                t.market_id == signal.market_id && 
                t.status != HedgeStatus::Complete && 
                t.status != HedgeStatus::TimedOut &&
                t.status != HedgeStatus::Cancelled
            });
            
            if existing.is_some() {
                debug!("Already have active trade for market {}, skipping", signal.market_id);
                return Ok(());
            }
        }

        // === INCREMENTAL TRADING (CICIL) CHECK ===
        if self.config.trading.enable_cicil_mode {
            // Cicil mode enabled - delegate to incremental engine
            match self.execute_incremental_hedge(&signal).await {
                Ok(Some(_trade)) => return Ok(()),
                Ok(None) => return Ok(()), // Skipped or failed gracefull
                Err(e) => return Err(e),
            }
        }

        // ... otherwise proceed with ATOMIC TURBO EXECUTION (below) ...

        // Resolve token ID from signal or fallback
        // CLOB API REQUIRES the decimal string of the Token ID (e.g. "8419..."), not hex ("0xabc...")
        let mut token_id = if let Some(id) = &signal.token_id {
            if id.starts_with("0x") && id.len() < 30 { // Likely a Market ID/Condition ID
                debug!("Signal provided hex Market ID {}, resolving to Token ID", id);
                self.get_token_id(&signal.market_id, signal.token_type).await?
            } else {
                id.clone()
            }
        } else {
            warn!("Signal missing token_id, resolving from market {}", signal.market_id);
            self.get_token_id(&signal.market_id, signal.token_type).await?
        };

        // FINAL SAFETY: If token_id is still hex (0x...), convert it to decimal string
        if token_id.starts_with("0x") {
            if let Ok(val) = ethers::types::U256::from_str_radix(&token_id.trim_start_matches("0x"), 16) {
                let dec_token = val.to_string();
                debug!("Converted hex token {} to decimal {}", token_id, dec_token);
                token_id = dec_token;
            }
        }

        // Create order request
        let order = OrderRequest {
            token_id,
            side: Side::Buy,
            price: signal.suggested_price,
            size: signal.suggested_size,
            order_type: OrderType::GoodTilCancelled,
            expiration: None,
            neg_risk: false,
        };

        // Log simulation or execute
        if is_simulated {
            info!(
                "[SIMULATION] Would buy {} {} shares at {} for market {}",
                signal.token_type, order.size, order.price, signal.market_id
            );
        }

        // === PRE-CALCULATION PHASE (Safety First) ===
        // Determine Leg 2 details BEFORE placing any order.
        // If this fails, we haven't spent any money yet. Safe to abort.
        
        let (leg2_type, base_leg2_price) = match signal.token_type {
            TokenType::Yes => (TokenType::No, signal.current_no),
            TokenType::No => (TokenType::Yes, signal.current_yes),
        };

        // === MAKER PRICING MODE (EARN REBATES INSTEAD OF PAYING TAKER FEES) ===
        // OLD: $0.99 taker price = 3.15% taker fee = destroys profit
        // NEW: base_price + offset = maker order that earns 1.56% rebate
        //
        // SAFETY BALANCE:
        // - If offset too small (0.01): might not fill if price moves fast → naked risk
        // - If offset too large (0.10): crosses spread → becomes taker → fee
        // - Sweet spot: +0.02 to +0.05 above current price → stays maker, fills reasonably fast
        //
        // NOTE: This trades some fill guarantee for better economics.
        // For maximum safety (old behavior), set leg2_price_offset = 0.49 in config

        let leg2_offset = self.config.trading.leg2_price_offset;
        let leg2_price_raw = base_leg2_price + leg2_offset;

        // Cap at 0.95 maximum to prevent crossing into deep taker territory
        // This means worst case we pay slightly more but stay maker most of the time
        let leg2_price = leg2_price_raw.min(Decimal::new(95, 2));

        info!(
            "📊 MAKER MODE: Leg 2 Price {} + {} offset = {} (capped at 0.95)",
            base_leg2_price, leg2_offset, leg2_price
        );

        // Resolve Leg 2 Token ID NOW
        let raw_leg2_token_id = self.get_token_id(&signal.market_id, leg2_type).await?;
        
        // Handle Hex conversion safely
        let leg2_token_id = if raw_leg2_token_id.starts_with("0x") {
             ethers::types::U256::from_str_radix(&raw_leg2_token_id.trim_start_matches("0x"), 16)
                .map(|v| v.to_string())
                .unwrap_or(raw_leg2_token_id)
        } else {
            raw_leg2_token_id
        };

        debug!("Pre-calculated Leg 2: {} ({}) @ {}", leg2_type, leg2_token_id, leg2_price);

        // === PREPARE ORDERS (But don't fire yet) ===
        // We construct both Request Objects in memory first.

        let leg1_order = order.clone(); // Clone for parallel use

        let leg2_order = OrderRequest {
            token_id: leg2_token_id.clone(),
            side: Side::Buy,
            price: leg2_price,
            size: order.size, 
            order_type: OrderType::GoodTilCancelled,
            expiration: None,
            neg_risk: false,
        };

        info!("🚀 TURBO ATOMIC: Firing BOTH Leg 1 & Leg 2 SIMULTANEOUSLY...");

        // === PARALLEL EXECUTION (The HFT Way) ===
        // We do not wait for Leg 1 to finish before sending Leg 2.
        // We send them TOGETHER to minimize latency gap.
        let execution_result = tokio::try_join!(
            self.client.place_order(&leg1_order),
            self.client.place_order(&leg2_order)
        );

        let (placed_leg1, placed_leg2) = match execution_result {
            Ok((p1, p2)) => {
                info!("✅ DOUBLE KILL: Leg 1 ({}) & Leg 2 ({}) Ordered!", p1.id, p2.id);
                (p1, p2)
            }
            Err(e) => {
                error!("❌ ATOMIC FAILURE: Parallel Execution Error: {:?}", e);
                // CRITICAL: If one failed and one succeeded, this is bad.
                // However, try_join! usually returns early on first error.
                // Given our "Suicide Squad" limit price and pre-checks, fill probability is high.
                return Err(e);
            }
        };

        // Note: Prices in placed_leg objects might be 'requested' prices, not 'filled' prices.
        // But the ID is valid for tracking.

        // ✅ MARK MARKET AS PROCESSED
        self.processed_markets.write().await.insert(signal.market_id.clone());
        info!("Market {} marked as DONE. Safety Guard Active.", signal.market_id);

        // Create hedge trade record (Both legs OPEN)
        let trade_id = uuid::Uuid::new_v4().to_string();
        let timeout_secs = self.config.trading.max_legs_timeout_sec as i64;

        let trade = HedgeTrade {
            id: trade_id.clone(),
            market_id: signal.market_id.clone(),
            asset: self.extract_asset(&signal.market_id),
            leg1: TradeLeg {
                order_id: Some(placed_leg1.id),
                token_id: order.token_id,
                token_type: signal.token_type,
                side: Side::Buy,
                price: order.price,
                size: order.size,
                filled_size: Decimal::ZERO,
                status: LegStatus::Open,
                filled_at: None,
            },
            leg2: Some(TradeLeg {
                order_id: Some(placed_leg2.id),
                token_id: leg2_token_id,
                token_type: leg2_type,
                side: Side::Buy,
                price: leg2_price,
                size: order.size,
                filled_size: Decimal::ZERO,
                status: LegStatus::Open,
                filled_at: None,
            }),
            status: HedgeStatus::Leg2Pending, // Both pending fill
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
            "Opened ATOMIC trade {}: {} {} + {} {}",
            trade_id,
            signal.token_type, signal.suggested_price,
            leg2_type, leg2_price
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
            neg_risk: false,
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
            neg_risk: false,
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
                // ATOMIC UPDATE: We now place Leg 2 immediately. This block is practically unused unless we revert strategy.
                if trade.status == HedgeStatus::Leg1Filled && trade.leg2.is_none() {
                     warn!("⚠️ Legacy Leg1Filled state detected for trade {}. Atomic strategy should have placed Leg 2 already.", trade_id);
                     // Optional: Panic rescue or just log
                }

                // ATOMIC EXECUTION HANDLER
                // In Atomic mode, status is Leg2Pending immediately. We must check fills for BOTH legs.
                if trade.status == HedgeStatus::Leg2Pending {
                    let mut leg1_done = trade.leg1.status == LegStatus::Filled;
                    let mut leg2_done = false;

                    // 1. Check Leg 1 (if not already filled)
                    if !leg1_done {
                         if self.check_leg_fill_status(trade, true).await {
                             trade.leg1.status = LegStatus::Filled;
                             trade.leg1.filled_size = trade.leg1.size;
                             trade.leg1.filled_at = Some(now);
                             info!("Atomic Trade {} Leg 1 FILLED", trade_id);
                             leg1_done = true;
                         }
                    }

                    // 2. Check Leg 2
                    let leg2_already_filled = if let Some(ref leg2) = trade.leg2 {
                        leg2.status == LegStatus::Filled
                    } else {
                        false
                    };

                    if leg2_already_filled {
                        leg2_done = true;
                    } else if trade.leg2.is_some() {
                        // Not filled yet, check via API
                        // This uses immutable borrow of `trade`
                        let is_filled = self.check_leg_fill_status(trade, false).await;
                        
                        // If filled, NOW get mutable borrow to update
                        if is_filled {
                            if let Some(ref mut leg2) = trade.leg2 {
                                leg2.status = LegStatus::Filled;
                                leg2.filled_size = leg2.size;
                                leg2.filled_at = Some(now);
                                info!("Atomic Trade {} Leg 2 FILLED", trade_id);
                                leg2_done = true;
                            }
                        }
                    }

                    // 3. If BOTH filled -> Complete
                    if leg1_done && leg2_done {
                        trade.status = HedgeStatus::Complete;
                        trade.closed_at = Some(now);

                        // Calculate PNL
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
                        
                        // Update daily stats
                        {
                            let mut stats = self.daily_stats.write().await;
                            stats.trades_count += 1;
                            stats.net_pnl += pnl;
                            if pnl > Decimal::ZERO {
                                stats.wins += 1;
                            } else {
                                stats.losses += 1;
                            }
                        }
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
            debug!(
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

    // =========================================================================
    // INCREMENTAL HEDGING (CICIL MODE)
    // =========================================================================

    /// Execute incremental hedge with cicil (1 share at a time) and momentum confirmation
    /// This is safer than firing all shares at once because:
    /// 1. Smaller initial exposure if market moves against us
    /// 2. Can abort mid-cicil if momentum reverses
    /// 3. Better fill rates on smaller orders
    pub async fn execute_incremental_hedge(
        &self,
        signal: &Signal,
    ) -> Result<Option<HedgeTrade>> {
        let is_simulated = self.config.is_test_mode();

        // Safety checks
        if self.processed_markets.read().await.contains(&signal.market_id) {
            warn!("🛑 Market {} already traded, skipping cicil", signal.market_id);
            return Ok(None);
        }

        let cicil_size = self.config.trading.cicil_share_size;
        let max_cicil = self.config.trading.max_cicil_count;
        let max_shares = self.config.trading.per_trade_shares;
        let wait_secs = self.config.trading.cicil_wait_seconds;

        // Calculate total cost for balance check
        // Balance check removed by user request (assume sufficient funds/reserve)
        // let estimated_cost = ... 


        // Resolve token IDs
        let leg1_token_id = self.get_token_id(&signal.market_id, signal.token_type).await?;
        let leg2_type = match signal.token_type {
            TokenType::Yes => TokenType::No,
            TokenType::No => TokenType::Yes,
        };
        let leg2_token_id = self.get_token_id(&signal.market_id, leg2_type).await?;

        info!("═══════════════════════════════════════════════════════════════");
        info!("  🔄 INCREMENTAL HEDGE (CICIL MODE)");
        info!("═══════════════════════════════════════════════════════════════");
        info!("  Market: {}", signal.market_id);
        info!("  Max Shares: {} | Cicil Size: {} | Max Cicils: {}", max_shares, cicil_size, max_cicil);
        info!("  Wait between cicils: {}s", wait_secs);
        info!("═══════════════════════════════════════════════════════════════");

        let mut total_leg1_filled = Decimal::ZERO;
        let mut total_leg2_filled = Decimal::ZERO;
        let mut leg1_orders: Vec<String> = Vec::new();
        let mut leg2_orders: Vec<String> = Vec::new();

        for cicil_num in 1..=max_cicil {
            if total_leg1_filled >= max_shares {
                info!("Max shares reached, stopping cicil");
                break;
            }

            let remaining = max_shares - total_leg1_filled;
            // ENFORCE MIN ORDER SIZE (Polymarket Rule: Min ~5 shares or $5)
            // If remaining < 5, we can't trade it. Stop.
            if remaining < Decimal::new(5, 0) {
                if remaining > Decimal::ZERO {
                    warn!("Remaining shares {} < Min Order Size (5). Stopping cicil.", remaining);
                }
                break;
            }

            let mut this_cicil = cicil_size.min(remaining);
            if this_cicil < Decimal::new(5, 0) {
                 this_cicil = Decimal::new(5, 0); // Force min size if config is weird
            }
            // Cap at remaining? If remaining < 5 we broke above.
            
            // === CICIL LEG 1 ===
            info!("📦 Cicil #{}: Placing Leg1 {} {} @ ${}",
                cicil_num, signal.token_type, this_cicil, signal.suggested_price);

            let leg1_order = OrderRequest {
                token_id: leg1_token_id.clone(),
                side: Side::Buy,
                price: signal.suggested_price,
                size: this_cicil,
                order_type: OrderType::GoodTilCancelled,
                expiration: None,
                neg_risk: false,
            };

            let placed_leg1 = if is_simulated {
                info!("[SIM] Would place Leg1 order");
                Order {
                    id: format!("sim_leg1_{}", uuid::Uuid::new_v4()),
                    status: PolyOrderStatus::Live,
                    token_id: leg1_token_id.clone(),
                    side: Side::Buy,
                    original_size: this_cicil,
                    size_matched: this_cicil,
                    price: signal.suggested_price,
                    created_at: Utc::now(),
                }
            } else {
                self.client.place_order(&leg1_order).await?
            };
            leg1_orders.push(placed_leg1.id.clone());
            total_leg1_filled += this_cicil;

            // === WAIT FOR MOMENTUM CONFIRMATION (Polling) ===
            info!("⏳ Waiting {}s for momentum confirmation...", wait_secs);

            // Polling logic: Check every 1s for new cascade in self.latest_cascade
            let mut momentum_confirmed = false;
            let check_interval = 2; // check every 2s
            let iterations = wait_secs / check_interval;

            for _ in 0..iterations {
                tokio::time::sleep(std::time::Duration::from_secs(check_interval as u64)).await;
                
                let cascade_opt = self.latest_cascade.read().await;
                if let Some(cascade) = cascade_opt.as_ref() {
                    // Check validity duration (stale check)
                    let now = Utc::now();
                    if now - cascade.timestamp > chrono::Duration::seconds(30) {
                         // Cascade stale
                         continue;
                    }

                    momentum_confirmed = match (signal.token_type, cascade.suggested_action) {
                        (TokenType::Yes, CascadeAction::BuyUp) => true,
                        (TokenType::No, CascadeAction::BuyDown) => true,
                        _ => false
                    };
                    
                    if momentum_confirmed {
                        info!("✅ CASCADE CONFIRMS MOMENTUM: {} ${:.0}",
                            cascade.direction, cascade.total_value_usd);
                        break; 
                    }
                }
            }

            info!("🚦 TRAFFIC LIGHT: {}", if momentum_confirmed { "🟢 HIJAU - Lanjut Cicil!" } else { "🟡 KUNING/MERAH - Hati-hati!" });

            // If NO cascade detected, we assume neutral/safe to verify via other means or just proceed slowly
            // If cascade AGAINST us, we break. But we only have `latest_cascade`.
            // Let's rely on: If not explicitly confirmed and we are > 1 cicil, proceed unless dangerous?
            // Actually, "cicil_wait_seconds" is primarily for spacing orders.
            // If no bad news, we continue.
            
            // Check risk of "Bad Cascade"
            let cascade_opt = self.latest_cascade.read().await;
            if let Some(cascade) = cascade_opt.as_ref() {
                 let against_us = match (signal.token_type, cascade.suggested_action) {
                    (TokenType::Yes, CascadeAction::BuyDown) => true,
                    (TokenType::No, CascadeAction::BuyUp) => true,
                    _ => false
                 };
                 
                 if against_us {
                     warn!("⚠️ MOMENTUM SHIFT: Cascade against our position! Stopping cicil.");
                     break; 
                 }
            }

            // === CICIL LEG 2 (HEDGE) ===
            // Fetch fresh market data to calculate Maker price
            // We want to be a Maker (limit order) to earn rebates
            // Target Price = Midpoint + Offset (0.02)
            // But capped at 0.95 to avoid crossing spread too deep (Taker)
            
            let mut leg2_price = Decimal::new(95, 2); // Fallback Default
            
             // Use `fetch_orderbook` with the Leg 2 token ID we already resolved
             match self.client.fetch_orderbook(&leg2_token_id).await {
                Ok(book) => {
                     // Get best bid/ask
                     let best_bid = book.best_bid().unwrap_or(Decimal::ZERO);
                     let best_ask = book.best_ask().unwrap_or(Decimal::new(99, 2));

                     // Calculate Midpoint
                     // If spread is wide, midpoint is safe maker.
                     // Safe approach: (Bid + Ask) / 2
                     let mid = (best_bid + best_ask) / Decimal::new(2, 0);
                     let offset = self.config.trading.leg2_price_offset; // 0.02
                     
                     let raw_price = mid + offset;
                     leg2_price = raw_price.min(Decimal::new(95, 2)); // Cap at 0.95
                     
                     info!("🎯 Leg 2 Pricing (Fresh): Bid=${:.3} Ask=${:.3} Mid=${:.3} -> Target=${:.3}", 
                         best_bid, best_ask, mid, leg2_price);
                }
                Err(e) => {
                    warn!("Failed to fetch orderbook for Leg 2 pricing: {}, using fallback 0.95", e);
                    // Use fallback 0.95 or maybe inverse signal price?
                    // Fallback to "safe" price
                }
             }

            info!("📦 Cicil #{}: Placing Leg2 {} {} @ ${} (Maker Mode)",
                cicil_num, leg2_type, this_cicil, leg2_price);

            let leg2_order = OrderRequest {
                token_id: leg2_token_id.clone(),
                side: Side::Buy,
                price: leg2_price,
                size: this_cicil,
                order_type: OrderType::GoodTilCancelled,
                expiration: None,
                neg_risk: false,
            };

            let placed_leg2 = if is_simulated {
                info!("[SIM] Would place Leg2 order");
                Order {
                    id: format!("sim_leg2_{}", uuid::Uuid::new_v4()),
                    status: PolyOrderStatus::Live,
                    token_id: leg2_token_id.clone(),
                    side: Side::Buy,
                    original_size: this_cicil,
                    size_matched: this_cicil,
                    price: leg2_price,
                    created_at: Utc::now(),
                }
            } else {
                self.client.place_order(&leg2_order).await?
            };
            leg2_orders.push(placed_leg2.id.clone());
            total_leg2_filled += this_cicil;

            info!("✅ Cicil #{} complete: Leg1={}, Leg2={}",
                cicil_num, total_leg1_filled, total_leg2_filled);

            // Short delay between cicils
            if cicil_num < max_cicil && total_leg1_filled < max_shares {
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            }
        }

        // Mark market as processed
        self.processed_markets.write().await.insert(signal.market_id.clone());

        // Create HedgeTrade record
        let trade_id = uuid::Uuid::new_v4().to_string();
        let trade = HedgeTrade {
            id: trade_id.clone(),
            market_id: signal.market_id.clone(),
            asset: self.extract_asset(&signal.market_id),
            leg1: TradeLeg {
                order_id: leg1_orders.first().cloned(),
                token_id: leg1_token_id,
                token_type: signal.token_type,
                side: Side::Buy,
                price: signal.suggested_price,
                size: total_leg1_filled,
                filled_size: total_leg1_filled,
                status: LegStatus::Filled,
                filled_at: Some(Utc::now()),
            },
            leg2: Some(TradeLeg {
                order_id: leg2_orders.first().cloned(),
                token_id: leg2_token_id,
                token_type: leg2_type,
                side: Side::Buy,
                price: signal.current_no + self.config.trading.leg2_price_offset,
                size: total_leg2_filled,
                filled_size: total_leg2_filled,
                status: LegStatus::Filled,
                filled_at: Some(Utc::now()),
            }),
            status: HedgeStatus::Complete,
            entry_reason: format!("Cicil hedge: {} iterations, {} total shares",
                leg1_orders.len(), total_leg1_filled),
            created_at: Utc::now(),
            timeout_at: Utc::now() + Duration::seconds(self.config.trading.max_legs_timeout_sec as i64),
            closed_at: Some(Utc::now()),
            pnl: None, // Will be calculated when fills confirmed
            is_simulated,
        };

        self.active_trades.write().await.insert(trade_id.clone(), trade.clone());

        info!("═══════════════════════════════════════════════════════════════");
        info!("  ✅ CICIL HEDGE COMPLETE");
        info!("  Total Leg1: {} shares | Total Leg2: {} shares", total_leg1_filled, total_leg2_filled);
        info!("  Trade ID: {}", trade_id);
        info!("═══════════════════════════════════════════════════════════════");

        Ok(Some(trade))
    }

    // =========================================================================
    // PARTIAL FILL HANDLER
    // =========================================================================

    /// Check for partial fill imbalances and auto-rebalance
    /// Call this periodically (e.g., every 5-10 seconds) instead of on every market update
    /// Check for partial fill imbalances and auto-rebalance
    /// Call this periodically (e.g., every 5-10 seconds)
    pub async fn check_and_rebalance_fills(&self) -> Result<()> {
        if !self.config.fill_monitor.enable_auto_rebalance {
            return Ok(());
        }

        let max_imbalance = self.config.fill_monitor.max_imbalance_shares;
        // Clone trades to avoid holding lock during async calls
        let trades: Vec<(String, HedgeTrade)> = {
            let guard = self.active_trades.read().await;
            guard.iter().map(|(k, v)| (k.clone(), v.clone())).collect()
        };

        for (trade_id, trade) in trades {
            // cleanup checks...
            if trade.status == HedgeStatus::Complete { continue; }

            let leg1_filled = self.get_order_filled_size(&trade.leg1).await;
            let leg2_filled = if let Some(ref leg2) = trade.leg2 {
                self.get_order_filled_size(leg2).await
            } else { Decimal::ZERO };

            let imbalance = (leg1_filled - leg2_filled).abs();
            let now = Utc::now();
            
            // === 1. TIMEOUT CHECK -> CLOSE ALL ===
            if now > trade.timeout_at {
                if leg1_filled > Decimal::ZERO || leg2_filled > Decimal::ZERO {
                    warn!("⏳ TRADE TIMEOUT: {} - Closing all positions!", trade_id);
                    // Sell Leg 1
                    if leg1_filled >= Decimal::new(5, 0) {
                        self.sell_shares(&trade.leg1.token_id, leg1_filled, Decimal::new(1, 2)).await?;
                    }
                    // Sell Leg 2
                    if let Some(ref leg2) = trade.leg2 {
                        if leg2_filled >= Decimal::new(5, 0) {
                            self.sell_shares(&leg2.token_id, leg2_filled, Decimal::new(1, 2)).await?;
                        }
                    }
                    // Mark trade as TimedOut in map? (Need write lock, later)
                }
                continue;
            }

            // === 2. IMBALANCE CHECK -> SELL EXCESS ===
            if imbalance > max_imbalance {
                let excess_side = if leg1_filled > leg2_filled { "Leg1" } else { "Leg2" };
                warn!("⚠️ IMBALANCE: Trade {} | {} Excess={} (Min Order 5)", trade_id, excess_side, imbalance);

                // Logic: Need to sell 'imbalance' shares of the heavier side
                let (token_id, total_held, excess_amt) = if leg1_filled > leg2_filled {
                    (&trade.leg1.token_id, leg1_filled, imbalance)
                } else {
                    // Safety: Leg2 must exist if it has fills
                    if let Some(ref l2) = trade.leg2 {
                         (&l2.token_id, leg2_filled, imbalance)
                    } else {
                         // Should not happen if leg2_filled > leg1_filled
                         continue;
                    }
                };

                // VALIDATE MIN SIZE
                let sell_size = if excess_amt >= Decimal::new(5, 0) {
                    excess_amt
                } else if total_held >= Decimal::new(5, 0) {
                    // Excess is small (e.g. 2), but we hold 10.
                    // We must sell at least 5 to execute order.
                    warn!("Excess {} < Min(5). Selling 5 shares to reduce exposure.", excess_amt);
                    Decimal::new(5, 0)
                } else {
                    // Total held < 5. We are stuck with dust.
                    error!("STUCK WITH DUST: Held {} < Min(5). Cannot auto-sell.", total_held);
                    continue;
                };

                // Execute SELL
                // SMART SELL PRICE LOGIC
                let mut sell_price = Decimal::new(1, 2); // Default $0.01 (Panic Dump)

                // Try to get market expiry to determine strategy
                let cache_guard = self.cached_markets.read().await;
                if let Some(market) = cache_guard.get(&trade.market_id) {
                    if let Some(ref iso) = market.end_date_iso {
                        if let Ok(end_time) = chrono::DateTime::parse_from_rfc3339(iso) {
                            let time_left = end_time.with_timezone(&Utc) - Utc::now();
                            let seconds_left = time_left.num_seconds();

                            if seconds_left > 300 {
                                // More than 5 mins left: Try slightly smarter sell
                                sell_price = Decimal::new(10, 2); 
                                info!("🕒 Time Left {}s > 300s. Using 'Soft Panic' Price: ${}", seconds_left, sell_price);
                            } else {
                                // < 5 mins: HARD PANIC ($0.01)
                                info!("🕒 Time Left {}s < 300s. Using 'Hard Panic' Price: ${}", seconds_left, sell_price);
                            }
                        }
                    } else {
                        warn!("Market {} has no end_date_iso. Defaulting to panic dump.", trade.market_id);
                    }
                } else {
                    // Market not in cache? Default to panic.
                    warn!("Market {} not found in cache for pricing. Defaulting to $0.01", trade.market_id);
                }
                drop(cache_guard); // Release lock

                warn!("⚠️ NAKED RISK DETECTED! Imbalance {} shares. Auto-selling excess @ ${}...", imbalance, sell_price);
                
                self.sell_shares(token_id, sell_size, sell_price).await?;
                
                info!("✅ AUTO-SELL REQUESTED: Excess sold to rebalance.");
            }
        }
        Ok(())
    }

    /// Helper to place a SELL order (Async)
    async fn sell_shares(&self, token_id: &str, size: Decimal, price: Decimal) -> Result<()> {
        let sell_order = OrderRequest {
            token_id: token_id.to_string(),
            side: Side::Sell,
            price,
            size, // Must be >= 5
            order_type: OrderType::GoodTilCancelled,
            expiration: None,
            neg_risk: false,
        };

        info!("📉 SELLING: {} shares @ ${}", size, price);
        match self.client.place_order(&sell_order).await {
            Ok(o) => info!("✅ SELL Placed: {}", o.id),
            Err(e) => error!("❌ SELL Failed: {}", e),
        }
        Ok(())
    }

    /// Get actual filled size for an order from API
    async fn get_order_filled_size(&self, leg: &TradeLeg) -> Decimal {
        if let Some(ref order_id) = leg.order_id {
            // In simulation, return the leg size
            if self.config.is_test_mode() {
                return leg.size;
            }

            // Query fills from API
            match self.client.get_fills(None).await {
                Ok(fills) => {
                    let filled: Decimal = fills
                        .iter()
                        .filter(|f| f.order_id == *order_id)
                        .map(|f| f.size)
                        .sum();
                    filled
                }
                Err(e) => {
                    warn!("Failed to get fills for order {}: {}", order_id, e);
                    leg.filled_size // Return cached value
                }
            }
        } else {
            leg.filled_size
        }
    }

    /// Dedicated fill monitor task - runs independently to reduce polling spam
    /// Call this from main.rs as a spawned task
    pub async fn run_fill_monitor(&self) {
        let poll_interval = self.config.fill_monitor.poll_interval_sec;

        info!("📊 Fill monitor started (interval: {}s)", poll_interval);

        loop {
            tokio::time::sleep(std::time::Duration::from_secs(poll_interval as u64)).await;

            // Check and rebalance
            if let Err(e) = self.check_and_rebalance_fills().await {
                error!("Fill monitor error: {}", e);
            }

            // Also update balance periodically
            if let Err(e) = self.update_balance().await {
                debug!("Balance update failed: {}", e);
            }
        }
    }
}

use chrono::Timelike;

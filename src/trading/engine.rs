//! Trading engine - orchestrates strategies and execution

use crate::binance::PriceMove;
use crate::config::AppConfig;
use crate::polymarket::types::{Market, MarketPrices, OrderRequest, OrderType, Side, TokenType};
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
use tokio::sync::{mpsc, RwLock};
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

        // Aggressive Pricing Calculation (PSEUDO-MARKET ORDER)
        // We increase slippage from 0.03 to 0.10 to survive massive volatility spikes.
        // Even if we bid +0.10, the engine will still give us the BEST available price (e.g. +0.00).
        // This is strictly a safety net to prevent "Unfilled Orders" during dumps/pumps.
        let leg2_price = (base_leg2_price + Decimal::new(10, 2)).min(Decimal::new(99, 2));

        info!(
            "Atomic Hedge: Adjusting Leg 2 Price from {} -> {} (Ultra-Aggressive +0.10)",
            base_leg2_price, leg2_price
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

        // === EXECUTION PHASE (Atomic) ===
        // Place Leg 1
        let placed_leg1 = self.client.place_order(&order).await?;
        info!("Leg 1 Placed: {} ({} {})", placed_leg1.id, signal.token_type, order.price);

        // Prepare Leg 2 Order
        let leg2_order = OrderRequest {
            token_id: leg2_token_id.clone(), // Ready to use immediately!
            side: Side::Buy,
            price: leg2_price,
            size: order.size, 
            order_type: OrderType::GoodTilCancelled,
            expiration: None,
            neg_risk: false,
        };

        // Warn if spread crossed (but execute anyway to hedge)
        if order.price + leg2_price >= Decimal::ONE {
             warn!("⚠️ Price Warning: {} + {} >= 1.0. Closing loop to prevent naked pos.", order.price, leg2_price);
        }

        // Place Leg 2 immediately (Nano-second delay after Leg 1)
        let placed_leg2 = self.client.place_order(&leg2_order).await?;
        info!("Leg 2 Placed (Atomic): {} ({} {})", placed_leg2.id, leg2_type, leg2_price);

        // ✅ MARK MARKET AS PROCESSED
        // This guarantees we NEVER open another trade for this Market ID in this session.
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
}

use chrono::Timelike;

//! Trading strategies for hedging and latency arbitrage

use crate::binance::{MoveDirection, PriceMove};
use crate::config::AppConfig;
use crate::polymarket::types::{MarketPrices, OrderBook, Side, TokenType};
use crate::trading::types::*;
use rust_decimal::Decimal;
use std::sync::Arc;
use tracing::{debug, info, warn};

/// Hedging arbitrage strategy
///
/// Core strategy: When Yes_price + No_price < threshold, buy both sides
/// to lock in profit when market resolves.
pub struct HedgingStrategy {
    config: Arc<AppConfig>,
}

impl HedgingStrategy {
    pub fn new(config: Arc<AppConfig>) -> Self {
        Self { config }
    }

    /// Check if hedging opportunity exists
    pub fn check_opportunity(&self, prices: &MarketPrices) -> Option<Signal> {
        let sum = prices.price_sum();
        let threshold = self.config.trading.min_profit_threshold;
        
        // === SANITY CHECKS ===
        // Reject unrealistic prices (likely stale/corrupt data from closed markets)
        
        // 1. Both prices must be in valid range [0.02, 0.98]
        let min_valid = Decimal::new(2, 2);  // 0.02
        let max_valid = Decimal::new(98, 2); // 0.98
        
        if prices.yes_price < min_valid || prices.yes_price > max_valid ||
           prices.no_price < min_valid || prices.no_price > max_valid {
            debug!(
                "Price out of valid range: Yes={:.4} No={:.4} (valid: 0.02-0.98)",
                prices.yes_price, prices.no_price
            );
            return None;
        }
        
        // 2. Sum must be realistic - in real binary markets, sum ≈ 0.98-1.02
        // If sum is too low, one side likely has stale/default data (e.g., 0.50 from empty orderbook)
        let min_sum = self.config.trading.min_price_sum;
        if sum < min_sum {
            warn!(
                "Rejecting suspicious opportunity: Yes={:.4} + No={:.4} = {:.4} (sum < {:.2}, likely stale data)",
                prices.yes_price, prices.no_price, sum, min_sum
            );
            return None;
        }

        // Debug level logging for price checks (avoid log flood)
        debug!(
            "Price check: Yes={:.4} + No={:.4} = {:.4} | Edge={:.4}",
            prices.yes_price, prices.no_price, sum, threshold - sum
        );

        if sum >= threshold {
            return None;
        }

        let edge = threshold - sum;
        let min_edge = Decimal::new(self.config.trading.min_edge_cents as i64, 2);

        if edge < min_edge && min_edge > Decimal::ZERO {
            debug!("Edge {:.4} too small, min required: {:.4}", edge, min_edge);
            return None;
        }

        // Determine which side to buy first (the cheaper one)
        let (token_type, price, token_id) = if prices.yes_price < prices.no_price {
            (TokenType::Yes, prices.yes_price, &prices.yes_token_id)
        } else {
            (TokenType::No, prices.no_price, &prices.no_token_id)
        };

        let size = self.config.trading.per_trade_shares;

        info!(
            "Hedge opportunity: {} + {} = {} (< {}), edge: {:.4}",
            prices.yes_price, prices.no_price, sum, threshold, edge
        );

        Some(Signal {
            signal_type: SignalType::HedgeEntry,
            market_id: prices.condition_id.clone(),
            token_type,
            token_id: Some(token_id.clone()),
            suggested_price: price,
            suggested_size: size,
            confidence: edge / threshold, // Higher edge = higher confidence
            reason: format!(
                "Hedge arb: sum={:.4}, edge={:.4}, buy {} first",
                sum, edge, token_type
            ),
            timestamp: chrono::Utc::now(),
            current_yes: prices.yes_price,
            current_no: prices.no_price,
        })
    }

    /// Calculate optimal Leg 2 order
    pub fn calculate_leg2(
        &self,
        leg1: &TradeLeg,
        yes_book: &OrderBook,
        no_book: &OrderBook,
    ) -> Option<(TokenType, Decimal, Decimal)> {
        // Leg 2 is the opposite of Leg 1
        let (token_type, book) = match leg1.token_type {
            TokenType::Yes => (TokenType::No, no_book),
            TokenType::No => (TokenType::Yes, yes_book),
        };

        let price = book.best_ask()?;
        let size = leg1.filled_size; // Match Leg 1 size

        // Verify total cost is still below 1.0 for profit
        let total_cost = leg1.price * leg1.filled_size + price * size;
        let total_shares = leg1.filled_size.min(size);

        if total_cost >= total_shares {
            debug!(
                "Leg 2 would not be profitable: cost {} >= payout {}",
                total_cost, total_shares
            );
            return None;
        }

        Some((token_type, price, size))
    }

    /// Check if averaging down is appropriate
    pub fn should_average_down(
        &self,
        position: &Position,
        current_price: Decimal,
        token_id: &str,
    ) -> Option<Signal> {
        let dump_trigger = self.config.trading.dump_trigger_pct;

        // Calculate price drop from average
        let avg_price = if position.yes_shares > position.no_shares {
            position.yes_avg_price
        } else {
            position.no_avg_price
        };

        if avg_price == Decimal::ZERO {
            return None;
        }

        let drop_pct = (avg_price - current_price) / avg_price * Decimal::new(100, 0);

        if drop_pct >= dump_trigger {
            let token_type = if position.yes_shares > position.no_shares {
                TokenType::Yes
            } else {
                TokenType::No
            };

            info!(
                "Average down opportunity: {} dropped {:.2}% from avg {}",
                token_type, drop_pct, avg_price
            );

            return Some(Signal {
                signal_type: SignalType::AverageDown,
                market_id: position.market_id.clone(),
                token_type,
                token_id: Some(token_id.to_string()),
                suggested_price: current_price,
                suggested_size: self.config.trading.per_trade_shares,
                confidence: (drop_pct / dump_trigger).min(Decimal::ONE),
                reason: format!("Avg down: {:.2}% drop, new price {}", drop_pct, current_price),
                timestamp: chrono::Utc::now(),
                current_yes: Decimal::ZERO,
                current_no: Decimal::ZERO,
            });
        }

        None
    }
}

/// Latency arbitrage strategy
///
/// Uses Binance spot price moves to predict Polymarket outcome
/// and enter before odds adjust.
pub struct LatencyStrategy {
    config: Arc<AppConfig>,
}

impl LatencyStrategy {
    pub fn new(config: Arc<AppConfig>) -> Self {
        Self { config }
    }

    /// Process Binance price move and generate signal if opportunity exists
    pub fn process_move(
        &self,
        price_move: &PriceMove,
        market_prices: &MarketPrices,
        market_id: &str,
    ) -> Option<Signal> {
        let trigger_pct = self.config.trading.spot_move_trigger_pct;

        if price_move.change_pct.abs() < trigger_pct {
            return None;
        }

        // Determine expected winning side based on move direction
        // For "Will X go up?" markets: Up move = Yes wins, Down move = No wins
        let expected_winner = match price_move.direction {
            MoveDirection::Up => TokenType::Yes,
            MoveDirection::Down => TokenType::No,
        };

        // Get current odds for the expected winner
        let (current_price, token_id) = match expected_winner {
            TokenType::Yes => (market_prices.yes_price, &market_prices.yes_token_id),
            TokenType::No => (market_prices.no_price, &market_prices.no_token_id),
        };

        // Check if there's lag (odds haven't adjusted yet)
        // If big move up, Yes should be high. If still low, there's opportunity.
        let expected_min_price = match price_move.direction {
            MoveDirection::Up if price_move.change_pct > Decimal::ONE => {
                Decimal::new(60, 2) // Expect Yes > 0.60 after 1%+ up
            }
            MoveDirection::Down if price_move.change_pct.abs() > Decimal::ONE => {
                Decimal::new(60, 2) // Expect No > 0.60 after 1%+ down
            }
            _ => Decimal::new(55, 2), // Smaller moves, lower threshold
        };

        if current_price >= expected_min_price {
            debug!(
                "No latency opportunity: {} already at {} (expected lag below {})",
                expected_winner, current_price, expected_min_price
            );
            return None;
        }

        let edge = expected_min_price - current_price;
        let size = self.config.trading.per_trade_shares;

        info!(
            "Latency opportunity: {} move {:.2}%, {} at {} (expected > {}), edge: {:.4}",
            price_move.symbol,
            price_move.change_pct,
            expected_winner,
            current_price,
            expected_min_price,
            edge
        );

        Some(Signal {
            signal_type: SignalType::LatencyEntry,
            market_id: market_id.to_string(),
            token_type: expected_winner,
            token_id: Some(token_id.clone()),
            suggested_price: current_price + Decimal::new(1, 2), // Bid 1 cent above market
            suggested_size: size,
            confidence: (edge / Decimal::new(10, 2)).min(Decimal::ONE),
            reason: format!(
                "Latency arb: {} {:.2}% -> buy {} @ {}",
                price_move.symbol, price_move.change_pct, expected_winner, current_price
            ),
            timestamp: chrono::Utc::now(),
            current_yes: Decimal::ZERO,
            current_no: Decimal::ZERO,
        })
    }
}

/// Combined strategy manager
pub struct StrategyManager {
    pub hedging: HedgingStrategy,
    pub latency: LatencyStrategy,
    config: Arc<AppConfig>,
}

impl StrategyManager {
    pub fn new(config: Arc<AppConfig>) -> Self {
        Self {
            hedging: HedgingStrategy::new(Arc::clone(&config)),
            latency: LatencyStrategy::new(Arc::clone(&config)),
            config,
        }
    }

    /// Check all strategies for opportunities
    pub fn evaluate(
        &self,
        market_prices: &MarketPrices,
        binance_move: Option<&PriceMove>,
        position: Option<&Position>,
    ) -> Vec<Signal> {
        let mut signals = Vec::new();

        // Check hedging opportunity
        if let Some(mut signal) = self.hedging.check_opportunity(market_prices) {
            // Boost confidence if maker rebates are enabled
            if self.config.maker_rebates.enabled {
                signal = self.apply_rebate_boost(signal);
            }
            signals.push(signal);
        }

        // Check latency opportunity from Binance
        if let Some(mv) = binance_move {
            if let Some(mut signal) =
                self.latency
                    .process_move(mv, market_prices, &market_prices.condition_id)
            {
                if self.config.maker_rebates.enabled {
                    signal = self.apply_rebate_boost(signal);
                }
                signals.push(signal);
            }
        }

        // Check averaging down if we have a position
        if let Some(pos) = position {
            let (current_price, token_id) = if pos.yes_shares > pos.no_shares {
                (market_prices.yes_price, &market_prices.yes_token_id)
            } else {
                (market_prices.no_price, &market_prices.no_token_id)
            };

            if let Some(mut signal) = self.hedging.should_average_down(pos, current_price, token_id) {
                if self.config.maker_rebates.enabled {
                    signal = self.apply_rebate_boost(signal);
                }
                signals.push(signal);
            }
        }

        signals
    }

    /// Apply rebate-based confidence boost to a signal
    /// Higher rebate potential = higher confidence = higher priority
    fn apply_rebate_boost(&self, mut signal: Signal) -> Signal {
        let rebate_estimate = self.estimate_rebate_for_signal(&signal);
        let threshold = self.config.maker_rebates.rebate_estimate_threshold;

        if rebate_estimate > threshold {
            // Boost confidence by up to 10% based on rebate potential
            let rebate_factor = (rebate_estimate / threshold).min(Decimal::new(2, 0));
            let boost = Decimal::new(1, 1) * (rebate_factor - Decimal::ONE); // 0-10% boost

            signal.confidence = (signal.confidence + boost).min(Decimal::ONE);

            let old_reason = signal.reason.clone();
            signal.reason = format!(
                "{} | Rebate boost: +{:.2}% est rebate",
                old_reason, rebate_estimate
            );

            debug!(
                "Rebate boost applied: {} -> confidence {:.3}",
                signal.signal_type, signal.confidence
            );
        }

        signal
    }

    /// Estimate rebate percentage for a signal
    fn estimate_rebate_for_signal(&self, signal: &Signal) -> Decimal {
        let price = signal.suggested_price;

        // Rebate rate is symmetric around $0.50, max ~1.56%
        // Formula: 1.56% * 4 * p * (1-p)
        let base_rate = Decimal::new(156, 4); // 1.56%
        let price_factor = Decimal::new(4, 0) * price * (Decimal::ONE - price);

        base_rate * price_factor * Decimal::new(100, 0) // Convert to percentage
    }
}

/// Rebate-aware order pricing
/// Adjusts limit order price to maximize rebate while ensuring fill
pub fn optimize_limit_price_for_rebate(
    base_price: Decimal,
    side: Side,
    spread_bps: u32,
) -> Decimal {
    // For maker orders, price slightly better than market to ensure we're maker
    // But not so aggressive that we don't get filled
    let adjustment = Decimal::new(spread_bps as i64 / 4, 4); // 25% of spread

    match side {
        Side::Buy => base_price + adjustment, // Bid higher to get filled, still maker if below ask
        Side::Sell => base_price - adjustment, // Ask lower to get filled, still maker if above bid
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_test_config() -> Arc<AppConfig> {
        Arc::new(AppConfig::default())
    }

    #[test]
    fn test_hedge_opportunity() {
        let config = make_test_config();
        let strategy = HedgingStrategy::new(config);

        let prices = MarketPrices {
            condition_id: "test".to_string(),
            yes_price: Decimal::new(40, 2), // 0.40
            no_price: Decimal::new(50, 2),  // 0.50
            yes_token_id: "yes123".to_string(),
            no_token_id: "no123".to_string(),
            timestamp: chrono::Utc::now(),
        };

        let signal = strategy.check_opportunity(&prices);
        assert!(signal.is_some());

        let sig = signal.unwrap();
        assert_eq!(sig.signal_type, SignalType::HedgeEntry);
        assert_eq!(sig.token_type, TokenType::Yes); // Cheaper side
    }

    #[test]
    fn test_no_hedge_when_sum_high() {
        let config = make_test_config();
        let strategy = HedgingStrategy::new(config);

        let prices = MarketPrices {
            condition_id: "test".to_string(),
            yes_price: Decimal::new(50, 2), // 0.50
            no_price: Decimal::new(50, 2),  // 0.50
            yes_token_id: "yes123".to_string(),
            no_token_id: "no123".to_string(),
            timestamp: chrono::Utc::now(),
        };

        let signal = strategy.check_opportunity(&prices);
        assert!(signal.is_none()); // Sum = 1.0, no opportunity
    }
}

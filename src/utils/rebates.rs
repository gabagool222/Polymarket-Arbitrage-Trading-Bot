//! Maker rebates tracking and estimation
//!
//! Tracks filled maker volume and estimates daily rebates from the
//! Polymarket maker rebates program (Jan 2026).
//!
//! Rebates are funded 100% from taker fees and paid daily in USDC.
//! Rate: Up to 1.56% at $0.50 price (symmetric, lower at extremes).

use crate::config::AppConfig;
use crate::polymarket::types::{Fill, RebateEstimate};
use anyhow::Result;
use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::sync::Arc;
use tracing::{debug, info, warn};

/// Daily rebate tracking record
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DailyRebateRecord {
    pub date: String,
    pub maker_volume: Decimal,
    pub fill_count: u32,
    pub estimated_rebate: Decimal,
    pub actual_rebate: Option<Decimal>,
    pub is_simulated: bool,
}

/// Rebate tracker for monitoring and estimating maker rebates
pub struct RebateTracker {
    config: Arc<AppConfig>,
    /// Daily records by date
    records: HashMap<String, DailyRebateRecord>,
    /// Today's running totals
    today_volume: Decimal,
    today_fills: u32,
    today_estimated: Decimal,
    /// Simulated rebates (test mode)
    simulated_volume: Decimal,
    simulated_rebates: Decimal,
}

impl RebateTracker {
    /// Create a new rebate tracker
    pub fn new(config: Arc<AppConfig>) -> Self {
        Self {
            config,
            records: HashMap::new(),
            today_volume: Decimal::ZERO,
            today_fills: 0,
            today_estimated: Decimal::ZERO,
            simulated_volume: Decimal::ZERO,
            simulated_rebates: Decimal::ZERO,
        }
    }

    /// Load historical rebate records from file
    pub fn load_from_file(&mut self) -> Result<()> {
        let path = &self.config.maker_rebates.rebates_file;

        if !Path::new(path).exists() {
            info!("Rebates file not found, starting fresh");
            return Ok(());
        }

        let file = File::open(path)?;
        let reader = BufReader::new(file);

        for line in reader.lines() {
            let line = line?;
            if line.is_empty() || line.starts_with('#') {
                continue;
            }

            match serde_json::from_str::<DailyRebateRecord>(&line) {
                Ok(record) => {
                    self.records.insert(record.date.clone(), record);
                }
                Err(e) => {
                    warn!("Failed to parse rebate record: {}", e);
                }
            }
        }

        info!("Loaded {} rebate records", self.records.len());
        Ok(())
    }

    /// Record a maker fill
    pub fn record_fill(&mut self, fill: &Fill, is_simulated: bool) {
        if !fill.is_maker {
            return;
        }

        let volume = fill.volume();
        let rebate = self.estimate_rebate_for_fill(fill);

        if is_simulated {
            self.simulated_volume += volume;
            self.simulated_rebates += rebate;

            info!(
                "[SIM] Maker fill: {} @ {} = ${:.4} volume, ${:.6} est rebate",
                fill.size, fill.price, volume, rebate
            );
        } else {
            self.today_volume += volume;
            self.today_fills += 1;
            self.today_estimated += rebate;

            debug!(
                "Maker fill: {} @ {} = ${:.4} volume, ${:.6} est rebate",
                fill.size, fill.price, volume, rebate
            );
        }
    }

    /// Record a simulated order (test mode)
    pub fn record_simulated_order(
        &mut self,
        price: Decimal,
        size: Decimal,
    ) {
        let volume = price * size;
        let rebate = self.calculate_rebate_at_price(price, volume);

        self.simulated_volume += volume;
        self.simulated_rebates += rebate;

        info!(
            "[SIM] Would earn rebate: ${:.6} from ${:.4} volume @ {}",
            rebate, volume, price
        );
    }

    /// Estimate rebate for a fill based on price
    fn estimate_rebate_for_fill(&self, fill: &Fill) -> Decimal {
        let volume = fill.volume();
        self.calculate_rebate_at_price(fill.price, volume)
    }

    /// Calculate rebate at a given price
    /// Rebate rate is symmetric around $0.50, max ~1.56%
    fn calculate_rebate_at_price(&self, price: Decimal, volume: Decimal) -> Decimal {
        // Base rate ~1% (100 bps), max 1.56% at $0.50
        // Formula: base_rate * 4 * p * (1-p) gives max at p=0.50
        let base_rate = Decimal::new(156, 4); // 0.0156 = 1.56%
        let price_factor = Decimal::new(4, 0) * price * (Decimal::ONE - price);
        let effective_rate = base_rate * price_factor;

        volume * effective_rate
    }

    /// Get today's rebate estimate
    pub fn get_today_estimate(&self) -> RebateEstimate {
        let today = Utc::now().format("%Y-%m-%d").to_string();

        RebateEstimate {
            date: today,
            maker_volume: self.today_volume,
            fill_count: self.today_fills,
            estimated_rebate: self.today_estimated,
            effective_rate_bps: if self.today_volume > Decimal::ZERO {
                let rate = self.today_estimated / self.today_volume * Decimal::new(10000, 0);
                rate.to_string().parse().unwrap_or(100)
            } else {
                100
            },
        }
    }

    /// Get simulated rebate totals
    pub fn get_simulated_totals(&self) -> (Decimal, Decimal) {
        (self.simulated_volume, self.simulated_rebates)
    }

    /// Save today's record to file
    pub fn save_today(&self) -> Result<()> {
        if !self.config.maker_rebates.track_daily_rebates {
            return Ok(());
        }

        let today = Utc::now().format("%Y-%m-%d").to_string();
        let is_simulated = self.config.is_test_mode();

        let record = DailyRebateRecord {
            date: today.clone(),
            maker_volume: if is_simulated {
                self.simulated_volume
            } else {
                self.today_volume
            },
            fill_count: if is_simulated {
                0 // Simulated doesn't have real fill count
            } else {
                self.today_fills
            },
            estimated_rebate: if is_simulated {
                self.simulated_rebates
            } else {
                self.today_estimated
            },
            actual_rebate: None, // Filled in manually when wallet receives rebate
            is_simulated,
        };

        let path = &self.config.maker_rebates.rebates_file;
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)?;

        let json = serde_json::to_string(&record)?;
        writeln!(file, "{}", json)?;

        info!(
            "Saved rebate record for {}: est ${:.4} from ${:.2} volume",
            today, record.estimated_rebate, record.maker_volume
        );

        Ok(())
    }

    /// Reset daily counters (call at start of new day)
    pub fn reset_daily(&mut self) {
        // Save yesterday's data first
        let _ = self.save_today();

        self.today_volume = Decimal::ZERO;
        self.today_fills = 0;
        self.today_estimated = Decimal::ZERO;

        info!("Daily rebate counters reset");
    }

    /// Get historical rebate estimate for a date
    pub fn get_historical(&self, date: &str) -> Option<&DailyRebateRecord> {
        self.records.get(date)
    }

    /// Get total estimated rebates across all days
    pub fn get_total_estimated(&self) -> Decimal {
        let historical: Decimal = self
            .records
            .values()
            .map(|r| r.estimated_rebate)
            .sum();

        historical + self.today_estimated + self.simulated_rebates
    }

    /// Print rebate summary
    pub fn print_summary(&self) {
        let today = self.get_today_estimate();

        if self.config.is_test_mode() {
            info!("=== SIMULATED REBATES ===");
            info!(
                "Total Simulated Volume: ${:.2}",
                self.simulated_volume
            );
            info!(
                "Total Estimated Rebates: ${:.4}",
                self.simulated_rebates
            );
        } else {
            info!("=== MAKER REBATES (Today) ===");
            info!("{}", today);
            info!(
                "All-time Estimated: ${:.4}",
                self.get_total_estimated()
            );
        }
        info!("==========================");
    }
}

/// Check if rebates are worth prioritizing for a trade
pub fn should_prioritize_for_rebates(
    config: &AppConfig,
    price: Decimal,
    size: Decimal,
) -> bool {
    if !config.maker_rebates.enabled {
        return false;
    }

    let volume = price * size;
    let base_rate = Decimal::new(156, 4); // 1.56%
    let price_factor = Decimal::new(4, 0) * price * (Decimal::ONE - price);
    let effective_rate = base_rate * price_factor;
    let estimated_rebate_pct = effective_rate * Decimal::new(100, 0);

    estimated_rebate_pct >= config.maker_rebates.rebate_estimate_threshold
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_rebate_at_midpoint() {
        let config = Arc::new(AppConfig::default());
        let tracker = RebateTracker::new(config);

        // At $0.50 price, rebate should be maximum (~1.56%)
        let volume = Decimal::new(100, 0); // $100
        let rebate = tracker.calculate_rebate_at_price(Decimal::new(50, 2), volume);

        // 1.56% of $100 = $1.56
        assert!(rebate > Decimal::new(15, 1)); // > $1.5
        assert!(rebate < Decimal::new(17, 1)); // < $1.7
    }

    #[test]
    fn test_rebate_at_extreme() {
        let config = Arc::new(AppConfig::default());
        let tracker = RebateTracker::new(config);

        // At $0.10 price, rebate should be lower
        let volume = Decimal::new(100, 0);
        let rebate_low = tracker.calculate_rebate_at_price(Decimal::new(10, 2), volume);
        let rebate_mid = tracker.calculate_rebate_at_price(Decimal::new(50, 2), volume);

        assert!(rebate_low < rebate_mid);
    }
}

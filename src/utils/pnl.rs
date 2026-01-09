//! PNL (Profit and Loss) tracking

use crate::config::AppConfig;
use crate::trading::types::{DailyStats, HedgeTrade, HedgeStatus};
use anyhow::Result;
use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{info, warn};

/// PNL record for a single trade
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PnlRecord {
    pub trade_id: String,
    pub timestamp: DateTime<Utc>,
    pub asset: String,
    pub entry_cost: Decimal,
    pub exit_value: Decimal,
    pub gross_pnl: Decimal,
    pub fees: Decimal,
    pub net_pnl: Decimal,
    pub is_simulated: bool,
}

/// PNL tracker for recording and analyzing trade performance
pub struct PnlTracker {
    config: Arc<AppConfig>,
    records: Vec<PnlRecord>,
    cumulative_pnl: Decimal,
    simulated_pnl: Decimal,
    real_pnl: Decimal,
}

impl PnlTracker {
    /// Create a new PNL tracker
    pub fn new(config: Arc<AppConfig>) -> Self {
        Self {
            config,
            records: Vec::new(),
            cumulative_pnl: Decimal::ZERO,
            simulated_pnl: Decimal::ZERO,
            real_pnl: Decimal::ZERO,
        }
    }

    /// Load existing PNL records from file
    pub fn load_from_file(&mut self) -> Result<()> {
        let path = &self.config.logging.pnl_file;

        if !Path::new(path).exists() {
            info!("PNL file not found, starting fresh");
            return Ok(());
        }

        let file = File::open(path)?;
        let reader = BufReader::new(file);

        for line in reader.lines() {
            let line = line?;
            if line.is_empty() || line.starts_with('#') {
                continue;
            }

            match serde_json::from_str::<PnlRecord>(&line) {
                Ok(record) => {
                    self.cumulative_pnl += record.net_pnl;
                    if record.is_simulated {
                        self.simulated_pnl += record.net_pnl;
                    } else {
                        self.real_pnl += record.net_pnl;
                    }
                    self.records.push(record);
                }
                Err(e) => {
                    warn!("Failed to parse PNL record: {}", e);
                }
            }
        }

        info!(
            "Loaded {} PNL records, cumulative: {} (sim: {}, real: {})",
            self.records.len(),
            self.cumulative_pnl,
            self.simulated_pnl,
            self.real_pnl
        );

        Ok(())
    }

    /// Record a completed trade
    pub fn record_trade(&mut self, trade: &HedgeTrade) -> Result<PnlRecord> {
        // Calculate PNL based on hedge status
        let (entry_cost, exit_value, gross_pnl) = self.calculate_trade_pnl(trade);

        // Estimate fees (maker rebate for limit orders)
        // Polymarket: 0% taker fee, 1-1.5% maker rebate
        let volume = entry_cost + exit_value;
        let fees = if trade.is_simulated {
            Decimal::ZERO
        } else {
            // Assume small effective fee after rebates
            volume * Decimal::new(-1, 3) // -0.1% (rebate)
        };

        let net_pnl = gross_pnl - fees;

        let record = PnlRecord {
            trade_id: trade.id.clone(),
            timestamp: trade.closed_at.unwrap_or_else(Utc::now),
            asset: trade.asset.clone(),
            entry_cost,
            exit_value,
            gross_pnl,
            fees,
            net_pnl,
            is_simulated: trade.is_simulated,
        };

        // Update cumulative totals
        self.cumulative_pnl += net_pnl;
        if trade.is_simulated {
            self.simulated_pnl += net_pnl;
        } else {
            self.real_pnl += net_pnl;
        }

        // Append to file
        self.append_to_file(&record)?;

        self.records.push(record.clone());

        info!(
            "[PNL] Trade {}: {} (cumulative: {})",
            trade.id, net_pnl, self.cumulative_pnl
        );

        Ok(record)
    }

    /// Calculate PNL for a trade
    fn calculate_trade_pnl(&self, trade: &HedgeTrade) -> (Decimal, Decimal, Decimal) {
        let leg1 = &trade.leg1;
        let leg1_cost = leg1.price * leg1.filled_size;

        match &trade.leg2 {
            Some(leg2) => {
                let leg2_cost = leg2.price * leg2.filled_size;
                let entry_cost = leg1_cost + leg2_cost;

                // For a complete hedge, we get $1 per share when market resolves
                let exit_value = leg1.filled_size.min(leg2.filled_size);

                let gross_pnl = exit_value - entry_cost;
                (entry_cost, exit_value, gross_pnl)
            }
            None => {
                // Incomplete hedge - estimate based on current prices
                // or assume loss if timed out
                match trade.status {
                    HedgeStatus::TimedOut | HedgeStatus::Cancelled => {
                        // Assume we lost the spread on exit
                        let loss = leg1_cost * Decimal::new(5, 2); // 5% loss estimate
                        (leg1_cost, leg1_cost - loss, -loss)
                    }
                    _ => (leg1_cost, Decimal::ZERO, -leg1_cost),
                }
            }
        }
    }

    /// Append record to PNL file
    fn append_to_file(&self, record: &PnlRecord) -> Result<()> {
        let path = &self.config.logging.pnl_file;

        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)?;

        let json = serde_json::to_string(record)?;
        writeln!(file, "{}", json)?;

        Ok(())
    }

    /// Get cumulative PNL
    pub fn get_cumulative_pnl(&self) -> Decimal {
        self.cumulative_pnl
    }

    /// Get simulated PNL only
    pub fn get_simulated_pnl(&self) -> Decimal {
        self.simulated_pnl
    }

    /// Get real PNL only
    pub fn get_real_pnl(&self) -> Decimal {
        self.real_pnl
    }

    /// Get daily summary
    pub fn get_daily_summary(&self, date: &str) -> DailySummary {
        let day_records: Vec<_> = self
            .records
            .iter()
            .filter(|r| r.timestamp.format("%Y-%m-%d").to_string() == date)
            .collect();

        let total_pnl: Decimal = day_records.iter().map(|r| r.net_pnl).sum();
        let wins = day_records.iter().filter(|r| r.net_pnl > Decimal::ZERO).count();
        let losses = day_records.iter().filter(|r| r.net_pnl < Decimal::ZERO).count();

        DailySummary {
            date: date.to_string(),
            trades: day_records.len() as u32,
            wins: wins as u32,
            losses: losses as u32,
            net_pnl: total_pnl,
        }
    }

    /// Get performance statistics
    pub fn get_statistics(&self) -> PerformanceStats {
        if self.records.is_empty() {
            return PerformanceStats::default();
        }

        let wins = self
            .records
            .iter()
            .filter(|r| r.net_pnl > Decimal::ZERO)
            .count();

        let losses = self
            .records
            .iter()
            .filter(|r| r.net_pnl < Decimal::ZERO)
            .count();

        let total_trades = self.records.len();

        let win_rate = if total_trades > 0 {
            Decimal::from(wins) / Decimal::from(total_trades) * Decimal::new(100, 0)
        } else {
            Decimal::ZERO
        };

        let avg_win: Decimal = if wins > 0 {
            self.records
                .iter()
                .filter(|r| r.net_pnl > Decimal::ZERO)
                .map(|r| r.net_pnl)
                .sum::<Decimal>()
                / Decimal::from(wins)
        } else {
            Decimal::ZERO
        };

        let avg_loss: Decimal = if losses > 0 {
            self.records
                .iter()
                .filter(|r| r.net_pnl < Decimal::ZERO)
                .map(|r| r.net_pnl.abs())
                .sum::<Decimal>()
                / Decimal::from(losses)
        } else {
            Decimal::ZERO
        };

        let profit_factor = if avg_loss > Decimal::ZERO {
            avg_win / avg_loss
        } else {
            Decimal::ZERO
        };

        PerformanceStats {
            total_trades: total_trades as u32,
            wins: wins as u32,
            losses: losses as u32,
            win_rate,
            avg_win,
            avg_loss,
            profit_factor,
            cumulative_pnl: self.cumulative_pnl,
            simulated_pnl: self.simulated_pnl,
            real_pnl: self.real_pnl,
        }
    }
}

/// Daily trading summary
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DailySummary {
    pub date: String,
    pub trades: u32,
    pub wins: u32,
    pub losses: u32,
    pub net_pnl: Decimal,
}

/// Performance statistics
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PerformanceStats {
    pub total_trades: u32,
    pub wins: u32,
    pub losses: u32,
    pub win_rate: Decimal,
    pub avg_win: Decimal,
    pub avg_loss: Decimal,
    pub profit_factor: Decimal,
    pub cumulative_pnl: Decimal,
    pub simulated_pnl: Decimal,
    pub real_pnl: Decimal,
}

impl std::fmt::Display for PerformanceStats {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Trades: {} | W/L: {}/{} ({:.1}%) | Avg W/L: {:.4}/{:.4} | PF: {:.2} | PNL: {} (Sim: {}, Real: {})",
            self.total_trades,
            self.wins,
            self.losses,
            self.win_rate,
            self.avg_win,
            self.avg_loss,
            self.profit_factor,
            self.cumulative_pnl,
            self.simulated_pnl,
            self.real_pnl
        )
    }
}

//! Configuration module for PolyRustArb bot
//!
//! Loads configuration from config.toml and environment variables.
//! Environment variables override config file values with POLY_ prefix.

use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use std::path::Path;
use thiserror::Error;

/// Default value for min_price_sum (used by serde)
fn default_min_price_sum() -> Decimal {
    Decimal::new(90, 2) // 0.90
}

#[derive(Error, Debug)]
pub enum ConfigError {
    #[error("Failed to read config file: {0}")]
    ReadError(#[from] std::io::Error),

    #[error("Failed to parse config: {0}")]
    ParseError(#[from] toml::de::Error),

    #[error("Invalid configuration: {0}")]
    ValidationError(String),

    #[error("Missing required environment variable: {0}")]
    MissingEnvVar(String),
}

/// Operating mode for the bot
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum OperatingMode {
    #[default]
    Test,
    Real,
}

impl std::fmt::Display for OperatingMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OperatingMode::Test => write!(f, "test"),
            OperatingMode::Real => write!(f, "real"),
        }
    }
}

impl std::str::FromStr for OperatingMode {
    type Err = ConfigError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "test" | "simulation" | "sim" => Ok(OperatingMode::Test),
            "real" | "production" | "prod" | "live" => Ok(OperatingMode::Real),
            _ => Err(ConfigError::ValidationError(
                format!("Invalid mode '{}'. Use 'test' or 'real'", s)
            )),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GeneralConfig {
    #[serde(default)]
    pub mode: OperatingMode,
    pub rpc_url: String,
    pub clob_api_url: String,
    pub gamma_api_url: String,
    pub clob_ws_url: String,
}

impl Default for GeneralConfig {
    fn default() -> Self {
        Self {
            mode: OperatingMode::Test,
            rpc_url: "https://polygon-rpc.com".to_string(),
            clob_api_url: "https://clob.polymarket.com".to_string(),
            gamma_api_url: "https://gamma-api.polymarket.com".to_string(),
            clob_ws_url: "wss://ws-subscriptions-clob.polymarket.com/ws/market".to_string(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WalletConfig {
    pub chain_id: u64,
}

impl Default for WalletConfig {
    fn default() -> Self {
        Self { chain_id: 137 }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TradingConfig {
    pub market_slugs: Vec<String>,

    #[serde(with = "rust_decimal::serde::str")]
    pub per_trade_shares: Decimal,

    #[serde(with = "rust_decimal::serde::str")]
    pub min_profit_threshold: Decimal,

    /// Minimum sum of Yes+No prices to consider valid (anti-stale data)
    #[serde(with = "rust_decimal::serde::str", default = "default_min_price_sum")]
    pub min_price_sum: Decimal,

    #[serde(with = "rust_decimal::serde::str")]
    pub dump_trigger_pct: Decimal,

    #[serde(with = "rust_decimal::serde::str")]
    pub spot_move_trigger_pct: Decimal,

    pub entry_window_min: u32,
    pub max_legs_timeout_sec: u32,

    #[serde(with = "rust_decimal::serde::str")]
    pub max_daily_risk_pct: Decimal,

    pub orderbook_depth: usize,
    pub slippage_bps: u32,
    pub max_order_retries: u32,
    pub min_edge_cents: u32,
}

impl Default for TradingConfig {
    fn default() -> Self {
        Self {
            market_slugs: vec![
                "will-btc-go-up-15-min".to_string(),
                "will-eth-go-up-15-min".to_string(),
                "will-sol-go-up-15-min".to_string(),
            ],
            per_trade_shares: Decimal::new(20, 1), // 2.0
            min_profit_threshold: Decimal::new(95, 2), // 0.95
            min_price_sum: Decimal::new(90, 2), // 0.90
            dump_trigger_pct: Decimal::new(15, 2), // 0.15
            spot_move_trigger_pct: Decimal::new(5, 1), // 0.5
            entry_window_min: 2,
            max_legs_timeout_sec: 420,
            max_daily_risk_pct: Decimal::new(200, 1), // 20.0
            orderbook_depth: 5,
            slippage_bps: 50,
            max_order_retries: 3,
            min_edge_cents: 2,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BinanceConfig {
    pub enabled: bool,
    pub symbols: Vec<String>,
    pub move_window_sec: u32,
    pub ws_url: String,
    pub reconnect_delay_ms: u64,
    pub max_reconnect_attempts: u32,
}

impl Default for BinanceConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            symbols: vec![
                "btcusdt".to_string(),
                "ethusdt".to_string(),
                "solusdt".to_string(),
            ],
            move_window_sec: 5,
            ws_url: "wss://stream.binance.com:9443".to_string(),
            reconnect_delay_ms: 1000,
            max_reconnect_attempts: 10,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RiskConfig {
    #[serde(with = "rust_decimal::serde::str")]
    pub max_position_per_market: Decimal,

    #[serde(with = "rust_decimal::serde::str")]
    pub max_total_exposure: Decimal,

    #[serde(with = "rust_decimal::serde::str")]
    pub stop_loss_pct: Decimal,

    pub loss_cooldown_sec: u32,
    pub max_consecutive_losses: u32,
}

impl Default for RiskConfig {
    fn default() -> Self {
        Self {
            max_position_per_market: Decimal::new(10000, 1), // 1000.0
            max_total_exposure: Decimal::new(50000, 1), // 5000.0
            stop_loss_pct: Decimal::new(100, 1), // 10.0
            loss_cooldown_sec: 60,
            max_consecutive_losses: 3,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoggingConfig {
    pub log_level: String,
    pub pnl_file: String,
    pub trade_file: String,
    pub json_logging: bool,
    pub max_log_size_mb: u32,
}

impl Default for LoggingConfig {
    fn default() -> Self {
        Self {
            log_level: "info".to_string(),
            pnl_file: "pnl.log".to_string(),
            trade_file: "trades.log".to_string(),
            json_logging: false,
            max_log_size_mb: 100,
        }
    }
}

/// Maker rebates configuration (Jan 2026 program)
/// Optimizes limit orders to earn rebates from taker fees
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MakerRebatesConfig {
    /// Enable maker rebates optimization
    pub enabled: bool,

    /// Minimum estimated rebate % to prioritize entry
    #[serde(with = "rust_decimal::serde::str")]
    pub rebate_estimate_threshold: Decimal,

    /// Track and log daily rebate estimates
    pub track_daily_rebates: bool,

    /// Rebates log file path
    pub rebates_file: String,

    /// Enable market making mode (optional)
    /// Posts bid/ask around midpoint during low-volatility windows
    pub enable_market_making: bool,

    /// Spread for market making (basis points from midpoint)
    pub market_making_spread_bps: u32,

    /// Maximum position for market making (shares per side)
    #[serde(with = "rust_decimal::serde::str")]
    pub market_making_max_position: Decimal,

    /// Quote refresh interval (milliseconds)
    pub quote_refresh_ms: u64,
}

impl Default for MakerRebatesConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            rebate_estimate_threshold: Decimal::new(1, 4), // 0.01%
            track_daily_rebates: true,
            rebates_file: "rebates.log".to_string(),
            enable_market_making: false,
            market_making_spread_bps: 100,
            market_making_max_position: Decimal::new(500, 1), // 50.0
            quote_refresh_ms: 5000,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AuthConfig {
    pub api_key: Option<String>,
    pub api_secret: Option<String>,
    pub passphrase: Option<String>,
    pub funder_address: Option<String>, // For POLY_PROXY: the proxy wallet address
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AppConfig {
    #[serde(default)]
    pub general: GeneralConfig,

    #[serde(default)]
    pub auth: AuthConfig, // Added AuthConfig

    #[serde(default)]
    pub wallet: WalletConfig,

    #[serde(default)]
    pub trading: TradingConfig,

    #[serde(default, rename = "binance_integration")]
    pub binance: BinanceConfig,

    #[serde(default, rename = "risk_management")]
    pub risk: RiskConfig,

    #[serde(default)]
    pub logging: LoggingConfig,

    #[serde(default)]
    pub maker_rebates: MakerRebatesConfig,
}

impl AppConfig {
    /// Load configuration from file and environment variables
    pub fn load<P: AsRef<Path>>(config_path: P) -> Result<Self, ConfigError> {
        let config_content = std::fs::read_to_string(config_path)?;
        let mut config: AppConfig = toml::from_str(&config_content)?;

        // Override with environment variables
        config.apply_env_overrides();

        // Validate configuration
        config.validate()?;

        Ok(config)
    }

    /// Load from default path or create default config
    pub fn load_default() -> Result<Self, ConfigError> {
        let config_path = std::env::current_dir()
            .unwrap_or_default()
            .join("config.toml");

        if config_path.exists() {
            Self::load(&config_path)
        } else {
            let mut config = Self::default();
            config.apply_env_overrides();
            config.validate()?;
            Ok(config)
        }
    }

    /// Apply environment variable overrides
    fn apply_env_overrides(&mut self) {
        // Mode override (POLY_MODE)
        if let Ok(mode) = std::env::var("POLY_MODE") {
            if let Ok(parsed_mode) = mode.parse::<OperatingMode>() {
                self.general.mode = parsed_mode;
            }
        }

        // RPC URL override
        if let Ok(rpc_url) = std::env::var("POLY_RPC_URL") {
            self.general.rpc_url = rpc_url;
        }

        // Per trade shares override
        if let Ok(shares) = std::env::var("POLY_PER_TRADE_SHARES") {
            if let Ok(parsed) = shares.parse::<Decimal>() {
                self.trading.per_trade_shares = parsed;
            }
        }

        // Log level override
        if let Ok(level) = std::env::var("POLY_LOG_LEVEL") {
            self.logging.log_level = level;
        }

        // Binance enabled override
        if let Ok(enabled) = std::env::var("POLY_BINANCE_ENABLED") {
            self.binance.enabled = enabled.to_lowercase() == "true" || enabled == "1";
        }

        // Spot move trigger override
        if let Ok(pct) = std::env::var("POLY_SPOT_MOVE_TRIGGER_PCT") {
            if let Ok(parsed) = pct.parse::<Decimal>() {
                self.trading.spot_move_trigger_pct = parsed;
            }
        }

        // Max daily risk override
        if let Ok(pct) = std::env::var("POLY_MAX_DAILY_RISK_PCT") {
            if let Ok(parsed) = pct.parse::<Decimal>() {
                self.trading.max_daily_risk_pct = parsed;
            }
        }

        // Maker rebates enabled override
        if let Ok(enabled) = std::env::var("POLY_MAKER_REBATES_ENABLED") {
            self.maker_rebates.enabled = enabled.to_lowercase() == "true" || enabled == "1";
        }

        // Market making enabled override
        if let Ok(enabled) = std::env::var("POLY_MARKET_MAKING_ENABLED") {
            self.maker_rebates.enable_market_making = enabled.to_lowercase() == "true" || enabled == "1";
        }

        // L2 Auth Credentials
        if let Ok(key) = std::env::var("POLY_API_KEY") {
            self.auth.api_key = Some(key);
        }
        if let Ok(secret) = std::env::var("POLY_API_SECRET") {
            self.auth.api_secret = Some(secret);
        }
        if let Ok(passphrase) = std::env::var("POLY_PASSPHRASE") {
            self.auth.passphrase = Some(passphrase);
        }
        if let Ok(funder) = std::env::var("POLY_FUNDER_ADDRESS") {
            self.auth.funder_address = Some(funder);
        }
    }

    /// Validate configuration values
    fn validate(&self) -> Result<(), ConfigError> {
        // Validate per_trade_shares is positive
        if self.trading.per_trade_shares <= Decimal::ZERO {
            return Err(ConfigError::ValidationError(
                "per_trade_shares must be positive".to_string()
            ));
        }

        // Validate thresholds are in valid range
        if self.trading.min_profit_threshold <= Decimal::ZERO
            || self.trading.min_profit_threshold > Decimal::ONE
        {
            return Err(ConfigError::ValidationError(
                "min_profit_threshold must be between 0 and 1".to_string()
            ));
        }

        // Validate Binance symbols are lowercase
        for symbol in &self.binance.symbols {
            if symbol != &symbol.to_lowercase() {
                return Err(ConfigError::ValidationError(
                    format!("Binance symbol '{}' must be lowercase", symbol)
                ));
            }
        }

        Ok(())
    }

    /// Check if running in test/simulation mode
    pub fn is_test_mode(&self) -> bool {
        self.general.mode == OperatingMode::Test
    }

    /// Check if running in real/production mode
    pub fn is_real_mode(&self) -> bool {
        self.general.mode == OperatingMode::Real
    }

    /// Get private key from environment (only in real mode)
    pub fn get_private_key(&self) -> Result<String, ConfigError> {
        let key = std::env::var("POLY_PRIVATE_KEY")
            .map_err(|_| ConfigError::MissingEnvVar("POLY_PRIVATE_KEY".to_string()))?;

        // Ensure key starts with 0x
        if !key.starts_with("0x") {
            Ok(format!("0x{}", key))
        } else {
            Ok(key)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config() {
        let config = AppConfig::default();
        assert_eq!(config.general.mode, OperatingMode::Test);
        assert!(!config.binance.symbols.is_empty());
        assert!(config.maker_rebates.enabled);
    }

    #[test]
    fn test_mode_parsing() {
        assert_eq!("test".parse::<OperatingMode>().unwrap(), OperatingMode::Test);
        assert_eq!("real".parse::<OperatingMode>().unwrap(), OperatingMode::Real);
        assert_eq!("production".parse::<OperatingMode>().unwrap(), OperatingMode::Real);
    }
}

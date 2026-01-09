//! Logging configuration and utilities

use crate::config::AppConfig;
use anyhow::Result;
use std::sync::Arc;
use tracing::Level;
use tracing_subscriber::{
    fmt::{self, format::FmtSpan},
    layer::SubscriberExt,
    util::SubscriberInitExt,
    EnvFilter,
};

/// Initialize logging based on configuration
pub fn init_logging(config: &AppConfig) -> Result<()> {
    let level = match config.logging.log_level.to_lowercase().as_str() {
        "trace" => Level::TRACE,
        "debug" => Level::DEBUG,
        "info" => Level::INFO,
        "warn" => Level::WARN,
        "error" => Level::ERROR,
        _ => Level::INFO,
    };

    let filter = EnvFilter::from_default_env()
        .add_directive(format!("poly_rust_arb={}", level).parse()?)
        .add_directive("tokio_tungstenite=warn".parse()?)
        .add_directive("tungstenite=warn".parse()?)
        .add_directive("hyper=warn".parse()?)
        .add_directive("reqwest=warn".parse()?);

    if config.logging.json_logging {
        // JSON format for production
        let fmt_layer = fmt::layer()
            .json()
            .with_target(true)
            .with_thread_ids(true)
            .with_file(true)
            .with_line_number(true);

        tracing_subscriber::registry()
            .with(filter)
            .with(fmt_layer)
            .init();
    } else {
        // Human-readable format for development
        let fmt_layer = fmt::layer()
            .with_target(true)
            .with_thread_ids(false)
            .with_file(false)
            .with_line_number(false)
            .with_span_events(FmtSpan::CLOSE);

        tracing_subscriber::registry()
            .with(filter)
            .with(fmt_layer)
            .init();
    }

    tracing::info!(
        "Logging initialized at {} level (JSON: {})",
        config.logging.log_level,
        config.logging.json_logging
    );

    Ok(())
}

/// Log a trade event to the trade log file
pub fn log_trade(
    trade_id: &str,
    action: &str,
    token: &str,
    price: rust_decimal::Decimal,
    size: rust_decimal::Decimal,
    is_simulated: bool,
) {
    let mode = if is_simulated { "SIM" } else { "REAL" };
    tracing::info!(
        target: "trades",
        trade_id = %trade_id,
        mode = %mode,
        action = %action,
        token = %token,
        price = %price,
        size = %size,
        "[TRADE] {} {} {} @ {} x {}",
        mode, action, token, price, size
    );
}

/// Log a PNL event
pub fn log_pnl(
    trade_id: &str,
    pnl: rust_decimal::Decimal,
    cumulative_pnl: rust_decimal::Decimal,
    is_simulated: bool,
) {
    let mode = if is_simulated { "SIM" } else { "REAL" };
    tracing::info!(
        target: "pnl",
        trade_id = %trade_id,
        mode = %mode,
        pnl = %pnl,
        cumulative = %cumulative_pnl,
        "[PNL] {} Trade {} PNL: {} (Cumulative: {})",
        mode, trade_id, pnl, cumulative_pnl
    );
}

/// Log a significant price move
pub fn log_price_move(
    source: &str,
    symbol: &str,
    direction: &str,
    change_pct: rust_decimal::Decimal,
) {
    tracing::info!(
        target: "prices",
        source = %source,
        symbol = %symbol,
        direction = %direction,
        change_pct = %change_pct,
        "[PRICE] {} {} {} {:.2}%",
        source, symbol, direction, change_pct
    );
}

/// Log a risk event
pub fn log_risk_event(event: &str, details: &str) {
    tracing::warn!(
        target: "risk",
        event = %event,
        details = %details,
        "[RISK] {}: {}",
        event, details
    );
}

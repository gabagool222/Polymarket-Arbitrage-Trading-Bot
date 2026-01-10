//! PolyRustArb Bot - Automated hedging/arbitrage for Polymarket 15-min crypto markets
//!
//! # Overview
//!
//! This bot implements two main strategies:
//!
//! 1. **Hedging Arbitrage**: When Yes_price + No_price < threshold, buy both sides
//!    to lock in profit when the market resolves.
//!
//! 2. **Latency Arbitrage**: Use Binance spot prices to detect moves before Polymarket
//!    odds adjust, and enter positions on the expected winning side.
//!
//! # Usage
//!
//! ```bash
//! # Test mode (simulation, no real trades)
//! POLY_MODE=test cargo run
//!
//! # Real mode (requires POLY_PRIVATE_KEY)
//! POLY_MODE=real POLY_PRIVATE_KEY=0x... cargo run
//! ```

mod binance;
mod config;
mod polymarket;
mod trading;
mod utils;

use anyhow::{Context, Result};
use clap::Parser;
use rust_decimal::Decimal;
use std::sync::Arc;
use tokio::sync::broadcast;
use tracing::{error, info, warn};

use crate::binance::{run_binance_ws, BinanceTick, PriceMove};
use crate::config::{AppConfig, OperatingMode};
use crate::polymarket::websocket::{run_polymarket_ws, OrderBookUpdate, PriceUpdate};
use crate::polymarket::{PolymarketClient, MarketPrices};
use crate::trading::TradingEngine;
use crate::utils::{init_logging, PnlTracker, RebateTracker, SimulationEngine};

/// PolyRustArb - Polymarket Arbitrage Bot
#[derive(Parser, Debug)]
#[command(name = "polyarb")]
#[command(author = "PolyRustArb Team")]
#[command(version = "0.1.0")]
#[command(about = "Automated hedging/arbitrage bot for Polymarket 15-min crypto markets")]
struct Args {
    /// Operating mode: test (simulation) or real (live trading)
    #[arg(short, long, env = "POLY_MODE", default_value = "test")]
    mode: String,

    /// Path to config file
    #[arg(short, long, default_value = "config.toml")]
    config: String,

    /// Per-trade shares (overrides config)
    #[arg(long, env = "POLY_PER_TRADE_SHARES")]
    per_trade_shares: Option<f64>,

    /// Initial balance for simulation (USD)
    #[arg(long, default_value = "314.0")]
    initial_balance: f64,

    /// Log level: trace, debug, info, warn, error
    #[arg(long, env = "POLY_LOG_LEVEL", default_value = "info")]
    log_level: String,

    /// Enable verbose output
    #[arg(short, long)]
    verbose: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    // Load environment variables from .env if present
    let _ = dotenvy::dotenv();

    // Parse CLI arguments
    let args = Args::parse();

    // Load configuration
    let mut config = AppConfig::load(&args.config)
        .unwrap_or_else(|e| {
            eprintln!("Warning: Failed to load config: {}. Using defaults.", e);
            AppConfig::default()
        });

    // Apply CLI overrides
    if let Ok(mode) = args.mode.parse::<OperatingMode>() {
        config.general.mode = mode;
    }
    config.logging.log_level = args.log_level.clone();

    if let Some(shares) = args.per_trade_shares {
        config.trading.per_trade_shares = Decimal::try_from(shares)
            .unwrap_or(Decimal::new(20, 1));
    }

    let config = Arc::new(config);

    // Initialize logging
    init_logging(&config)?;

    // Print startup banner
    print_banner(&config, &args);

    // Run the bot
    run_bot(config, args.initial_balance).await
}

fn print_banner(config: &AppConfig, args: &Args) {
    info!("╔═══════════════════════════════════════════════════════════╗");
    info!("║           PolyRustArb - Polymarket Arbitrage Bot          ║");
    info!("╠═══════════════════════════════════════════════════════════╣");
    info!("║  Mode: {:10}                                        ║",
          config.general.mode.to_string().to_uppercase());
    info!("║  Per Trade: {} shares                                   ║",
          config.trading.per_trade_shares);
    info!("║  Profit Threshold: {}                                   ║",
          config.trading.min_profit_threshold);
    info!("║  Binance Integration: {}                                ║",
          if config.binance.enabled { "Enabled" } else { "Disabled" });
    info!("║  Maker Rebates: {}                                       ║",
          if config.maker_rebates.enabled { "Enabled" } else { "Disabled" });
    info!("║  Initial Balance: ${}                                 ║",
          args.initial_balance);
    info!("╚═══════════════════════════════════════════════════════════╝");

    if config.maker_rebates.enabled {
        info!("");
        info!("💰 MAKER REBATES ENABLED - Earn up to 1.56% on filled limit orders");
    }

    if config.is_test_mode() {
        info!("");
        info!("🧪 RUNNING IN TEST/SIMULATION MODE - NO REAL TRADES WILL BE EXECUTED");
        info!("");
    } else {
        warn!("");
        warn!("⚠️  RUNNING IN REAL MODE - REAL FUNDS WILL BE USED!");
        warn!("");
    }
}

async fn run_bot(config: Arc<AppConfig>, initial_balance: f64) -> Result<()> {
    // Create broadcast channels for inter-task communication
    let (poly_price_tx, mut poly_price_rx) = broadcast::channel::<PriceUpdate>(1000);
    let (poly_book_tx, _poly_book_rx) = broadcast::channel::<OrderBookUpdate>(1000);
    let (binance_tick_tx, _binance_tick_rx) = broadcast::channel::<BinanceTick>(1000);
    let (binance_move_tx, mut binance_move_rx) = broadcast::channel::<PriceMove>(100);

    // Initialize Polymarket client
    let client = Arc::new(
        PolymarketClient::new(Arc::clone(&config))
            .context("Failed to create Polymarket client")?
    );

    // Initialize trading engine
    let trading_engine = Arc::new(TradingEngine::new(
        Arc::clone(&config),
        Arc::clone(&client),
    ));

    // Initialize simulation engine (for test mode)
    let sim_engine = if config.is_test_mode() {
        Some(Arc::new(SimulationEngine::new(
            Arc::clone(&config),
            Decimal::try_from(initial_balance).unwrap_or(Decimal::new(314, 0)),
        )))
    } else {
        None
    };

    // Initialize PNL tracker
    let mut pnl_tracker = PnlTracker::new(Arc::clone(&config));
    let _ = pnl_tracker.load_from_file();

    // Initialize rebate tracker
    let mut rebate_tracker = RebateTracker::new(Arc::clone(&config));
    let _ = rebate_tracker.load_from_file();

    info!("Fetching available 15-min crypto markets...");

    // Fetch markets (active 15-min crypto binary)
    let mut markets = client.fetch_active_crypto_markets().await?;

    if markets.is_empty() {
        warn!("No active 15-min crypto markets found. Will retry...");
    } else {
        info!("Found {} active crypto markets:", markets.len());
        for market in &markets {
            info!("  - {} ({}) [{}]", market.title, market.condition_id, market.asset);
        }
    }

    // Collect token IDs for WebSocket subscriptions
    let mut current_token_ids = PolymarketClient::get_all_token_ids(&markets);

    // Spawn Polymarket WebSocket task
    let poly_config = Arc::clone(&config);
    let poly_tokens = current_token_ids.clone();
    let poly_price_tx_clone = poly_price_tx.clone();
    let poly_book_tx_clone = poly_book_tx.clone();
    
    let mut poly_ws_handle = tokio::spawn(async move {
        if let Err(e) = run_polymarket_ws(
            poly_config,
            poly_tokens,
            poly_price_tx_clone,
            poly_book_tx_clone,
        )
        .await
        {
            error!("Polymarket WebSocket error: {}", e);
        }
    });

    // Spawn Binance WebSocket task (if enabled)
    if config.binance.enabled {
        let binance_config = Arc::clone(&config);
        tokio::spawn(async move {
            if let Err(e) = run_binance_ws(
                binance_config,
                binance_tick_tx,
                binance_move_tx,
            )
            .await
            {
                error!("Binance WebSocket error: {}", e);
            }
        });
    }

    info!("Bot started. Monitoring markets for opportunities...");

    // Main event loop
    let mut last_binance_move: Option<PriceMove> = None;
    let mut stats_interval = tokio::time::interval(std::time::Duration::from_secs(60));
    // Refresh markets every 5 minutes (300 seconds)
    let mut refresh_interval = tokio::time::interval(std::time::Duration::from_secs(300));

    loop {
        tokio::select! {
            // Market Refresh Task
            _ = refresh_interval.tick() => {
                info!("Refreshing 15-min crypto markets...");
                match client.fetch_active_crypto_markets().await {
                    Ok(new_markets) => {
                        let new_token_ids = PolymarketClient::get_all_token_ids(&new_markets);
                        
                        // Check if subscription needs update
                        if new_token_ids != current_token_ids && !new_token_ids.is_empty() {
                            info!("Market change detected. Updating WebSocket subscription...");
                            info!("Old markets: {}, New markets: {}", markets.len(), new_markets.len());
                            
                            // Abort old WS task
                            poly_ws_handle.abort();
                            
                            // Spawn new WS task
                            let poly_config = Arc::clone(&config);
                            let poly_tokens = new_token_ids.clone();
                            let poly_price_tx_clone = poly_price_tx.clone();
                            let poly_book_tx_clone = poly_book_tx.clone();
                            
                            poly_ws_handle = tokio::spawn(async move {
                                if let Err(e) = run_polymarket_ws(
                                    poly_config,
                                    poly_tokens,
                                    poly_price_tx_clone,
                                    poly_book_tx_clone,
                                )
                                .await
                                {
                                    error!("Polymarket WebSocket error: {}", e);
                                }
                            });
                            
                            // Update state
                            markets = new_markets;
                            current_token_ids = new_token_ids;
                        } else {
                            debug!("No market changes detected.");
                        }
                    },
                    Err(e) => {
                        error!("Failed to refresh markets: {}", e);
                    }
                }
            }
            // Handle Polymarket price updates
            Ok(update) = poly_price_rx.recv() => {
                // Find the market for this token
                if let Some(market) = markets.iter().find(|m|
                    m.yes_token_id == update.token_id || m.no_token_id == update.token_id
                ) {
                    // Fetch full market prices using crypto specific function
                    match client.fetch_crypto_market_prices(market).await {
                        Ok(prices) => {
                            // Process with trading engine
                            if let Err(e) = trading_engine.on_market_update(
                                &prices,
                                last_binance_move.as_ref(),
                            ).await {
                                error!("Trading engine error: {}", e);
                            }

                            // Update simulation if in test mode
                            if let Some(ref sim) = sim_engine {
                                let _ = sim.update_orders(&prices).await;
                            }
                        }
                        Err(e) => {
                            warn!("Failed to fetch market prices: {}", e);
                        }
                    }
                }
            }

            // Handle Binance price moves
            Ok(price_move) = binance_move_rx.recv() => {
                info!(
                    "Binance move: {} {} {:.2}% in {}s",
                    price_move.symbol.to_uppercase(),
                    price_move.direction,
                    price_move.change_pct,
                    price_move.window_sec
                );

                last_binance_move = Some(price_move);
            }

            // Periodic stats logging
            _ = stats_interval.tick() => {
                let stats = trading_engine.get_daily_stats().await;
                info!(
                    "Daily Stats: {} trades | W/L: {}/{} | Net PNL: ${}",
                    stats.trades_count,
                    stats.wins,
                    stats.losses,
                    stats.net_pnl
                );

                if let Some(ref sim) = sim_engine {
                    sim.print_summary().await;
                }
            }

            // Graceful shutdown on Ctrl+C
            _ = tokio::signal::ctrl_c() => {
                info!("Shutdown signal received");
                break;
            }
        }
    }

    // Print final summary
    info!("=== FINAL SUMMARY ===");
    let stats = trading_engine.get_daily_stats().await;
    info!("Total Trades: {}", stats.trades_count);
    info!("Wins/Losses: {}/{}", stats.wins, stats.losses);
    info!("Win Rate: {:.1}%", stats.win_rate());
    info!("Net PNL: ${}", stats.net_pnl);

    if let Some(ref sim) = sim_engine {
        sim.print_summary().await;

        // Show simulated rebates
        if config.maker_rebates.enabled {
            let rebates = sim.get_simulated_rebates().await;
            let volume = sim.get_simulated_maker_volume().await;
            info!("=== SIMULATED REBATES ===");
            info!("Maker Volume: ${:.2}", volume);
            info!("Est. Rebates: ${:.4}", rebates);
        }
    }

    // Show rebate tracker summary
    if config.maker_rebates.enabled {
        rebate_tracker.print_summary();
        let _ = rebate_tracker.save_today();
    }

    let perf = pnl_tracker.get_statistics();
    info!("Performance: {}", perf);

    info!("Bot shutdown complete.");

    Ok(())
}

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
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{broadcast, RwLock};
use tracing::{debug, error, info, warn};

use crate::binance::{run_binance_ws, BinanceTick, PriceMove};
use crate::config::{AppConfig, OperatingMode};
use crate::polymarket::websocket::{run_polymarket_ws, OrderBookUpdate, PriceUpdate};
use crate::polymarket::{PolymarketClient, MarketPrices, Market};
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

    // Apply CLI overrides
    if let Some(shares) = args.per_trade_shares {
        config.trading.per_trade_shares = Decimal::try_from(shares)
            .unwrap_or(Decimal::new(20, 1));
    }

    // Handle L2 API Keys in Real Mode
    if config.is_real_mode() {
        if config.auth.api_key.is_none() {
            // No credentials provided - try to derive from private key
            println!("🔑 API Keys missing. Attempting to derive from Private Key...");
            let temp_config = Arc::new(config.clone());
            match PolymarketClient::new(temp_config) {
                Ok(temp_client) => {
                    match temp_client.derive_api_keys().await {
                        Ok(creds) => {
                            println!("✅ API Credentials Derived Successfully!");
                            println!("ℹ️  API Key: {}", creds.api_key.as_deref().unwrap_or("?"));
                            println!("");
                            println!("💡 TIP: Save these credentials to avoid re-deriving:");
                            println!("   POLY_API_KEY={}", creds.api_key.as_deref().unwrap_or(""));
                            println!("   POLY_API_SECRET={}", creds.api_secret.as_deref().unwrap_or(""));
                            println!("   POLY_PASSPHRASE={}", creds.passphrase.as_deref().unwrap_or(""));
                            println!("");
                            config.auth = creds;
                        },
                        Err(e) => {
                            eprintln!("❌ FATAL: Failed to derive API keys: {}", e);
                            eprintln!("⚠️  Cannot proceed in Real Mode without valid API Keys.");
                            eprintln!("");
                            eprintln!("💡 For POLY_PROXY wallets (Magic Link), get credentials from:");
                            eprintln!("   https://polymarket.com → Settings → API Keys");
                            std::process::exit(1);
                        }
                    }
                },
                Err(e) => {
                    eprintln!("❌ Failed to create client: {}", e);
                    std::process::exit(1);
                }
            }
        } else {
            // Credentials provided - trust them and proceed
            println!("🔑 Using provided API credentials");
            println!("   API Key: {}...", &config.auth.api_key.as_deref().unwrap_or("?")[..8]);
            if config.auth.funder_address.is_some() {
                println!("   Mode: POLY_PROXY (Magic Link wallet)");
                println!("   Funder: {}", config.auth.funder_address.as_deref().unwrap_or("?"));
            } else {
                println!("   Mode: EOA (direct wallet)");
            }
        }
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
        warn!("╔══════════════════════════════════════════════════════════════╗");
        warn!("║  ⚠️  WARNING: REAL MODE - REAL FUNDS WILL BE USED!           ║");
        warn!("║                                                              ║");
        warn!("║  Safety limits active:                                       ║");
        warn!("║  • Max 100 trades per day                                    ║");
        warn!("║  • Circuit breaker at -$50 loss                              ║");
        warn!("║  • Press Ctrl+C NOW to abort if this is unintended!         ║");
        warn!("╚══════════════════════════════════════════════════════════════╝");
        warn!("");
        warn!("⏳ Starting in 10 seconds... (Ctrl+C to abort)");
        for i in (1..=10).rev() {
            warn!("   {}...", i);
            std::thread::sleep(std::time::Duration::from_secs(1));
        }
        warn!("🚀 STARTING REAL TRADING NOW!");
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
        info!("Found {} active crypto markets", markets.len());
        
        // Convert CryptoMarket to Market for trading engine cache
        let engine_markets: Vec<Market> = markets.iter().map(|cm| {
            Market {
                condition_id: cm.condition_id.clone(),
                question_id: cm.condition_id.clone(), // Use same as condition_id
                tokens: vec![],
                slug: cm.slug.clone(),
                question: cm.title.clone(),
                end_date_iso: cm.end_time.clone(),
                active: true,
                closed: false,
                accepting_orders: cm.accepting_orders,
                clob_token_ids: vec![cm.yes_token_id.clone(), cm.no_token_id.clone()],
            }
        }).collect();
        
        // Update trading engine cache
        trading_engine.update_market_cache(engine_markets).await;
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

    // === PRICE CACHE ===
    // Store prices from WebSocket to avoid re-fetching orderbooks
    // Key: token_id, Value: (best_bid, best_ask, last_price, timestamp)
    type PriceData = (Option<Decimal>, Option<Decimal>, Decimal, chrono::DateTime<chrono::Utc>);
    let price_cache: Arc<RwLock<HashMap<String, PriceData>>> = Arc::new(RwLock::new(HashMap::new()));
    
    // === THROTTLE MAP ===
    // Prevent processing same market more than once per 500ms
    let mut last_process_time: HashMap<String, std::time::Instant> = HashMap::new();
    let throttle_ms = 500; // Minimum ms between processing same market

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
                            
                            // Update trading engine cache
                            let engine_markets: Vec<Market> = markets.iter().map(|cm| {
                                Market {
                                    condition_id: cm.condition_id.clone(),
                                    question_id: cm.condition_id.clone(),
                                    tokens: vec![],
                                    slug: cm.slug.clone(),
                                    question: cm.title.clone(),
                                    end_date_iso: cm.end_time.clone(),
                                    active: true,
                                    closed: false,
                                    accepting_orders: cm.accepting_orders,
                                    clob_token_ids: vec![cm.yes_token_id.clone(), cm.no_token_id.clone()],
                                }
                            }).collect();
                            trading_engine.update_market_cache(engine_markets).await;
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
                // Update price cache from WebSocket
                {
                    let mut cache = price_cache.write().await;
                    cache.insert(
                        update.token_id.clone(),
                        (update.best_bid, update.best_ask, update.price, update.timestamp)
                    );
                }

                // Find the market for this token
                if let Some(market) = markets.iter().find(|m|
                    m.yes_token_id == update.token_id || m.no_token_id == update.token_id
                ) {
                    // Throttle: skip if we processed this market recently
                    let now = std::time::Instant::now();
                    if let Some(last_time) = last_process_time.get(&market.condition_id) {
                        if now.duration_since(*last_time).as_millis() < throttle_ms as u128 {
                            continue; // Skip, processed too recently
                        }
                    }
                    last_process_time.insert(market.condition_id.clone(), now);
                    
                    // Try to build prices from cache first
                    let cache = price_cache.read().await;
                    
                    let yes_data = cache.get(&market.yes_token_id);
                    let no_data = cache.get(&market.no_token_id);
                    
                    // Store whether we have data before processing
                    let yes_has_data = yes_data.is_some();
                    let no_has_data = no_data.is_some();
                    
                    // Calculate prices from cached bid/ask or last_price
                    let yes_price = yes_data.and_then(|(bid, ask, last_price, _)| {
                        // Priority: mid_price from bid/ask > last_price
                        match (bid, ask) {
                            (Some(b), Some(a)) => Some((*b + *a) / Decimal::new(2, 0)),
                            (Some(b), None) => Some(*b),
                            (None, Some(a)) => Some(*a),
                            (None, None) => Some(*last_price),
                        }
                    });
                    
                    let no_price = no_data.and_then(|(bid, ask, last_price, _)| {
                        match (bid, ask) {
                            (Some(b), Some(a)) => Some((*b + *a) / Decimal::new(2, 0)),
                            (Some(b), None) => Some(*b),
                            (None, Some(a)) => Some(*a),
                            (None, None) => Some(*last_price),
                        }
                    });
                    
                    drop(cache); // Release read lock
                    
                    // Build MarketPrices from cache or skip if incomplete
                    let prices = if let (Some(yp), Some(np)) = (yes_price, no_price) {
                        // Use cached prices
                        if yp > Decimal::ZERO && np > Decimal::ZERO {
                            debug!(
                                "Using cached prices for {}: Yes={:.4} No={:.4} Sum={:.4}",
                                market.asset, yp, np, yp + np
                            );
                            MarketPrices {
                                condition_id: market.condition_id.clone(),
                                yes_price: yp,
                                no_price: np,
                                yes_token_id: market.yes_token_id.clone(),
                                no_token_id: market.no_token_id.clone(),
                                timestamp: chrono::Utc::now(),
                            }
                        } else {
                            // Zero prices from cache - skip this market
                            debug!("Cached prices are zero for {}, waiting for real data...", market.asset);
                            continue;
                        }
                    } else {
                        // Not enough cached data - skip and wait for WebSocket to populate both sides
                        // DO NOT fallback to orderbook API (often returns empty/stale data)
                        debug!(
                            "Waiting for WebSocket data on {} (Yes: {}, No: {})",
                            market.asset,
                            yes_has_data,
                            no_has_data
                        );
                        continue;
                    };

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

                // Note: SimulationEngine's internal counters aren't used (trades go through TradingEngine)
                // Daily Stats above already shows the accurate trade data
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

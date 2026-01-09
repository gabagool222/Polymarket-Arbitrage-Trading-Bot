# PolyRustArb Bot

Automated hedging/arbitrage bot for Polymarket 15-minute crypto markets (BTC/ETH/SOL Up/Down).

## Overview

PolyRustArb implements two proven trading strategies for low-risk, consistent profits:

### 1. Hedging Arbitrage (Core Strategy)
When `Yes_price + No_price < threshold` (e.g., 0.40 + 0.50 = 0.90 < 0.95), buy both sides to lock in guaranteed profit when the market resolves.

- **Leg 1**: Buy the cheaper side first (typically during dumps)
- **Leg 2**: Hedge with the opposite side
- **Exit**: Market resolution pays $1 per correct share

### 2. Latency Arbitrage (Yield Boost)
Use Binance spot prices to detect significant moves before Polymarket odds adjust.

- Stream real-time prices from Binance WebSocket
- Detect moves >0.5% within 5-second windows
- Enter positions on the expected winning side before odds catch up

## Features

- **Test/Simulation Mode**: Run with real-time data but no actual trades
- **Real Trading Mode**: Full execution with order signing
- **Configurable**: All parameters via `config.toml` or environment variables
- **Risk Management**: Daily limits, consecutive loss protection, cooldown periods
- **PNL Tracking**: Detailed trade logging and performance statistics
- **Low Latency**: Async Rust with WebSocket streaming

## Quick Start

### Prerequisites

- Rust 1.75+ (install via [rustup](https://rustup.rs/))
- Polygon wallet with USDC for trading (real mode only)

### Installation

```bash
# Clone the repository
git clone https://github.com/yourusername/PolyRustArb-BOT.git
cd PolyRustArb-BOT

# Build the project
cargo build --release
```

### Configuration

```bash
# Copy example environment file
cp .env.example .env

# Edit configuration
nano config.toml
```

### Running

```bash
# Test mode (simulation - no real trades)
POLY_MODE=test cargo run --release

# Or with CLI flags
cargo run --release -- --mode test --per-trade-shares 10

# Real mode (requires private key)
POLY_MODE=real POLY_PRIVATE_KEY=0x... cargo run --release
```

## Configuration

### config.toml

```toml
[general]
mode = "test"                  # "test" or "real"
rpc_url = "https://polygon-rpc.com"

[trading]
per_trade_shares = 2.0         # ~$1 at $0.50 price
min_profit_threshold = 0.95    # Sum Yes+No < this triggers hedge
dump_trigger_pct = 0.15        # % drop for averaging down
spot_move_trigger_pct = 0.5    # Binance move % for latency trigger
entry_window_min = 2           # Only trade first N minutes of round
max_legs_timeout_sec = 420     # 7 minutes max for hedge completion

[binance_integration]
enabled = true
symbols = ["btcusdt", "ethusdt", "solusdt"]
move_window_sec = 5

[risk_management]
max_position_per_market = 1000.0
max_daily_risk_pct = 20.0
max_consecutive_losses = 3
```

### Environment Variables

All config values can be overridden via environment variables with `POLY_` prefix:

| Variable | Description | Default |
|----------|-------------|---------|
| `POLY_MODE` | Operating mode | `test` |
| `POLY_PRIVATE_KEY` | Wallet private key (required for real mode) | - |
| `POLY_PER_TRADE_SHARES` | Shares per trade | `2.0` |
| `POLY_LOG_LEVEL` | Log verbosity | `info` |
| `POLY_BINANCE_ENABLED` | Enable Binance integration | `true` |

## Trade Size Examples

Configure `per_trade_shares` based on your target trade size:

| Target $ | Price | Shares |
|----------|-------|--------|
| $1 | $0.50 | 2 |
| $5 | $0.50 | 10 |
| $10 | $0.50 | 20 |
| $50 | $0.50 | 100 |
| $100 | $0.50 | 200 |

## Architecture

```
src/
├── main.rs              # Entry point, CLI, main event loop
├── config/              # Configuration loading
├── polymarket/          # Polymarket CLOB API & WebSocket
│   ├── client.rs        # REST API client
│   ├── websocket.rs     # Real-time price streaming
│   └── types.rs         # Data structures
├── binance/             # Binance price integration
│   └── mod.rs           # WebSocket streaming, move detection
├── trading/             # Trading logic
│   ├── strategy.rs      # Hedging & latency strategies
│   ├── engine.rs        # Trade execution orchestration
│   └── types.rs         # Trade/position structures
└── utils/               # Utilities
    ├── logging.rs       # Tracing setup
    ├── pnl.rs           # PNL tracking
    └── simulation.rs    # Test mode simulation
```

## Risk Disclosure

This bot is for educational purposes. Trading crypto prediction markets involves significant risk:

- Markets can be illiquid
- Prices can move against you
- Technical failures can occur
- Only trade what you can afford to lose

Always start in **test mode** and with small amounts in real mode.

## License

MIT License - See [LICENSE](LICENSE) for details.

## Acknowledgments

- Inspired by strategies from successful Polymarket traders
- Built on [rs-clob-client](https://github.com/Polymarket/rs-clob-client) concepts
- Uses Binance public WebSocket API for price data

# Polymarket Arbitrage Bot

Automated hedging and arbitrage bot for **Polymarket** 15-minute crypto binary markets (BTC, ETH, SOL Up/Down). Built in Rust for speed and reliability.

**Author:** [@gabagool222](https://t.me/gabagool222)  
**Repository:** [github.com/gabagool222/Polymarket-Arbitrage-Trading-Bot](https://github.com/gabagool222/Polymarket-Arbitrage-Trading-Bot)

---

## What This Bot Does

The bot trades **15-minute binary markets** on Polymarket (e.g. “Will BTC go up in the next 15 min?”). It uses two main ideas:

1. **Hedging arbitrage** — When Yes + No prices add up to less than $1, you can buy both sides and lock in profit when the market  settles.
2. **Latency / signal arbitrage** — Uses Binance (and optionally Bybit) price moves to enter before Polymarket odds fully adjust.

For a **beginner-friendly explanation** of how the strategies work and how to use the bot, see the **[docs](docs/)** folder.

---

## Quick Links

| Document | Description |
|----------|-------------|
| [**Getting Started**](docs/01-getting-started.md) | Prerequisites, install, first run (test & real mode) |
| [**Trading Strategy Guide**](docs/02-strategy-guide.md) | How hedging and latency arbitrage work, in plain language |
| [**Configuration**](docs/03-configuration.md) | `config.toml` and environment variables |
| [**Risk & Safety**](docs/04-risk-and-safety.md) | Risk disclosure, limits, and best practices |

---

## Quick Start

### Prerequisites

- **Rust** 1.75+ ([rustup](https://rustup.rs/))
- **Polygon wallet** with USDC (for real trading)
- Optional: Python 3 for signing scripts

### Install & Run

```bash
# Clone this repo
git clone https://github.com/gabagool222/Polymarket-Arbitrage-Trading-Bot.git
cd Polymarket-Arbitrage-Trading-Bot

# Build release binary
cargo build --release

# Copy env template and edit
cp .env.example .env
# Edit .env and config.toml as needed (see docs/03-configuration.md)

# Test mode (no real trades — recommended first)
POLY_MODE=test cargo run --release

# Real mode (requires POLY_PRIVATE_KEY and API credentials in .env)
POLY_MODE=real cargo run --release
```

The built binary is `target/release/pmarb` (Polymarket Arbitrage).

---

## Configuration Summary

| What | Where |
|------|--------|
| Mode | `POLY_MODE=test` or `real` |
| Wallet / API | `POLY_PRIVATE_KEY`, `POLY_API_KEY`, `POLY_API_SECRET`, `POLY_PASSPHRASE` (see `.env.example`) |
| Trading size | `per_trade_shares` in `config.toml` or `POLY_PER_TRADE_SHARES` |
| Log level | `POLY_LOG_LEVEL` (e.g. `info`, `debug`) |

Full list and explanations: **[docs/03-configuration.md](docs/03-configuration.md)**.

---

## Project Structure

```
Polymarket-Arbitrage-Trading-Bot/
├── src/
│   ├── main.rs           # Entry point, CLI, event loop
│   ├── config/           # Configuration loading
│   ├── polymarket/       # Polymarket CLOB API & WebSocket
│   ├── binance/          # Binance price stream (latency signals)
│   ├── bybit/            # Bybit liquidation stream (optional)
│   ├── trading/         # Strategy & execution engine
│   └── utils/            # Logging, PNL, rebates, simulation
├── scripts/              # Signing, creds, monitor
├── docs/                 # User & strategy documentation
├── config.toml           # Main config file
├── .env.example          # Env template (copy to .env)
└── README.md             # This file
```

---

## Risk Disclosure

Trading prediction markets involves real risk. Markets can be illiquid, prices can move against you, and technical issues can occur. This bot is for **educational use**; only risk what you can afford to lose. Always start in **test mode** and with small size in real mode. See **[docs/04-risk-and-safety.md](docs/04-risk-and-safety.md)** for more.

---

## License

MIT. See [LICENSE](LICENSE) for details.

## Contact

- **Telegram:** [@gabagool222](https://t.me/gabagool222)  
- **GitHub:** [gabagool222/Polymarket-Arbitrage-Trading-Bot](https://github.com/gabagool222/Polymarket-Arbitrage-Trading-Bot)

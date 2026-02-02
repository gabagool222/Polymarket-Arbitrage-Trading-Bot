# Getting Started — Polymarket Arbitrage Bot

This guide walks you through **installing** and **running** the bot for the first time, from prerequisites to your first test run.

---

## Who This Is For

- You want to run the bot in **test mode** (no real money) or **real mode** (live trading).
- You're okay using the terminal and editing a config file.
- You have (or will get) a Polymarket account and Polygon wallet for real trading.

---

## Prerequisites

### Required for Everyone

| Requirement | Purpose |
|-------------|---------|
| **Rust 1.75+** | Build and run the bot. Install via [rustup](https://rustup.rs/). |
| **Git** | Clone the repository. |

### Required for Real Trading Only

| Requirement | Purpose |
|-------------|---------|
| **Polygon wallet** | Holds USDC; used to trade on Polymarket. |
| **USDC on Polygon** | Funds for placing orders. |
| **Polymarket API credentials** | The bot uses API key + secret + passphrase (or derives them from your private key). |
| **Python 3** (optional) | Used by signing scripts if you rely on the included Python tooling. |

Check Rust:

```bash
rustc --version
# Should show 1.75 or higher
```

---

## Step 1: Clone the Repository

```bash
git clone https://github.com/gabagool222/Polymarket-Arbitrage-Trading-Bot.git
cd Polymarket-Arbitrage-Trading-Bot
```

---

## Step 2: Build the Bot

```bash
cargo build --release
```

This produces the binary at `target/release/pmarb`. You can run it with:

```bash
./target/release/pmarb --help
```

Or use `cargo run --release` (see below).

---

## Step 3: Configure (Minimum)

1. **Copy the example environment file**

   ```bash
   cp .env.example .env
   ```

2. **Edit `.env`** (use any text editor)

   - For **test mode** you only need:
     - `POLY_MODE=test`
   - For **real mode** you will need:
     - `POLY_MODE=real`
     - `POLY_PRIVATE_KEY=0x...` (your Polygon wallet private key), **or**
     - Polymarket API credentials: `POLY_API_KEY`, `POLY_API_SECRET`, `POLY_PASSPHRASE` (and `POLY_FUNDER_ADDRESS` if using a proxy wallet).

3. **Optional:** Edit `config.toml` in the project root to change:
   - Trade size (`per_trade_shares`)
   - Profit threshold (`min_profit_threshold`)
   - Binance/Bybit integration, risk limits, etc.

Details: [Configuration](03-configuration.md).

---

## Step 4: Run in Test Mode First (Recommended)

Test mode uses **real market data** but **does not place real orders**. It’s the safe way to confirm the bot runs and logs as expected.

```bash
POLY_MODE=test cargo run --release
```

Or, if you already built:

```bash
POLY_MODE=test ./target/release/pmarb
```

You should see:

- A startup banner with mode **TEST**
- Logs about fetching markets, WebSocket connections, and price updates
- A line like: **RUNNING IN TEST/SIMULATION MODE - NO REAL TRADES WILL BE EXECUTED**

Let it run for a few minutes, then stop with **Ctrl+C**. The final summary will show simulated stats.

---

## Step 5: Run in Real Mode (Only When Ready)

Real mode **places real orders** and uses real USDC. Only switch after:

- You’ve run test mode successfully.
- You’ve read [Risk & Safety](04-risk-and-safety.md).
- Your `.env` has the correct private key or API credentials and (if needed) funder address.

```bash
POLY_MODE=real cargo run --release
```

The bot will:

- Show a **WARNING** that real funds will be used.
- Wait about 10 seconds before starting (so you can abort with Ctrl+C if needed).
- Then connect to Polymarket and run the strategies.

Keep an eye on logs and your Polymarket position/balance.

---

## Command-Line Options

| Option | Description | Example |
|--------|-------------|---------|
| `--mode` | Override mode (test/real) | `--mode test` |
| `--config` | Config file path | `--config config.toml` |
| `--per-trade-shares` | Override trade size | `--per-trade-shares 5` |
| `--initial-balance` | Starting balance for simulation (test mode) | `--initial-balance 100` |
| `--log-level` | Log verbosity | `--log-level debug` |
| `-v, --verbose` | Verbose output | `-v` |

Example:

```bash
cargo run --release -- --mode test --log-level debug
```

---

## What’s Next?

- **[Trading Strategy Guide](02-strategy-guide.md)** — Understand how hedging and latency arbitrage work.
- **[Configuration](03-configuration.md)** — Tune trade size, thresholds, and features.
- **[Risk & Safety](04-risk-and-safety.md)** — Limits, circuit breakers, and best practices.

---

## Troubleshooting

| Problem | What to try |
|--------|--------------|
| Build fails | Run `rustup update` and ensure you’re in the project root with `Cargo.toml`. |
| “Failed to load config” | Ensure `config.toml` exists next to the binary (or pass `--config /path/to/config.toml`). |
| No markets found | Polymarket may have no active 15-min crypto markets at that time; the bot will retry. |
| API / auth errors in real mode | Check `POLY_PRIVATE_KEY` or `POLY_API_KEY` / `POLY_API_SECRET` / `POLY_PASSPHRASE` and, if applicable, `POLY_FUNDER_ADDRESS`. |
| Bot exits immediately | Check logs for errors; often config or env (e.g. missing key or invalid value). |

For more help: **Telegram** [@gabagool222](https://t.me/gabagool222).

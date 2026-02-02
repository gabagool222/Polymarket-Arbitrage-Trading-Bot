# Configuration — Polymarket Arbitrage Bot

This page describes how to configure the bot: **config.toml** and **environment variables**.

---

## Overview

- **config.toml** — Main file for trading parameters, APIs, risk, and logging.
- **.env** — Secrets and overrides (private key, API credentials, mode). Never commit `.env`.

Environment variables override config file values. Many use the **POLY_** prefix (e.g. `POLY_MODE`, `POLY_PER_TRADE_SHARES`).

---

## Environment Variables (.env)

Copy `.env.example` to `.env` and fill in values.

### Mode & Credentials

| Variable | Required | Description |
|----------|----------|-------------|
| `POLY_MODE` | Yes | `test` (simulation) or `real` (live trading). |
| `POLY_PRIVATE_KEY` | Real mode* | Polygon wallet private key (e.g. `0x...`). Used to sign and optionally derive API creds. |
| `POLY_API_KEY` | Real mode* | Polymarket CLOB API key (if not derived from key). |
| `POLY_API_SECRET` | Real mode* | Polymarket API secret. |
| `POLY_PASSPHRASE` | Real mode* | Polymarket API passphrase. |
| `POLY_FUNDER_ADDRESS` | If proxy wallet | Funder address for Polymarket proxy (magic link) wallets. |

\* Either provide `POLY_PRIVATE_KEY` (bot can derive API creds) or provide `POLY_API_KEY` + `POLY_API_SECRET` + `POLY_PASSPHRASE`.

### Trading & Logging

| Variable | Description | Example |
|----------|-------------|---------|
| `POLY_PER_TRADE_SHARES` | Override shares per trade. | `5` |
| `POLY_LOG_LEVEL` | Log level. | `info`, `debug`, `warn`, `error` |
| `POLY_BINANCE_ENABLED` | Enable Binance price stream. | `true` / `false` |
| `POLY_MAKER_REBATES_ENABLED` | Enable maker rebates logic. | `true` / `false` |
| `POLY_ENABLE_CICIL_MODE` | Incremental hedging (small steps). | `true` / `false` |
| `POLY_CICIL_SHARE_SIZE` | Shares per cicil step. | `1` |
| `POLY_CICIL_WAIT_SECONDS` | Seconds between cicil steps. | `15` |

---

## config.toml — Main Sections

### [general]

| Option | Description | Example |
|--------|-------------|---------|
| `mode` | `test` or `real`. | `test` |
| `rpc_url` | Polygon RPC URL. | `https://polygon-rpc.com` |
| `clob_api_url` | Polymarket CLOB API base. | `https://clob.polymarket.com` |
| `gamma_api_url` | Polymarket Gamma API (markets). | `https://gamma-api.polymarket.com` |
| `clob_ws_url` | Polymarket WebSocket URL. | `wss://ws-subscriptions-clob.polymarket.com/ws/market` |

### [trading]

| Option | Description | Typical |
|--------|-------------|---------|
| `per_trade_shares` | Max shares per side per hedge. | `3`–`20` |
| `min_profit_threshold` | Enter hedge only when Yes+No &lt; this (e.g. 0.98 = 2% edge). | `0.98` |
| `min_price_sum` | Ignore if Yes+No below this (stale data filter). | `0.90` |
| `entry_window_min` | Only trade in first N minutes of the 15-min round. | `10` |
| `max_legs_timeout_sec` | Max seconds to complete the second leg. | `120` |
| `leg2_price_offset` | Offset for Leg 2 limit price (maker). | `0.02` |
| `enable_cicil_mode` | Use small incremental hedge steps. | `true` |
| `cicil_share_size` | Shares per cicil step. | `1.0` |
| `max_cicil_count` | Max cicil iterations per market. | `3` |
| `cicil_wait_seconds` | Wait between cicil steps. | `15` |
| `min_balance_usd` | Don’t trade if balance below this. | `10.0` |
| `require_balance_check` | Check balance before each trade. | `true` |

### [binance_integration]

| Option | Description |
|--------|-------------|
| `enabled` | Turn Binance price stream on/off. |
| `symbols` | Pairs to stream (e.g. `["btcusdt","ethusdt","solusdt"]`). |
| `move_window_sec` | Window (seconds) to detect a “move” for latency logic. |

### [bybit_liquidation]

| Option | Description |
|--------|-------------|
| `enabled` | Use Bybit liquidation stream. |
| `symbols` | Symbols to monitor (e.g. `["BTCUSDT","ETHUSDT"]`). |
| `cascade_window_sec` | Window for cascade detection. |
| `cascade_threshold_usd` | Min liquidation volume (USD) to treat as cascade. |
| `cascade_profit_threshold` | Tighter profit threshold when cascade is detected. |

### [risk_management]

| Option | Description |
|--------|-------------|
| `max_position_per_market` | Max position size per market. |
| `max_total_exposure` | Max total exposure across markets. |
| `stop_loss_pct` | Stop-loss percentage. |
| `loss_cooldown_sec` | Pause after a loss (seconds). |
| `max_consecutive_losses` | Pause after this many losses in a row. |

(If your config also has `max_trades_per_day` or `circuit_breaker_loss`, those further limit real trading.)

### [logging]

| Option | Description |
|--------|-------------|
| `log_level` | `trace`, `debug`, `info`, `warn`, `error`. |
| `pnl_file` | PNL log file path. |
| `trade_file` | Trade log file path. |
| `rebates_file` | Rebates log (when maker rebates enabled). |

### [maker_rebates]

| Option | Description |
|--------|-------------|
| `enabled` | Use maker rebate optimization. |
| `rebate_estimate_threshold` | Min rebate % to prioritize. |
| `track_daily_rebates` | Log daily rebate estimates. |

---

## Trade Size Examples

Rough mapping of **per_trade_shares** to notional (assuming ~$0.50 per share):

| Target $ per side | Approx. shares |
|-------------------|----------------|
| ~$1  | 2  |
| ~$5  | 10 |
| ~$10 | 20 |
| ~$50 | 100 |

Start small (e.g. 2–5 shares) in real mode and increase only after you’re comfortable.

---

## Quick Reference

- **Test run:** `POLY_MODE=test` and no real credentials needed.
- **Real run:** `POLY_MODE=real` plus `POLY_PRIVATE_KEY` or full API credentials in `.env`.
- **Safer entry:** Higher `min_profit_threshold` (e.g. 0.98) and lower `per_trade_shares`.
- **More aggressive:** Lower `min_profit_threshold`, higher shares; increases risk.

For risk and limits, see [Risk & Safety](04-risk-and-safety.md).

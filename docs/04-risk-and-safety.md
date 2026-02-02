# Risk & Safety — Polymarket Arbitrage Bot

This document covers **risk disclosure**, **built-in safety limits**, and **best practices** so you can use the bot with your eyes open.

---

## Risk Disclosure

Trading **prediction markets** and **crypto** involves real financial risk. By using this bot (especially in real mode), you accept that:

- **Markets can be illiquid** — You may not get the price you want, or may not be able to close a position in time.
- **Prices move fast** — Slippage and partial fills can turn an “arbitrage” into a loss (e.g. one leg fills, the other doesn’t).
- **Technical failures happen** — Network issues, API downtime, or bugs can prevent orders from executing or cause unexpected behavior.
- **You can lose money** — Even with hedging, execution risk, fees, and extreme moves can lead to losses.

This bot is for **educational and experimental use**. Only risk capital you can afford to lose. The authors are not responsible for your trading losses.

---

## Built-in Safety Mechanisms

The bot includes several safeguards; understanding them helps you use it responsibly.

| Mechanism | What it does |
|-----------|----------------|
| **Test mode** | No real orders; uses live data for simulation. Always validate here first. |
| **One trade per market** | Tries to avoid double-entering the same market (e.g. in-memory tracking). |
| **Leg timeout** | If the second leg (hedge) doesn’t fill within `max_legs_timeout_sec`, the bot can try to exit or rebalance instead of staying “naked.” |
| **Balance check** | Optional check that balance is above `min_balance_usd` before trading. |
| **Daily / circuit limits** | Configurable limits such as max trades per day and circuit breaker (e.g. stop all trading if daily loss exceeds X). |
| **Consecutive loss cooldown** | After `max_consecutive_losses`, the bot can pause for `loss_cooldown_sec`. |

These reduce but **do not eliminate** risk. You still need to monitor the bot and your positions.

---

## Best Practices

### Before Going Live

1. **Run in test mode** — Confirm the bot runs, connects, and logs as expected.
2. **Read the strategy** — Understand [how hedging and latency arbitrage work](02-strategy-guide.md).
3. **Start small** — Use low `per_trade_shares` and small capital until you’re comfortable.
4. **Secure secrets** — Never commit `.env` or private keys; use env vars or a secrets manager.

### While Running

1. **Monitor logs** — Watch for errors, failed orders, or unexpected behavior.
2. **Check Polymarket** — Periodically verify positions and balance match what you expect.
3. **Respect circuit breakers** — If the bot stops due to daily loss limit, don’t disable safety to “rescue” a bad day without understanding the risk.

### Operational

1. **Keep config under version control** — Track `config.toml` (without secrets); keep `.env` only locally.
2. **Back up logs** — PNL and trade logs help you analyze performance and debug.
3. **Update carefully** — After pulling code or config changes, run test mode again before real mode.

---

## Summary

- **Risk:** Real; you can lose money. Use only capital you can afford to lose.
- **Safety:** Test mode, per-market limits, timeouts, balance checks, and optional daily/circuit limits.
- **Best practice:** Test first, start small, monitor, and keep secrets and config under control.

For setup and strategy, see [Getting Started](01-getting-started.md) and [Trading Strategy Guide](02-strategy-guide.md). For configuration details, see [Configuration](03-configuration.md).

# Trading Strategy Guide — For Beginners

This document explains **how the bot makes trading decisions** in plain language. No math required to get the main idea.

---

## What Markets Does the Bot Trade?

The bot focuses on **15-minute binary markets** on Polymarket, for example:

- “Will **BTC** go up in the next 15 min?”
- “Will **ETH** go up in the next 15 min?”
- “Will **SOL** go up in the next 15 min?”

Each market has two outcomes:

- **Yes** — the event happens (e.g. BTC goes up).
- **No** — the event does not happen (e.g. BTC goes down or flat).

You can buy **Yes** shares and/or **No** shares. When the 15 minutes end, the market **resolves**:

- If **Yes** wins, each Yes share pays **$1** and each No share pays **$0**.
- If **No** wins, each No share pays **$1** and each Yes share pays **$0**.

So you always get **$1 per winning share** and **$0 per losing share**.

---

## Strategy 1: Hedging Arbitrage (Main Strategy)

### The Core Idea

In a fair world, **Yes price + No price ≈ $1**, because one of the two must win. Sometimes the market prices get out of line so that:

**Yes price + No price &lt; $1**

Example: Yes = $0.48, No = $0.50 → total = $0.98.

If you **buy one Yes and one No** at those prices:

- You spend: $0.48 + $0.50 = **$0.98**.
- When the market resolves, **one** of the two shares pays $1, the other $0.
- You always receive **$1** in total.
- Profit: $1.00 − $0.98 = **$0.02** per “pair” (one Yes + one No).

That’s **hedging arbitrage**: you don’t bet on direction; you lock in profit by covering both outcomes when the total cost is below $1.

### What the Bot Does

1. **Watches** live Yes/No prices (e.g. from order books).
2. **Checks** if `Yes_price + No_price` is below a threshold you set (e.g. 0.98).
3. **If yes**, it tries to **buy both sides** (Yes and No) so that when the market resolves, it gets $1 per pair and keeps the difference as profit.

So the bot is not “guessing” up or down; it’s **capturing mispricings** where the two sides together cost less than $1.

### Why Isn’t It Always 100% Safe?

- **Slippage:** Prices can move between “we decide to trade” and “order fills.”
- **Fees:** Trading fees reduce profit.
- **Execution risk:** One side might fill and the other not (e.g. “naked” position). The bot uses logic (e.g. parallel orders, timeouts, maker pricing) to reduce this.

So we only enter when the **edge** (how much below $1 we can buy) is big enough to absorb slippage and fees. That’s what **min_profit_threshold** (and related settings) do.

---

## Strategy 2: Latency / Signal Arbitrage (Optional)

### The Idea

Sometimes **spot prices** (e.g. on Binance) move **before** Polymarket’s Yes/No odds fully adjust. If we see a fast move in BTC price, we might be able to:

- Buy **Yes** if we think the move “up” will be reflected in Polymarket next, or  
- Buy **No** (or avoid Yes) if we think the move is over.

This is **directional** and **speculative**: we’re betting that Polymarket will catch up to the move we saw elsewhere. The bot can use:

- **Binance** spot price moves (e.g. “BTC moved more than X% in Y seconds”).
- **Bybit** liquidation data (e.g. large liquidations in one direction) as an extra signal.

When such a “move” or “cascade” is detected, the bot may **relax the hedging threshold** or **favor one side** (Yes or No) for a short time. This is more aggressive and optional; you can disable Binance/Bybit in config if you want only pure hedging.

### In Short

- **Hedging arbitrage:** “Yes + No &lt; $1 → buy both, lock in profit.” (Main strategy.)
- **Latency/signal:** “Spot or liquidations moved a lot → maybe Polymarket hasn’t priced it in yet; adjust behavior or bias one side.” (Optional.)

---

## Key Concepts in the Bot

| Concept | Meaning |
|--------|--------|
| **Leg 1 / Leg 2** | In a hedge, “Leg 1” is often the first side we buy (e.g. Yes), “Leg 2” is the other side (No). The bot tries to get both filled so you’re never stuck with only one side. |
| **min_profit_threshold** | Only enter a hedge when Yes_price + No_price is **below** this (e.g. 0.98 = 2% “edge”). Higher = stricter = fewer but safer trades. |
| **Test mode** | Uses real prices but **does not send real orders**. Good for learning and checking that the bot runs. |
| **Real mode** | Sends real orders and uses real USDC. Use only after you understand the strategy and risks. |
| **Cicil mode** | Building the hedge in small steps (e.g. 1 share at a time) instead of one big order per side. Can help with fills and risk. |
| **Maker rebates** | Placing limit orders that sit in the book (maker) can earn rebates; the bot can prefer maker-style pricing where possible. |

---

## How This Fits Together

1. Bot connects to Polymarket (and optionally Binance/Bybit).
2. It gets **live prices** for 15-min Yes/No markets.
3. For **hedging:**  
   - It checks if Yes + No &lt; threshold.  
   - If yes, it places (or steps into) both sides with the configured size and logic (e.g. parallel orders, cicil, timeouts).
4. For **latency/signal:**  
   - It watches spot/liquidations; when a big move is detected, it may temporarily use a different threshold or bias.
5. When the 15-minute round **resolves**, winning shares pay $1 each; the bot’s PnL is tracked (e.g. in logs and optional files).

---

## What You Should Take Away

- The **main** strategy is **hedging**: buy both Yes and No when their combined price is below $1, then collect $1 at resolution.
- **Latency/signal** is an optional layer that uses external price/liquidation data to sometimes trade a bit more aggressively.
- The bot is designed to **reduce naked risk** (one side filled, other not) and to only enter when the edge is large enough (thresholds and safety limits).

For numbers and safety limits, see [Configuration](03-configuration.md) and [Risk & Safety](04-risk-and-safety.md).

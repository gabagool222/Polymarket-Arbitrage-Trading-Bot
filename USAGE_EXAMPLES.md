# Usage Examples for PolyRustArb Bot

This document provides code snippets for valid order placement using the installed `rs-clob-client` wrapper (`client.rs`).

## 1. Batch Hedging (Leg 1 + Leg 2)
This snippet demonstrates how to place two orders simultaneously using the new `place_orders` batch endpoint. This is useful for atomic-like hedging execution.

```rust
use crate::polymarket::types::{OrderRequest, Side, OrderType, TokenType};
use crate::trading::types::Signal;
use rust_decimal::Decimal;
use anyhow::Result;

pub async fn execute_hedge_batch(&self, signal: Signal) -> Result<()> {
    // Ensure we have a valid Token ID (from fix)
    let token_id_1 = signal.token_id.clone()
        .ok_or_else(|| anyhow::anyhow!("Missing token_id"))?;

    // Leg 1: The signal trigger (e.g., Buy YES @ $0.40)
    let leg1 = OrderRequest {
        token_id: token_id_1,
        side: Side::Buy,
        price: signal.suggested_price,
        size: signal.suggested_size,
        order_type: OrderType::GoodTilCancelled,
        expiration: None, // Default 5 mins
    };

    // Leg 2: The Hedge (Buy NO). Requires fetching/storing the opposite Token ID.
    // In this example, we assume we can fetch it or it's passed in context.
    // let token_id_2 = ...; 

    // Example Leg 2 (Hypothetical)
    // let leg2 = OrderRequest {
    //     token_id: token_id_2, 
    //     side: Side::Buy,
    //     price: Decimal::new(60, 2), // $0.60
    //     size: signal.suggested_size,
    //     order_type: OrderType::GoodTilCancelled,
    //     expiration: None,
    // };

    // Execute Batch
    // The client handles EIP-712 signing for both orders internally
    let orders = self.client.place_orders(vec![leg1 /*, leg2 */]).await?;

    for order in orders {
        println!("Order placed: ID {}, Status {:?}", order.id, order.status);
    }

    Ok(())
}
```

## 2. EIP-712 Signing Check
The bot now automatically signs all orders using EIP-712 TypedData (Polymerket CTF Exchange format).
- Domain: `Polymarket CTF Exchange` (Chain ID 137)
- Types: `Order` struct with `salt`, `maker`, `signer`, `taker`, `tokenId`, etc.
- Dependencies: `ethers` crate (v2.0).

## 3. Configuration Tips
- **slippage_bps**: Set in `config.toml` (e.g., `50` = 0.5%) to allow market buy tolerance.
- **min_profit_threshold**: `0.95` means bot buys when `Yes + No < 0.95`.
- **order_expiration**: Default is 5 minutes. Pass `expiration: Some(timestamp)` to override.

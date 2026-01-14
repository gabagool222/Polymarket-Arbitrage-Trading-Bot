#!/usr/bin/env python3
"""
Sign and place order using official py-clob-client SDK.
Called from Rust via subprocess.

Usage: python3 sign_order.py <token_id> <side> <price> <size> <neg_risk>
Returns: JSON with order result or error
"""
import sys
import json
from py_clob_client.client import ClobClient
from py_clob_client.clob_types import ApiCreds, OrderArgs, OrderType, CreateOrderOptions
from py_clob_client.order_builder.constants import BUY, SELL
import os

def main():
    if len(sys.argv) < 5:
        print(json.dumps({"error": "Usage: sign_order.py <token_id> <side> <price> <size> [neg_risk]"}))
        sys.exit(1)

    token_id = sys.argv[1]
    side = sys.argv[2].upper()  # BUY or SELL
    price = float(sys.argv[3])
    size = float(sys.argv[4])
    neg_risk = sys.argv[5].lower() == "true" if len(sys.argv) > 5 else False

    # Load credentials from environment and strip any whitespace/newlines
    private_key = os.environ.get("POLY_PRIVATE_KEY", "").strip()
    funder_address = os.environ.get("POLY_FUNDER_ADDRESS", "").strip()
    api_key = os.environ.get("POLY_API_KEY", "").strip()
    api_secret = os.environ.get("POLY_API_SECRET", "").strip()
    passphrase = os.environ.get("POLY_PASSPHRASE", "").strip()

    if not all([private_key, api_key, api_secret, passphrase]):
        print(json.dumps({"success": False, "error": f"Missing env: PK={'set' if private_key else 'MISSING'}, API_KEY={'set' if api_key else 'MISSING'}"}))
        sys.exit(1)

    # SAFETY: Convert Token ID from Hex to Decimal if needed (CLOB ONLY accepts Decimal Strings)
    if token_id.startswith("0x"):
        try:
            old_id = token_id
            token_id = str(int(token_id, 16))
            print(f"DEBUG: Converted hex TokenID {old_id} to decimal {token_id}", file=sys.stderr)
        except Exception as e:
            print(json.dumps({"success": False, "error": f"Failed to convert hex token_id: {e}"}))
            sys.exit(1)

    try:
        # Setup client
        creds = ApiCreds(api_key=api_key, api_secret=api_secret, api_passphrase=passphrase)
        
        # Determine signature type from env, fallback to logic
        env_sig_type = os.environ.get("POLY_SIGNATURE_TYPE")
        if env_sig_type:
            sig_type = int(env_sig_type)
        else:
            sig_type = 1 if funder_address else 0

        # Create client FIRST
        client = ClobClient(
            host="https://clob.polymarket.com",
            chain_id=137,
            key=private_key,
            creds=creds,
            signature_type=sig_type,
            funder=funder_address if funder_address else None
        )

        # THEN print debug info
        print(f"DEBUG: Using Signer={client.get_address()}, Funder={funder_address}, SigType={sig_type}", file=sys.stderr)

        # 1. Verify API Connectivity
        try:
            client.get_api_keys()
        except Exception as auth_err:
            print(json.dumps({"success": False, "error": f"L2 Auth Failed (Check keys/PK): {auth_err}"}))
            sys.exit(1)

        # 2. Get market metadata (Pinpoint if this specific call fails)
        try:
            tick_size = client.get_tick_size(token_id)
            neg_risk = client.get_neg_risk(token_id)
        except Exception as meta_err:
            print(json.dumps({"success": False, "error": f"Failed to fetch market metadata: {meta_err}"}))
            sys.exit(1)
        
        # 3. Align price to tick size
        tick_f = float(tick_size)
        price_aligned = round(round(price / tick_f) * tick_f, 8)
        
        if price != price_aligned:
             print(f"DEBUG: Aligned price {price} -> {price_aligned} (tick: {tick_size})", file=sys.stderr)

        # 4. Get fee rate
        try:
            fee_rate = client.get_fee_rate_bps(token_id)
        except:
            fee_rate = 0

        # 5. Create order
        try:
            order_args = OrderArgs(
                price=price_aligned,
                size=size,
                side=BUY if side == "BUY" else SELL,
                token_id=token_id,
                fee_rate_bps=fee_rate
            )

            options = CreateOrderOptions(tick_size=tick_size, neg_risk=neg_risk)
            signed_order = client.create_order(order_args, options)
        except Exception as build_err:
            print(json.dumps({"success": False, "error": f"Failed to build/sign order locally: {build_err}"}))
            sys.exit(1)

        # 6. Place order (The actual network request that often fails with 'Request exception!')
        try:
            # Note: client.post_order internally uses httpx
            result = client.post_order(signed_order, OrderType.GTC)
            
            print(json.dumps({
                "success": True,
                "order_id": result.get("orderID", ""),
                "status": result.get("status", ""),
                "result": result
            }))
        except Exception as post_err:
            # If it's a 429 or 5xx, it might be here
            print(json.dumps({"success": False, "error": f"Failed to post order to CLOB: {post_err}"}))
            sys.exit(1)

    except Exception as e:
        import traceback
        error_msg = str(e)
        if "invalid signature" in error_msg.lower():
            error_msg = f"Polymarket rejected signature. Body: {error_msg}. Check Signer/Funder/Token/Price."
        
        print(json.dumps({
            "success": False,
            "error": f"Unexpected error in script: {error_msg}",
            "traceback": traceback.format_exc()
        }))
        sys.exit(1)

if __name__ == "__main__":
    main()

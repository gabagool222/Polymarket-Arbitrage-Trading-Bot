#!/usr/bin/env python3
"""
Generate new API credentials using py-clob-client.
This will use the values defined in your .env file.
"""
import os
from dotenv import load_dotenv
from py_clob_client.client import ClobClient

# Load variables from .env
load_dotenv()

PRIVATE_KEY = os.getenv("POLY_PRIVATE_KEY", "").strip()
FUNDER_ADDRESS = os.getenv("POLY_FUNDER_ADDRESS", "").strip()
SIG_TYPE = int(os.getenv("POLY_SIGNATURE_TYPE", "2"))
HOST = "https://clob.polymarket.com"
CHAIN_ID = 137

if not PRIVATE_KEY:
    print("❌ Error: POLY_PRIVATE_KEY not found in .env")
    exit(1)

print("=== Generate Polymarket API Credentials ===")
print(f"Using PK: {PRIVATE_KEY[:10]}...{PRIVATE_KEY[-6:]}")
print(f"Funder: {FUNDER_ADDRESS}")
print(f"SigType: {SIG_TYPE}")
print("-" * 40)

try:
    # Initialize client
    client = ClobClient(
        host=HOST,
        chain_id=137,
        key=PRIVATE_KEY,
        signature_type=SIG_TYPE,
        funder=FUNDER_ADDRESS if FUNDER_ADDRESS else None
    )

    print(f"Signer Address: {client.get_address()}")
    print("Attempting to derive API credentials...")
    
    # This will either fetch existing or create new ones
    creds = client.create_or_derive_api_creds()
    
    print("\n✅ SUCCESS! Copy these to your .env file:")
    print("-" * 40)
    print(f"POLY_API_KEY={creds.api_key}")
    print(f"POLY_API_SECRET={creds.api_secret}")
    print(f"POLY_PASSPHRASE={creds.api_passphrase}")
    print("-" * 40)
    
except Exception as e:
    print(f"\n❌ FAILED: {e}")
    print("\nTroubleshooting Tips:")
    print("1. If using MetaMask, make sure POLY_SIGNATURE_TYPE=2")
    print("2. If using Magic Link, make sure POLY_SIGNATURE_TYPE=1")
    print("3. Ensure the Private Key is exactly 64 chars long (plus 0x)")
    print("4. Ensure Funder Address is the Proxy Wallet in Polymarket Settings")

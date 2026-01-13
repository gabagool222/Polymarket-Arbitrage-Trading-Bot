#!/usr/bin/env python3
"""
Generate new API credentials using py-clob-client
"""
from py_clob_client.client import ClobClient

# Private Key dari Polymarket Reveal (untuk wallet 0xe21Dc24...)
PRIVATE_KEY = "0x3293ab6e732cd9ec0412d660c29e87d316bbeb674109116fdba7c0dad8672e62"

# Polymarket proxy/funder address
FUNDER_ADDRESS = "0x9046BBDFAa366204836e66CF1d850a04a1d89541"

HOST = "https://clob.polymarket.com"
CHAIN_ID = 137

print("=== Generate New Polymarket API Credentials ===")
print()
print(f"Using private key: {PRIVATE_KEY[:10]}...{PRIVATE_KEY[-6:]}")
print(f"Funder address: {FUNDER_ADDRESS}")
print()

# Initialize client with signature_type=1 (POLY_PROXY)
client = ClobClient(
    host=HOST,
    chain_id=CHAIN_ID,
    key=PRIVATE_KEY,
    signature_type=1,  # POLY_PROXY for Magic/Email wallets
    funder=FUNDER_ADDRESS
)

print(f"Signer address: {client.get_address()}")
print()

try:
    print("Attempting to create or derive API credentials...")
    creds = client.create_or_derive_api_creds()
    
    print()
    print("✅ SUCCESS! New credentials generated:")
    print(f"   API Key: {creds.api_key}")
    print(f"   API Secret: {creds.api_secret}")
    print(f"   API Passphrase: {creds.api_passphrase}")
    print()
    print("Copy these to docker-compose.yml:")
    print(f"   POLY_API_KEY={creds.api_key}")
    print(f"   POLY_API_SECRET={creds.api_secret}")
    print(f"   POLY_PASSPHRASE={creds.api_passphrase}")
    
except Exception as e:
    print(f"❌ FAILED: {e}")
    print()
    print("Try signature_type=0 (EOA) instead...")
    
    try:
        client2 = ClobClient(
            host=HOST,
            chain_id=CHAIN_ID,
            key=PRIVATE_KEY,
            signature_type=0  # EOA
        )
        print(f"EOA Address: {client2.get_address()}")
        creds = client2.create_or_derive_api_creds()
        print()
        print("✅ SUCCESS with EOA! Credentials:")
        print(f"   API Key: {creds.api_key}")
        print(f"   API Secret: {creds.api_secret}")
        print(f"   API Passphrase: {creds.api_passphrase}")
    except Exception as e2:
        print(f"❌ EOA also failed: {e2}")

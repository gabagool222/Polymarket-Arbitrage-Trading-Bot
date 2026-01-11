import os
import sys
from py_clob_client.client import ClobClient
from dotenv import load_dotenv

# Load .env file or environment variables
load_dotenv()

def main():
    pk = os.getenv("POLY_PRIVATE_KEY")
    if not pk:
        print("❌ Error: POLY_PRIVATE_KEY environment variable is not set.")
        print("   Run: export POLY_PRIVATE_KEY=0x... or set in .env file")
        sys.exit(1)

    # Clean PK format just in case
    if not pk.startswith("0x"):
        pk = "0x" + pk

    print(f"🔑 Deriving API Key for wallet (Private Key: {pk[:6]}...{pk[-4:]})")
    print("   Connecting to Polymarket CLOB (Polygon Mainnet)...")

    try:
        # Initialize Client (Chain ID 137 = Polygon)
        # Using signature_type=0 (EOA) by default for Metamask keys
        client = ClobClient("https://clob.polymarket.com", key=pk, chain_id=137)
        
        # Derive API Key (L2 Auth)
        creds = client.derive_api_key()
        
        print("\n✅ SUCCESS! API Credentials Derived.")
        print("   Add these to your Coolify environment variables along with POLY_PRIVATE_KEY:")
        print("\n========================================================")
        print(f"POLY_API_KEY={creds.api_key}")
        print(f"POLY_API_SECRET={creds.api_secret}")
        print(f"POLY_PASSPHRASE={creds.passphrase}")
        print("========================================================\n")
        print("⚠️  KEEP THESE SECRET! Do not share them.")

    except Exception as e:
        print(f"\n❌ Error deriving keys: {e}")
        print("   Make sure your Private Key is correct and corresponds to a wallet with Polygon activity.")

if __name__ == "__main__":
    main()

use ethers::types::transaction::eip712::{EIP712Domain, TypedData, Eip712};
use serde_json::json;
use ethers::utils::keccak256;

fn main() {
    let typed_data_json = json!({
        "types": {
            "EIP712Domain": [
                { "name": "name", "type": "string" },
                { "name": "version", "type": "string" },
                { "name": "chainId", "type": "uint256" },
                { "name": "verifyingContract", "type": "address" },
            ],
            "Order": [
                { "name": "salt", "type": "uint256" },
                { "name": "maker", "type": "address" },
                { "name": "signer", "type": "address" },
                { "name": "taker", "type": "address" },
                { "name": "tokenId", "type": "uint256" },
                { "name": "makerAmount", "type": "uint256" },
                { "name": "takerAmount", "type": "uint256" },
                { "name": "expiration", "type": "uint256" },
                { "name": "nonce", "type": "uint256" },
                { "name": "feeRateBps", "type": "uint256" },
                { "name": "side", "type": "uint8" },
                { "name": "signatureType", "type": "uint8" },
            ],
        },
        "domain": {
            "name": "Polymarket CTF Exchange",
            "version": "1",
            "chainId": 137,
            "verifyingContract": "0x4bFb41d5B3570DeFd03C39a9A4D8dE6Bd8B8982E",
        },
        "primaryType": "Order",
        "message": {
            "salt": "12345",
            "maker": "0x9046BBDFAa366204836e66CF1d850a04a1d89541",
            "signer": "0xe21Dc24dD0Fa7c0facF32833aaA8494965049e09",
            "taker": "0x0000000000000000000000000000000000000000",
            "tokenId": "12345",
            "makerAmount": "10000",
            "takerAmount": "20000",
            "expiration": "0",
            "nonce": "0",
            "feeRateBps": "0",
            "side": 0,
            "signatureType": 1
        }
    });

    let typed_data: TypedData = serde_json::from_value(typed_data_json).unwrap();
    let domain_separator = typed_data.domain_separator();
    println!("Domain separator hash: 0x{}", hex::encode(domain_separator));

    let struct_hash = typed_data.struct_hash().unwrap();
    println!("Order struct hash: 0x{}", hex::encode(struct_hash));

    let full_hash = typed_data.encode_eip712().unwrap();
    println!("Full EIP-712 hash: 0x{}", hex::encode(keccak256(full_hash)));
}

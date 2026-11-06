use std::io::Read;

use alloy::signers::{local::PrivateKeySigner, SignerSync};
use serde::Deserialize;
use tangle_inference_core::{billing::spend_auth_hashes, SpendAuthPayload};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Input {
    private_key: String,
    verifying_contract: String,
    chain_id: u64,
    auth: SpendAuthPayload,
}

fn main() -> anyhow::Result<()> {
    let mut input = String::new();
    std::io::stdin().read_to_string(&mut input)?;
    // Never include deserialization input or signer parse errors in diagnostics.
    let input: Input =
        serde_json::from_str(&input).map_err(|_| anyhow::anyhow!("invalid signing input"))?;
    let signer: PrivateKeySigner = input
        .private_key
        .trim()
        .parse()
        .map_err(|_| anyhow::anyhow!("invalid signing key"))?;
    let (digest, domain, structure) =
        spend_auth_hashes(&input.auth, &input.verifying_contract, input.chain_id)
            .map_err(anyhow::Error::msg)?;
    let signature = signer.sign_hash_sync(&digest)?;
    println!(
        "{}",
        serde_json::json!({
            "signature": signature.to_string(), "digest": digest.to_string(),
            "domainSeparator": domain.to_string(), "structHash": structure.to_string()
        })
    );
    Ok(())
}

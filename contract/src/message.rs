//! Message building for clear-signing.
//!
//! All messages follow the format:
//!   `expires <timestamp>: <action> <content> | wallet: <name> proposal: <index>`
//!
//! Signers see exactly what they're approving — no opaque transaction bytes.

use crate::*;

/// Build a human-readable message for signing
pub fn build_message(
    wallet_name: &str,
    proposal_index: u64,
    expires_at: u64,
    action: &str,
    intent: &Intent,
    params: &serde_json::Value,
) -> String {
    let content = match intent.intent_type {
        IntentType::AddIntent => {
            let hash = params.get("hash").and_then(|v| v.as_str()).unwrap_or("unknown");
            format!("add intent definition_hash: {}", hash)
        }
        IntentType::RemoveIntent => {
            let idx = params.get("index").and_then(|v| v.as_u64()).unwrap_or(0);
            format!("remove intent {}", idx)
        }
        IntentType::UpdateIntent => {
            let idx = params.get("index").and_then(|v| v.as_u64()).unwrap_or(0);
            format!("update intent {}", idx)
        }
        IntentType::Custom => intent.render_template(params),
    };

    // Convert nanoseconds to ISO-ish timestamp
    let expires_secs = expires_at / 1_000_000_000;
    let expires_nanos = expires_at % 1_000_000_000;
    let expires_display = format!("{}.{:09}", expires_secs, expires_nanos);

    format!(
        "expires {}: {} {} | wallet: {} proposal: {}",
        expires_display, action, content, wallet_name, proposal_index
    )
}

/// Verify an ed25519 signature over a message
pub fn verify_signature(public_key: &str, signature_hex: &str, message: &str) {
    // Parse public key (strip "ed25519:" prefix if present)
    let pk_str = public_key.strip_prefix("ed25519:").unwrap_or(public_key);
    let pk_bytes = hex_decode(pk_str);
    assert_eq!(pk_bytes.len(), 32, "Invalid public key length");
    let pk: [u8; 32] = pk_bytes.try_into().unwrap();

    // Parse signature
    let sig_bytes = hex_decode(signature_hex);
    assert_eq!(sig_bytes.len(), 64, "Invalid signature length");
    let sig: [u8; 64] = sig_bytes.try_into().unwrap();

    // Verify using NEAR's built-in ed25519 verification
    let valid = env::ed25519_verify(&sig, message.as_bytes(), &pk);
    assert!(valid, "Invalid signature: the message was not signed by this key");
}

fn hex_decode(s: &str) -> Vec<u8> {
    (0..s.len())
        .step_by(2)
        .map(|i| {
            u8::from_str_radix(&s[i..i + 2], 16)
                .unwrap_or_else(|_| panic!("Invalid hex at position {}", i))
        })
        .collect()
}

pub fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{:02x}", b)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_test_intent() -> Intent {
        Intent {
            wallet_name: "treasury".to_string(),
            index: 3,
            intent_type: IntentType::Custom,
            name: "Transfer NEAR".to_string(),
            template: "transfer {amount} NEAR to {recipient}".to_string(),
            proposers: vec![],
            approvers: vec![],
            approval_threshold: 2,
            cancellation_threshold: 2,
            timelock_seconds: 0,
            params: vec![
                ParamDef {
                    name: "amount".to_string(),
                    param_type: ParamType::U128,
                    max_value: Some(U128(10_000_000_000_000_000_000_000_000)),
                },
                ParamDef {
                    name: "recipient".to_string(),
                    param_type: ParamType::AccountId,
                    max_value: None,
                },
            ],
            execution_gas_tgas: 50,
            active: true,
            active_proposal_count: 0,
        }
    }

    #[test]
    fn test_build_custom_message() {
        let intent = make_test_intent();
        let params = serde_json::json!({
            "amount": "1000000000000000000000000",
            "recipient": "bob.near"
        });

        let msg = build_message("treasury", 42, 1893456000_000_000_000, "propose", &intent, &params);

        assert!(msg.contains("transfer 1000000000000000000000000 NEAR to bob.near"));
        assert!(msg.contains("wallet: treasury proposal: 42"));
        assert!(msg.contains("expires"));
        assert!(msg.contains("propose"));
    }

    #[test]
    fn test_build_message_all_actions() {
        let intent = make_test_intent();
        let params = serde_json::json!({
            "amount": "1000000000000000000000000",
            "recipient": "bob.near"
        });

        for action in &["propose", "approve", "cancel", "amend"] {
            let msg = build_message("w", 0, 1000000000_000_000_000, action, &intent, &params);
            assert!(msg.starts_with("expires"));
            assert!(msg.contains(action));
        }
    }

    #[test]
    fn test_hex_encode_decode() {
        let original = vec![0x00, 0x01, 0xfe, 0xff];
        let encoded = hex_encode(&original);
        assert_eq!(encoded, "0001feff");
        let decoded = hex_decode(&encoded);
        assert_eq!(decoded, original);
    }
}

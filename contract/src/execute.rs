//! Execute approved proposals.

use crate::*;

#[near_bindgen]
impl Contract {
    pub fn execute(&mut self, wallet_name: String, proposal_id: u64) {
        let mut wallet = self.wallets.get(&wallet_name).expect("Wallet not found");
        let pkey = proposal_key(&wallet_name, proposal_id);
        let mut proposal = self.proposals.get(&pkey).expect("Proposal not found");
        assert!(proposal.status == ProposalStatus::Approved, "Must be approved");
        assert!(proposal.expires_at > env::block_timestamp(), "Proposal expired");

        let ikey = intent_key(&wallet_name, proposal.intent_index);
        let intent = self.intents.get(&ikey).expect("Intent not found");
        let timelock_ns = intent.timelock_seconds as u128 * 1_000_000_000;
        assert!(
            env::block_timestamp() as u128 >= proposal.approved_at as u128 + timelock_ns,
            "Timelock not elapsed"
        );

        let params: serde_json::Value = serde_json::from_str(&proposal.param_values).expect("Invalid JSON");

        match intent.intent_type {
            IntentType::AddIntent => self.execute_add_intent(&mut wallet, &wallet_name, &params),
            IntentType::RemoveIntent => self.execute_remove_intent(&wallet_name, &params),
            IntentType::UpdateIntent => self.execute_update_intent(&wallet_name, &params),
            IntentType::Custom => self.execute_custom(&wallet, &intent, &params),
        }

        proposal.status = ProposalStatus::Executed;
        self.proposals.insert(&pkey, &proposal);

        let mut intent_mut = intent.clone();
        intent_mut.active_proposal_count = intent_mut.active_proposal_count.saturating_sub(1);
        self.intents.insert(&ikey, &intent_mut);
        self.wallets.insert(&wallet_name, &wallet);

        // Emit execution event
        env::log_str(&format!("EVENT_JSON:{}", serde_json::json!({
            "standard": "clear-msig",
            "version": "1.0.0",
            "event": "proposal_executed",
            "data": {
                "wallet": wallet_name,
                "proposal_id": proposal_id,
                "intent_index": proposal.intent_index,
                "proposer": proposal.proposer,
            }
        })));

        log!("Proposal #{} executed", proposal_id);
    }

    pub fn cleanup(&mut self, wallet_name: String, proposal_id: u64) {
        let pkey = proposal_key(&wallet_name, proposal_id);
        let proposal = self.proposals.get(&pkey).expect("Proposal not found");
        assert!(
            proposal.status == ProposalStatus::Executed || proposal.status == ProposalStatus::Cancelled,
            "Must be executed or cancelled"
        );
        self.proposals.remove(&pkey);
        log!("Proposal #{} cleaned up", proposal_id);
    }
}

impl Contract {
    fn emit_event(wallet_name: &str, action: &str, data: serde_json::Value) {
        let mut event_data = data;
        event_data["wallet"] = serde_json::Value::String(wallet_name.to_string());
        env::log_str(&format!("EVENT_JSON:{}", serde_json::json!({
            "standard": "clear-msig",
            "version": "1.0.0",
            "event": action,
            "data": event_data
        })));
    }

    fn execute_add_intent(&mut self, wallet: &mut Wallet, wallet_name: &str, params: &serde_json::Value) {
        let index = wallet.intent_index;
        let name = params.get("name").and_then(|v| v.as_str()).unwrap_or("unnamed").to_string();
        let template = params.get("template").and_then(|v| v.as_str()).unwrap_or("").to_string();
        let proposers: Vec<AccountId> = params.get("proposers").and_then(|v| v.as_array())
            .map(|a| a.iter().filter_map(|v| v.as_str().and_then(|s| s.parse().ok())).collect()).unwrap_or_default();
        let approvers: Vec<AccountId> = params.get("approvers").and_then(|v| v.as_array())
            .map(|a| a.iter().filter_map(|v| v.as_str().and_then(|s| s.parse().ok())).collect()).unwrap_or_default();
        let threshold = params.get("approval_threshold").and_then(|v| v.as_u64()).unwrap_or(1) as u16;
        let timelock = params.get("timelock_seconds").and_then(|v| v.as_u64()).unwrap_or(0);

        // Validate limits
        assert!(approvers.len() <= 64, "Max 64 approvers (bitmap limit)");
        assert!(threshold as usize <= approvers.len(), "Threshold exceeds approvers");

        let intent = Intent {
            wallet_name: wallet_name.to_string(),
            index,
            intent_type: IntentType::Custom,
            name,
            template,
            proposers,
            approvers,
            approval_threshold: threshold,
            cancellation_threshold: threshold,
            timelock_seconds: timelock,
            params: vec![],
            active: true,
            active_proposal_count: 0,
        };

        self.intents.insert(&intent_key(wallet_name, index), &intent);
        wallet.intent_index = index + 1;

        Self::emit_event(wallet_name, "intent_added", serde_json::json!({
            "index": index,
            "name": intent.name,
        }));
    }

    fn execute_remove_intent(&mut self, wallet_name: &str, params: &serde_json::Value) {
        let idx = params.get("index").and_then(|v| v.as_u64()).expect("Missing index") as u32;
        let key = intent_key(wallet_name, idx);
        if let Some(mut intent) = self.intents.get(&key) {
            intent.active = false;
            self.intents.insert(&key, &intent);
        }

        Self::emit_event(wallet_name, "intent_removed", serde_json::json!({
            "index": idx,
        }));
    }

    fn execute_update_intent(&mut self, wallet_name: &str, params: &serde_json::Value) {
        let idx = params.get("index").and_then(|v| v.as_u64()).expect("Missing index") as u32;
        let key = intent_key(wallet_name, idx);
        let mut intent = self.intents.get(&key).expect("Intent not found");
        assert!(intent.active_proposal_count == 0, "Has active proposals");

        if let Some(n) = params.get("name").and_then(|v| v.as_str()) { intent.name = n.to_string(); }
        if let Some(t) = params.get("template").and_then(|v| v.as_str()) { intent.template = t.to_string(); }
        if let Some(th) = params.get("approval_threshold").and_then(|v| v.as_u64()) { intent.approval_threshold = th as u16; }
        if let Some(tl) = params.get("timelock_seconds").and_then(|v| v.as_u64()) { intent.timelock_seconds = tl; }

        self.intents.insert(&key, &intent);

        Self::emit_event(wallet_name, "intent_updated", serde_json::json!({
            "index": idx,
        }));
    }

    fn execute_custom(&mut self, wallet: &Wallet, intent: &Intent, params: &serde_json::Value) {
        match intent.name.as_str() {
            "Transfer NEAR" | "transfer_near" => {
                let recipient: AccountId = params.get("recipient").and_then(|v| v.as_str()).expect("Missing recipient")
                    .parse().expect("Invalid recipient");
                let amount: u128 = params.get("amount")
                    .and_then(|v| v.as_str().and_then(|s| s.parse().ok()))
                    .or_else(|| params.get("amount").and_then(|v| v.as_u64()).map(|v| v as u128))
                    .expect("Missing amount");
                Promise::new(recipient.clone()).transfer(NearToken::from_yoctonear(amount as u128));

                Self::emit_event(&wallet.name, "transfer_near", serde_json::json!({
                    "recipient": recipient,
                    "amount": amount.to_string(),
                }));

                log!("Transferred {} yoctoNEAR", amount);
            }
            "Transfer FT" | "transfer_ft" => {
                let token: AccountId = params.get("token").and_then(|v| v.as_str()).expect("Missing token")
                    .parse().expect("Invalid token");
                let recipient: AccountId = params.get("recipient").and_then(|v| v.as_str()).expect("Missing recipient")
                    .parse().expect("Invalid recipient");
                let amount = params.get("amount").and_then(|v| v.as_str()).expect("Missing amount");
                // Validate amount is a valid U128
                let _amount_validated: u128 = amount.parse().expect("Invalid FT amount");

                let msg = format!(r#"{{"receiver_id":"{}","amount":"{}","msg":""}}"#, recipient, amount);
                Promise::new(token.clone()).function_call(
                    "ft_transfer".to_string(),
                    msg.into_bytes(),
                    NearToken::from_yoctonear(1),
                    near_sdk::Gas::from_tgas(50),
                );

                Self::emit_event(&wallet.name, "transfer_ft", serde_json::json!({
                    "token": token,
                    "recipient": recipient,
                    "amount": amount,
                }));
            }
            _ => {
                Self::emit_event(&wallet.name, "custom_execution", serde_json::json!({
                    "intent": intent.name,
                    "params": params,
                }));
                log!("Custom execution: {}", intent.name);
            }
        }
    }
}

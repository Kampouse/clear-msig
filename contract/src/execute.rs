//! Execute approved proposals.

use crate::*;

#[near_bindgen]
impl Contract {
    /// Execute an approved proposal after timelock has elapsed.
    pub fn execute(&mut self, wallet_name: String, proposal_id: u64) {
        let mut wallet = self.wallets.get(&wallet_name).expect("Wallet not found");
        let pkey = proposal_key(&wallet_name, proposal_id);
        let mut proposal = self.proposals.get(&pkey).expect("Proposal not found");

        assert!(proposal.status == ProposalStatus::Approved, "Must be approved");
        assert!(proposal.expires_at > env::block_timestamp(), "Proposal expired");

        let ikey = intent_key(&wallet_name, proposal.intent_index);
        let intent = self.intents.get(&ikey).expect("Intent not found");

        // Timelock check
        let timelock_ns = intent.timelock_seconds as u128 * 1_000_000_000;
        assert!(
            env::block_timestamp() as u128 >= proposal.approved_at as u128 + timelock_ns,
            "Timelock not elapsed ({}s remaining)",
            ((proposal.approved_at as u128 + timelock_ns - env::block_timestamp() as u128)
                / 1_000_000_000)
        );

        let params: serde_json::Value =
            serde_json::from_str(&proposal.param_values).expect("Invalid JSON");

        // Re-validate params against current intent schema
        self.validate_params(&intent, &params);

        // Verify intent params haven't changed since proposal was created
        let current_hash = hash_params(&intent.params);
        assert_eq!(
            proposal.intent_params_hash, current_hash,
            "Intent params changed after proposal — proposal invalidated"
        );

        // Dispatch execution
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

        self.emit("proposal_executed", serde_json::json!({
            "wallet": wallet_name,
            "proposal_id": proposal_id,
            "intent_index": proposal.intent_index,
            "proposer": proposal.proposer.to_string(),
        }));

        log!("Proposal #{} executed", proposal_id);
    }

    /// Clean up an executed or cancelled proposal from storage.
    pub fn cleanup(&mut self, wallet_name: String, proposal_id: u64) {
        let pkey = proposal_key(&wallet_name, proposal_id);
        let proposal = self.proposals.get(&pkey).expect("Proposal not found");
        assert!(
            proposal.status == ProposalStatus::Executed
                || proposal.status == ProposalStatus::Cancelled,
            "Must be executed or cancelled"
        );
        self.proposals.remove(&pkey);

        self.emit("proposal_cleaned", serde_json::json!({
            "wallet": wallet_name,
            "proposal_id": proposal_id,
        }));

        log!("Proposal #{} cleaned up", proposal_id);
    }
}

// ── Execution Dispatchers ──────────────────────────────────────────────────

impl Contract {
    fn execute_add_intent(
        &mut self,
        wallet: &mut Wallet,
        wallet_name: &str,
        params: &serde_json::Value,
    ) {
        let index = wallet.intent_index;

        let name = params
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or("unnamed")
            .to_string();
        let template = params
            .get("template")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let proposers: Vec<AccountId> = params
            .get("proposers")
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().and_then(|s| s.parse().ok()))
                    .collect()
            })
            .unwrap_or_default();
        let approvers: Vec<AccountId> = params
            .get("approvers")
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().and_then(|s| s.parse().ok()))
                    .collect()
            })
            .unwrap_or_default();
        let threshold = params
            .get("approval_threshold")
            .and_then(|v| v.as_u64())
            .unwrap_or(1) as u16;
        let timelock = params
            .get("timelock_seconds")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        let gas_tgas = params
            .get("execution_gas_tgas")
            .and_then(|v| v.as_u64())
            .unwrap_or(DEFAULT_EXECUTION_GAS_TGAS);

        // Validate limits
        assert!(
            approvers.len() <= MAX_APPROVERS,
            "Max {} approvers",
            MAX_APPROVERS
        );
        assert!(
            threshold as usize <= approvers.len(),
            "Threshold exceeds approvers"
        );

        // Parse param definitions if provided
        let param_defs: Vec<ParamDef> = params
            .get("params")
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|v| {
                        let name = v.get("name")?.as_str()?.to_string();
                        let pt_str = v.get("param_type")?.as_str()?;
                        let param_type = match pt_str {
                            "AccountId" => ParamType::AccountId,
                            "U64" => ParamType::U64,
                            "U128" => ParamType::U128,
                            "String" => ParamType::String,
                            "Bool" => ParamType::Bool,
                            _ => return None,
                        };
                        let max_value = v
                            .get("max_value")
                            .and_then(|mv| mv.as_str())
                            .and_then(|s| s.parse::<u128>().ok())
                            .map(U128);
                        Some(ParamDef {
                            name,
                            param_type,
                            max_value,
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();

        assert!(!param_defs.is_empty(), "Dynamic intents must have param definitions");

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
            params: param_defs,
            execution_gas_tgas: gas_tgas.min(MAX_EXECUTION_GAS_TGAS),
            active: true,
            active_proposal_count: 0,
        };

        self.intents.insert(&intent_key(wallet_name, index), &intent);
        wallet.intent_index = index + 1;

        self.emit("intent_added_via_proposal", serde_json::json!({
            "wallet": wallet_name,
            "index": index,
        }));
    }

    fn execute_remove_intent(&mut self, wallet_name: &str, params: &serde_json::Value) {
        let idx = params
            .get("index")
            .and_then(|v| v.as_u64())
            .expect("Missing index") as u32;

        // Don't allow removing meta-intents (0-2)
        assert!(idx >= 3, "Cannot remove meta-intents");

        let key = intent_key(wallet_name, idx);
        if let Some(mut intent) = self.intents.get(&key) {
            assert!(
                intent.active_proposal_count == 0,
                "Cannot remove intent with active proposals"
            );
            intent.active = false;
            self.intents.insert(&key, &intent);
        }

        self.emit("intent_removed", serde_json::json!({
            "wallet": wallet_name,
            "index": idx,
        }));
    }

    fn execute_update_intent(&mut self, wallet_name: &str, params: &serde_json::Value) {
        let idx = params
            .get("index")
            .and_then(|v| v.as_u64())
            .expect("Missing index") as u32;

        // Don't allow updating meta-intents (0-2)
        assert!(idx >= 3, "Cannot update meta-intents");

        let key = intent_key(wallet_name, idx);
        let mut intent = self.intents.get(&key).expect("Intent not found");
        assert!(
            intent.active_proposal_count == 0,
            "Cannot update intent with active proposals"
        );

        if let Some(n) = params.get("name").and_then(|v| v.as_str()) {
            intent.name = n.to_string();
        }
        if let Some(t) = params.get("template").and_then(|v| v.as_str()) {
            intent.template = t.to_string();
        }
        if let Some(th) = params
            .get("approval_threshold")
            .and_then(|v| v.as_u64())
        {
            intent.approval_threshold = th as u16;
        }
        if let Some(tl) = params
            .get("timelock_seconds")
            .and_then(|v| v.as_u64())
        {
            intent.timelock_seconds = tl;
        }
        if let Some(g) = params
            .get("execution_gas_tgas")
            .and_then(|v| v.as_u64())
        {
            assert!(g <= MAX_EXECUTION_GAS_TGAS, "Gas exceeds max");
            intent.execution_gas_tgas = g;
        }

        self.intents.insert(&key, &intent);

        self.emit("intent_updated", serde_json::json!({
            "wallet": wallet_name,
            "index": idx,
        }));
    }

    fn execute_custom(&mut self, wallet: &Wallet, intent: &Intent, params: &serde_json::Value) {
        let gas = intent.execution_gas();

        match intent.name.as_str() {
            "Transfer NEAR" | "transfer_near" => {
                let recipient: AccountId = params
                    .get("recipient")
                    .and_then(|v| v.as_str())
                    .expect("Missing recipient")
                    .parse()
                    .expect("Invalid recipient");
                let amount: u128 = params
                    .get("amount")
                    .and_then(|v| v.as_str().and_then(|s| s.parse().ok()))
                    .or_else(|| {
                        params
                            .get("amount")
                            .and_then(|v| v.as_u64())
                            .map(|v| v as u128)
                    })
                    .expect("Missing amount");

                Promise::new(recipient.clone())
                    .transfer(NearToken::from_yoctonear(amount));

                self.emit("transfer_near", serde_json::json!({
                    "wallet": wallet.name,
                    "recipient": recipient.to_string(),
                    "amount": amount.to_string(),
                }));
            }

            "Transfer FT" | "transfer_ft" => {
                let token: AccountId = params
                    .get("token")
                    .and_then(|v| v.as_str())
                    .expect("Missing token")
                    .parse()
                    .expect("Invalid token");
                let recipient: AccountId = params
                    .get("recipient")
                    .and_then(|v| v.as_str())
                    .expect("Missing recipient")
                    .parse()
                    .expect("Invalid recipient");
                let amount = params
                    .get("amount")
                    .and_then(|v| v.as_str())
                    .expect("Missing amount");
                // Validate amount is a valid U128
                let _validated: u128 = amount.parse().expect("Invalid FT amount");

                let msg = format!(
                    r#"{{"receiver_id":"{}","amount":"{}","msg":""}}"#,
                    recipient, amount
                );
                Promise::new(token.clone()).function_call(
                    "ft_transfer".to_string(),
                    msg.into_bytes(),
                    NearToken::from_yoctonear(1),
                    gas,
                );

                self.emit("transfer_ft", serde_json::json!({
                    "wallet": wallet.name,
                    "token": token.to_string(),
                    "recipient": recipient.to_string(),
                    "amount": amount,
                }));
            }

            _ => {
                self.emit("custom_execution", serde_json::json!({
                    "wallet": wallet.name,
                    "intent": intent.name,
                    "params": params,
                }));
            }
        }
    }
}

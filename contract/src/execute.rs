//! Execute approved proposals.

use crate::*;

#[near_bindgen]
impl Contract {
    /// Execute an approved proposal after timelock has elapsed.
    /// Rejects attached deposits — contract holds its own balance.
    pub fn execute(&mut self, wallet_name: String, proposal_id: u64) {
        assert_eq!(
            env::attached_deposit().as_yoctonear(),
            0,
            "ERR_NO_DEPOSIT_ON_EXECUTE"
        );

        let mut wallet = self.wallets.get(&wallet_name).expect("ERR_WALLET_NOT_FOUND");
        let pkey = proposal_key(&wallet_name, proposal_id);
        let mut proposal = self.proposals.get(&pkey).expect("ERR_PROPOSAL_NOT_FOUND");

        assert!(proposal.status == ProposalStatus::Approved, "ERR_NOT_APPROVED");
        assert!(proposal.expires_at > env::block_timestamp(), "ERR_PROPOSAL_EXPIRED");

        let ikey = intent_key(&wallet_name, proposal.intent_index);
        let intent = self.intents.get(&ikey).expect("ERR_INTENT_NOT_FOUND");

        // Timelock check
        let timelock_ns = intent.timelock_seconds as u128 * 1_000_000_000;
        assert!(
            env::block_timestamp() as u128 >= proposal.approved_at as u128 + timelock_ns,
            "ERR_TIMELOCK: {}s remaining",
            (proposal.approved_at as u128 + timelock_ns - env::block_timestamp() as u128)
                / 1_000_000_000
        );

        let params: serde_json::Value =
            serde_json::from_str(&proposal.param_values).expect("ERR_INVALID_JSON");

        // Re-validate params against current intent schema
        self.validate_params(&intent, &params);

        // Verify intent params haven't changed since proposal was created
        let current_hash = hash_params(&intent.params);
        assert_eq!(
            proposal.intent_params_hash, current_hash,
            "ERR_PARAMS_CHANGED"
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
        let proposal = self.proposals.get(&pkey).expect("ERR_PROPOSAL_NOT_FOUND");
        assert!(
            proposal.status == ProposalStatus::Executed
                || proposal.status == ProposalStatus::Cancelled,
            "ERR_NOT_EXECUTABLE"
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

        let name = params.get("name").and_then(|v| v.as_str()).unwrap_or("unnamed").to_string();
        let template = params.get("template").and_then(|v| v.as_str()).unwrap_or("").to_string();
        let proposers: Vec<AccountId> = params
            .get("proposers").and_then(|v| v.as_array())
            .map(|a| a.iter().filter_map(|v| v.as_str().and_then(|s| s.parse().ok())).collect())
            .unwrap_or_default();
        let approvers: Vec<AccountId> = params
            .get("approvers").and_then(|v| v.as_array())
            .map(|a| a.iter().filter_map(|v| v.as_str().and_then(|s| s.parse().ok())).collect())
            .unwrap_or_default();
        let threshold = params.get("approval_threshold").and_then(|v| v.as_u64()).unwrap_or(1) as u16;
        let timelock = params.get("timelock_seconds").and_then(|v| v.as_u64()).unwrap_or(0);
        let gas_tgas = params.get("execution_gas_tgas").and_then(|v| v.as_u64()).unwrap_or(DEFAULT_EXECUTION_GAS_TGAS);

        assert!(approvers.len() <= MAX_APPROVERS, "ERR_MAX_APPROVERS");
        assert!(threshold as usize <= approvers.len(), "ERR_THRESHOLD_EXCEEDS");

        // Parse param definitions
        let param_defs: Vec<ParamDef> = params
            .get("params").and_then(|v| v.as_array())
            .map(|a| {
                a.iter().filter_map(|v| {
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
                    let max_value = v.get("max_value")
                        .and_then(|mv| mv.as_str())
                        .and_then(|s| s.parse::<u128>().ok())
                        .map(U128);
                    Some(ParamDef { name, param_type, max_value })
                }).collect()
            })
            .unwrap_or_default();

        assert!(!param_defs.is_empty(), "ERR_DYNAMIC_PARAMS_REQUIRED");

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
            "wallet": wallet_name, "index": index,
        }));
    }

    fn execute_remove_intent(&mut self, wallet_name: &str, params: &serde_json::Value) {
        let idx = params.get("index").and_then(|v| v.as_u64()).expect("ERR_MISSING_INDEX") as u32;
        assert!(idx >= 3, "ERR_CANNOT_REMOVE_META");

        let key = intent_key(wallet_name, idx);
        if let Some(mut intent) = self.intents.get(&key) {
            assert!(intent.active_proposal_count == 0, "ERR_HAS_ACTIVE_PROPOSALS");
            intent.active = false;
            self.intents.insert(&key, &intent);
        }

        self.emit("intent_removed", serde_json::json!({
            "wallet": wallet_name, "index": idx,
        }));
    }

    fn execute_update_intent(&mut self, wallet_name: &str, params: &serde_json::Value) {
        let idx = params.get("index").and_then(|v| v.as_u64()).expect("ERR_MISSING_INDEX") as u32;
        assert!(idx >= 3, "ERR_CANNOT_UPDATE_META");

        let key = intent_key(wallet_name, idx);
        let mut intent = self.intents.get(&key).expect("ERR_INTENT_NOT_FOUND");
        assert!(intent.active_proposal_count == 0, "ERR_HAS_ACTIVE_PROPOSALS");

        if let Some(n) = params.get("name").and_then(|v| v.as_str()) { intent.name = n.to_string(); }
        if let Some(t) = params.get("template").and_then(|v| v.as_str()) { intent.template = t.to_string(); }
        if let Some(th) = params.get("approval_threshold").and_then(|v| v.as_u64()) { intent.approval_threshold = th as u16; }
        if let Some(tl) = params.get("timelock_seconds").and_then(|v| v.as_u64()) { intent.timelock_seconds = tl; }
        if let Some(g) = params.get("execution_gas_tgas").and_then(|v| v.as_u64()) {
            assert!(g <= MAX_EXECUTION_GAS_TGAS, "ERR_GAS_TOO_HIGH");
            intent.execution_gas_tgas = g;
        }

        self.intents.insert(&key, &intent);

        self.emit("intent_updated", serde_json::json!({
            "wallet": wallet_name, "index": idx,
        }));
    }

    fn execute_custom(&mut self, wallet: &Wallet, intent: &Intent, params: &serde_json::Value) {
        let gas = intent.execution_gas();

        match intent.name.as_str() {
            "Transfer NEAR" | "transfer_near" => {
                let recipient: AccountId = params
                    .get("recipient").and_then(|v| v.as_str()).expect("ERR_MISSING_RECIPIENT")
                    .parse().expect("ERR_INVALID_RECIPIENT");
                let amount: u128 = params
                    .get("amount")
                    .and_then(|v| v.as_str().and_then(|s| s.parse().ok()))
                    .or_else(|| params.get("amount").and_then(|v| v.as_u64()).map(|v| v as u128))
                    .expect("ERR_MISSING_AMOUNT");

                Promise::new(recipient.clone()).transfer(NearToken::from_yoctonear(amount));

                self.emit("transfer_near", serde_json::json!({
                    "wallet": wallet.name,
                    "recipient": recipient.to_string(),
                    "amount": amount.to_string(),
                }));
            }

            "Transfer FT" | "transfer_ft" => {
                let token: AccountId = params
                    .get("token").and_then(|v| v.as_str()).expect("ERR_MISSING_TOKEN")
                    .parse().expect("ERR_INVALID_TOKEN");
                let recipient: AccountId = params
                    .get("recipient").and_then(|v| v.as_str()).expect("ERR_MISSING_RECIPIENT")
                    .parse().expect("ERR_INVALID_RECIPIENT");
                let amount = params
                    .get("amount").and_then(|v| v.as_str()).expect("ERR_MISSING_AMOUNT");
                // Validate amount parses as U128
                amount.parse::<u128>().expect("ERR_INVALID_FT_AMOUNT");

                // Use serde_json for safe serialization (fixes #8: string format JSON is fragile)
                let payload = safe_json_ft_transfer(recipient.as_ref(), amount);
                Promise::new(token.clone()).function_call(
                    "ft_transfer".to_string(),
                    payload,
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

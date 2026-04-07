use near_sdk::borsh::{BorshDeserialize, BorshSerialize};
use near_sdk::collections::LookupMap;
use near_sdk::json_types::U128;
use near_sdk::{
    env, log, near, near_bindgen, AccountId, BorshStorageKey, NearToken,
    PanicOnDefault, Promise, PromiseOrValue,
};

mod execute;
mod ft;
mod message;

use message::hex_encode;

// ── Constants ──────────────────────────────────────────────────────────────

/// Maximum proposal expiry: 1 year from now (nanoseconds)
const MAX_EXPIRY_NS: u64 = 365 * 24 * 60 * 60 * 1_000_000_000;
/// Maximum active proposals per intent
const MAX_ACTIVE_PROPOSALS: u32 = 100;
/// Maximum approvers per intent (bitmap is u64)
const MAX_APPROVERS: usize = 64;
/// Storage deposit per wallet (covers wallet + 3 meta-intents + headroom)
const STORAGE_DEPOSIT_YOCTO: u128 = 500_000_000_000_000_000_000_000; // 0.5 NEAR
/// Default execution gas for cross-contract calls (Tgas)
const DEFAULT_EXECUTION_GAS_TGAS: u64 = 50;
/// Maximum execution gas (Tgas)
const MAX_EXECUTION_GAS_TGAS: u64 = 300;

// ── Storage Keys ──────────────────────────────────────────────────────────

#[derive(BorshSerialize, BorshStorageKey)]
#[borsh(crate = "near_sdk::borsh")]
enum StorageKey {
    Wallets,
    Intents,
    Proposals,
    Delegations,
}

// ── Types ──────────────────────────────────────────────────────────────────

#[derive(Clone, Debug, PartialEq, Eq, Default)]
#[near(serializers = [borsh, json])]
pub enum IntentType {
    #[default]
    Custom,
    AddIntent,
    RemoveIntent,
    UpdateIntent,
}

#[derive(Clone, Debug, PartialEq, Eq, Default)]
#[near(serializers = [borsh, json])]
pub enum ProposalStatus {
    #[default]
    Active,
    Approved,
    Executed,
    Cancelled,
}

#[derive(Clone, Debug)]
#[near(serializers = [borsh, json])]
pub enum ParamType {
    AccountId,
    U64,
    U128,
    String,
    Bool,
}

#[derive(Clone, Debug)]
#[near(serializers = [borsh, json])]
pub struct ParamDef {
    pub name: String,
    pub param_type: ParamType,
    pub max_value: Option<U128>,
}

#[derive(Clone, Debug)]
#[near(serializers = [borsh, json])]
pub struct Intent {
    pub wallet_name: String,
    pub index: u32,
    pub intent_type: IntentType,
    pub name: String,
    pub template: String,
    pub proposers: Vec<AccountId>,
    pub approvers: Vec<AccountId>,
    pub approval_threshold: u16,
    pub cancellation_threshold: u16,
    pub timelock_seconds: u64,
    pub params: Vec<ParamDef>,
    /// Execution gas in teragas (default: 50)
    pub execution_gas_tgas: u64,
    pub active: bool,
    pub active_proposal_count: u32,
}

impl Intent {
    fn is_proposer(&self, account: &AccountId) -> bool {
        self.proposers.contains(account)
    }

    fn execution_gas(&self) -> near_sdk::Gas {
        let tgas = if self.execution_gas_tgas == 0 {
            DEFAULT_EXECUTION_GAS_TGAS
        } else {
            self.execution_gas_tgas.min(MAX_EXECUTION_GAS_TGAS)
        };
        near_sdk::Gas::from_tgas(tgas)
    }

    fn render_template(&self, param_values: &serde_json::Value) -> String {
        let mut result = self.template.clone();
        for param in &self.params {
            let placeholder = format!("{{{}}}", param.name);
            let value = match param_values.get(&param.name) {
                Some(v) => match param.param_type {
                    ParamType::AccountId => v.as_str().unwrap_or("unknown").to_string(),
                    ParamType::U64 => v.as_u64().map(|n| n.to_string()).unwrap_or_default(),
                    ParamType::U128 => v
                        .as_str()
                        .map(|s| s.to_string())
                        .or_else(|| match v {
                            serde_json::Value::Number(n) => Some(n.to_string()),
                            _ => None,
                        })
                        .unwrap_or_default(),
                    ParamType::String => v.as_str().unwrap_or("").to_string(),
                    ParamType::Bool => v.as_bool().map(|b| b.to_string()).unwrap_or_default(),
                },
                None => continue,
            };
            // Sanitize: reject message format characters to prevent injection
            assert!(
                !value.contains('|') && !value.contains('\n') && !value.contains('\r'),
                "Param '{}' contains illegal characters",
                param.name
            );
            result = result.replace(&placeholder, &value);
        }
        result
    }
}

#[derive(Clone, Debug)]
#[near(serializers = [borsh, json])]
pub struct Proposal {
    pub id: u64,
    pub wallet_name: String,
    pub intent_index: u32,
    pub proposer: AccountId,
    pub status: ProposalStatus,
    pub proposed_at: u64,
    pub approved_at: u64,
    pub expires_at: u64,
    pub approval_bitmap: u64,
    pub cancellation_bitmap: u64,
    pub param_values: String,
    pub message: String,
    /// SHA-256 of the intent's params schema at proposal time.
    /// Execution fails if the schema changed after proposal.
    pub intent_params_hash: String,
}

impl Proposal {
    fn approval_count(&self) -> u32 {
        self.approval_bitmap.count_ones()
    }

    fn cancellation_count(&self) -> u32 {
        self.cancellation_bitmap.count_ones()
    }

    fn has_approved(&self, idx: usize) -> bool {
        (self.approval_bitmap & (1u64 << idx)) != 0
    }

    fn set_approval(&mut self, idx: usize) {
        let mask = 1u64 << idx;
        self.cancellation_bitmap &= !mask;
        self.approval_bitmap |= mask;
    }

    fn set_cancellation(&mut self, idx: usize) {
        let mask = 1u64 << idx;
        self.approval_bitmap &= !mask;
        self.cancellation_bitmap |= mask;
    }

    fn reset_votes(&mut self) {
        self.approval_bitmap = 0;
        self.cancellation_bitmap = 0;
        self.approved_at = 0;
    }
}

#[derive(Clone, Debug)]
#[near(serializers = [borsh, json])]
pub struct Wallet {
    pub name: String,
    pub owner: AccountId,
    pub proposal_index: u64,
    pub intent_index: u32,
    pub created_at: u64,
    /// Amount of NEAR deposited for storage (yoctoNEAR)
    pub storage_deposit: u128,
    /// Actual storage used (bytes), tracked for accurate refunds
    pub storage_used: u64,
    /// Tokens allowed to be received via ft_on_transfer.
    /// Empty = accept all (open), non-empty = allowlist only.
    pub allowed_tokens: Vec<AccountId>,
    /// Number of unique FT tokens tracked (for storage accounting)
    pub ft_token_count: u32,
}

/// Storage cost per FT token entry (key + u128 value ≈ 100 bytes)
const FT_ENTRY_STORAGE_BYTES: u64 = 100;

// ── Composite keys ─────────────────────────────────────────────────────────

fn intent_key(wallet: &str, index: u32) -> String {
    format!("{}:i:{}", wallet, index)
}

fn proposal_key(wallet: &str, id: u64) -> String {
    format!("{}:p:{}", wallet, id)
}

fn delegation_key(wallet: &str, intent_index: u32, approver_index: usize) -> String {
    format!("{}:d:{}:{}", wallet, intent_index, approver_index)
}

// ── Helpers ────────────────────────────────────────────────────────────────

/// SHA-256 of Borsh-serialized params schema.
fn hash_params(params: &[ParamDef]) -> String {
    let mut data = Vec::new();
    near_sdk::borsh::BorshSerialize::serialize(params, &mut data)
        .unwrap_or_else(|_| env::panic_str("Failed to serialize params"));
    let hash = env::sha256(&data);
    hash.iter().map(|b| format!("{:02x}", b)).collect()
}

/// Panic if the call comes from a contract (not a direct user transaction).
fn assert_direct_call() {
    assert_eq!(
        env::signer_account_id(),
        env::predecessor_account_id(),
        "ERR_DIRECT_CALL_REQUIRED"
    );
}

/// Get the hex representation of the signer's ed25519 public key (32 bytes, no prefix).
fn signer_pk_hex() -> String {
    let pk = env::signer_account_pk();
    let bytes = pk.into_bytes();
    // near-sdk PublicKey: 1-byte curve type prefix (0x00 = ed25519) + 32 bytes key
    let raw = if bytes.len() == 33 { &bytes[1..] } else { &bytes[..] };
    hex_encode(raw)
}

/// Build a JSON string safely using serde_json (no string formatting for JSON).
fn safe_json_ft_transfer(recipient: &str, amount: &str) -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({
        "receiver_id": recipient,
        "amount": amount,
        "msg": ""
    }))
    .unwrap_or_else(|_| env::panic_str("ERR_JSON_SERIALIZE"))
}

// ── Contract ───────────────────────────────────────────────────────────────

#[derive(BorshDeserialize, BorshSerialize, PanicOnDefault)]
#[borsh(crate = "near_sdk::borsh")]
#[near_bindgen]
pub struct Contract {
    wallets: LookupMap<String, Wallet>,
    intents: LookupMap<String, Intent>,
    proposals: LookupMap<String, Proposal>,
    delegations: LookupMap<String, AccountId>,
    event_nonce: u64,
}

#[near_bindgen]
impl Contract {
    #[init]
    pub fn new() -> Self {
        Self {
            wallets: LookupMap::new(StorageKey::Wallets),
            intents: LookupMap::new(StorageKey::Intents),
            proposals: LookupMap::new(StorageKey::Proposals),
            delegations: LookupMap::new(StorageKey::Delegations),
            event_nonce: 0,
        }
    }

    // ── Wallet Management ──────────────────────────────────────────────

    #[payable]
    pub fn create_wallet(&mut self, name: String) {
        let deposit = env::attached_deposit();
        assert!(
            deposit.as_yoctonear() >= STORAGE_DEPOSIT_YOCTO,
            "ERR_STORAGE_DEPOSIT: need {} yoctoNEAR, got {}",
            STORAGE_DEPOSIT_YOCTO,
            deposit.as_yoctonear()
        );

        let predecessor = env::predecessor_account_id();
        assert!(self.wallets.get(&name).is_none(), "ERR_WALLET_EXISTS");
        assert!(!name.is_empty(), "ERR_NAME_EMPTY");
        assert!(name.len() <= 64, "ERR_NAME_TOO_LONG");
        assert!(
            name.chars().all(|c| c.is_alphanumeric() || c == '-' || c == '_'),
            "ERR_NAME_INVALID_CHARS"
        );

        let initial_storage = env::storage_usage();
        let wallet = Wallet {
            name: name.clone(),
            owner: predecessor.clone(),
            proposal_index: 0,
            intent_index: 3,
            created_at: env::block_timestamp(),
            storage_deposit: deposit.as_yoctonear(),
            storage_used: 0,
            allowed_tokens: Vec::new(),
            ft_token_count: 0,
        };
        self.wallets.insert(&name, &wallet);
        self.create_meta_intents(&name, &predecessor);
        let storage_used = env::storage_usage() - initial_storage;

        // Update with actual storage usage
        let mut w = self.wallets.get(&name).unwrap();
        w.storage_used = storage_used;
        self.wallets.insert(&name, &w);

        self.emit("wallet_created", serde_json::json!({
            "wallet": name,
            "owner": predecessor.to_string(),
            "deposit": deposit.as_yoctonear().to_string(),
            "storage_used": storage_used,
        }));

        log!("Wallet '{}' created ({} bytes storage)", name, storage_used);
    }

    pub fn delete_wallet(&mut self, name: String) {
        assert_direct_call();
        let wallet = self.wallets.get(&name).expect("ERR_WALLET_NOT_FOUND");
        assert_eq!(
            env::predecessor_account_id(),
            wallet.owner,
            "ERR_NOT_OWNER"
        );

        for i in 0..wallet.intent_index {
            if let Some(intent) = self.intents.get(&intent_key(&name, i)) {
                assert!(
                    intent.active_proposal_count == 0,
                    "ERR_ACTIVE_PROPOSALS: intent #{} has {}",
                    i,
                    intent.active_proposal_count
                );
            }
        }

        // Collect delegation keys before removing intents
        let mut del_keys: Vec<String> = Vec::new();
        for i in 0..wallet.intent_index {
            if let Some(intent) = self.intents.get(&intent_key(&name, i)) {
                for j in 0..intent.approvers.len() {
                    del_keys.push(delegation_key(&name, i, j));
                }
            }
        }

        for i in 0..wallet.intent_index {
            self.intents.remove(&intent_key(&name, i));
        }
        for i in 0..wallet.proposal_index {
            self.proposals.remove(&proposal_key(&name, i));
        }
        for dkey in del_keys {
            self.delegations.remove(&dkey);
        }

        // Refund only the actual storage cost (20 yoctoNEAR per byte) + deposit remainder
        let storage_cost = NearToken::from_yoctonear(wallet.storage_used as u128)
    .saturating_mul(env::storage_byte_cost().as_yoctonear())
    .as_yoctonear();
        let refund = wallet.storage_deposit.saturating_sub(storage_cost);
        if refund > 0 {
            Promise::new(wallet.owner.clone()).transfer(NearToken::from_yoctonear(refund));
        }

        self.wallets.remove(&name);

        self.emit("wallet_deleted", serde_json::json!({
            "wallet": name,
            "storage_used": wallet.storage_used,
            "refund": refund.to_string(),
        }));

        log!("Wallet '{}' deleted (refunded {} yocto)", name, refund);
    }

    pub fn transfer_ownership(&mut self, wallet_name: String, new_owner: AccountId) {
        assert_direct_call();
        let mut wallet = self.wallets.get(&wallet_name).expect("ERR_WALLET_NOT_FOUND");
        assert_eq!(env::predecessor_account_id(), wallet.owner, "ERR_NOT_OWNER");
        assert_ne!(new_owner, wallet.owner, "ERR_ALREADY_OWNER");

        let old_owner = wallet.owner.clone();
        wallet.owner = new_owner.clone();
        self.wallets.insert(&wallet_name, &wallet);

        for i in 0..3u32 {
            let ikey = intent_key(&wallet_name, i);
            if let Some(mut intent) = self.intents.get(&ikey) {
                if let Some(pos) = intent.proposers.iter().position(|a| a == &old_owner) {
                    intent.proposers[pos] = new_owner.clone();
                }
                if let Some(pos) = intent.approvers.iter().position(|a| a == &old_owner) {
                    intent.approvers[pos] = new_owner.clone();
                }
                self.intents.insert(&ikey, &intent);
            }
        }

        self.emit("ownership_transferred", serde_json::json!({
            "wallet": wallet_name,
            "old_owner": old_owner.to_string(),
            "new_owner": new_owner.to_string(),
        }));

        log!("Ownership of '{}' transferred to {}", wallet_name, new_owner);
    }

    // ── Intent Management ──────────────────────────────────────────────

    /// Add a token to the wallet's FT allowlist. Owner only.
    /// Empty allowlist = accept all tokens.
    /// Once you add the first token, only listed tokens are accepted.
    pub fn add_allowed_token(&mut self, wallet_name: String, token: AccountId) {
        assert_direct_call();
        let mut wallet = self.wallets.get(&wallet_name).expect("ERR_WALLET_NOT_FOUND");
        assert_eq!(env::predecessor_account_id(), wallet.owner, "ERR_NOT_OWNER");
        assert!(
            !wallet.allowed_tokens.contains(&token),
            "ERR_TOKEN_ALREADY_ALLOWED"
        );
        wallet.allowed_tokens.push(token.clone());
        self.wallets.insert(&wallet_name, &wallet);

        self.emit("token_allowed", serde_json::json!({
            "wallet": wallet_name,
            "token": token.to_string(),
        }));

        log!("Token '{}' allowed for wallet '{}'", token, wallet_name);
    }

    /// Remove a token from the wallet's FT allowlist. Owner only.
    pub fn remove_allowed_token(&mut self, wallet_name: String, token: AccountId) {
        assert_direct_call();
        let mut wallet = self.wallets.get(&wallet_name).expect("ERR_WALLET_NOT_FOUND");
        assert_eq!(env::predecessor_account_id(), wallet.owner, "ERR_NOT_OWNER");
        let original_len = wallet.allowed_tokens.len();
        wallet.allowed_tokens.retain(|t| t != &token);
        assert!(
            wallet.allowed_tokens.len() < original_len,
            "ERR_TOKEN_NOT_IN_LIST"
        );
        self.wallets.insert(&wallet_name, &wallet);

        self.emit("token_removed_from_allowlist", serde_json::json!({
            "wallet": wallet_name,
            "token": token.to_string(),
        }));

        log!("Token '{}' removed from allowlist for '{}'", token, wallet_name);
    }

    pub fn delegate_approver(
        &mut self,
        wallet_name: String,
        intent_index: u32,
        approver_index: u16,
        delegate: AccountId,
    ) {
        assert_direct_call();
        let caller = env::predecessor_account_id();

        let intent = self.intents.get(&intent_key(&wallet_name, intent_index)).expect("ERR_INTENT_NOT_FOUND");
        assert!((approver_index as usize) < intent.approvers.len(), "ERR_INVALID_APPROVER_INDEX");
        assert_eq!(intent.approvers[approver_index as usize], caller, "ERR_NOT_YOUR_SLOT");

        let dkey = delegation_key(&wallet_name, intent_index, approver_index as usize);

        if delegate == caller {
            self.delegations.remove(&dkey);
            self.emit("delegation_revoked", serde_json::json!({
                "wallet": wallet_name, "intent_index": intent_index, "approver_index": approver_index,
            }));
        } else {
            self.delegations.insert(&dkey, &delegate);
            self.emit("delegation_set", serde_json::json!({
                "wallet": wallet_name, "intent_index": intent_index,
                "approver_index": approver_index, "delegate": delegate.to_string(),
            }));
        }
    }

    // ── Proposal Lifecycle ─────────────────────────────────────────────

    pub fn propose(
        &mut self,
        wallet_name: String,
        intent_index: u32,
        param_values: String,
        expires_at: u64,
        proposer_pubkey: String,
        signature: String,
    ) {
        assert_direct_call();

        let proposer = env::predecessor_account_id();
        let mut wallet = self.wallets.get(&wallet_name).expect("ERR_WALLET_NOT_FOUND");
        let ikey = intent_key(&wallet_name, intent_index);
        let intent = self.intents.get(&ikey).expect("ERR_INTENT_NOT_FOUND");

        assert!(intent.active, "ERR_INTENT_INACTIVE");
        assert!(intent.is_proposer(&proposer) || proposer == wallet.owner, "ERR_NOT_PROPOSER");
        assert!(expires_at > env::block_timestamp(), "ERR_EXPIRED");
        assert!(expires_at <= env::block_timestamp() + MAX_EXPIRY_NS, "ERR_EXPIRY_TOO_FAR");
        assert!(intent.active_proposal_count < MAX_ACTIVE_PROPOSALS, "ERR_MAX_PROPOSALS");

        let params: serde_json::Value = serde_json::from_str(&param_values).expect("ERR_INVALID_JSON");
        self.validate_params(&intent, &params);

        let proposal_index = wallet.proposal_index;
        let msg = message::build_message(&wallet_name, proposal_index, expires_at, "propose", &intent, &params);

        message::verify_signature(&proposer_pubkey, &signature, &msg);
        assert_eq!(proposer_pubkey, signer_pk_hex(), "ERR_PK_MISMATCH");

        let proposal = Proposal {
            id: proposal_index,
            wallet_name: wallet_name.clone(),
            intent_index,
            proposer: proposer.clone(),
            status: ProposalStatus::Active,
            proposed_at: env::block_timestamp(),
            approved_at: 0,
            expires_at,
            approval_bitmap: 0,
            cancellation_bitmap: 0,
            param_values,
            message: msg.clone(),
            intent_params_hash: hash_params(&intent.params),
        };

        self.proposals.insert(&proposal_key(&wallet_name, proposal_index), &proposal);

        let mut intent_mut = intent.clone();
        intent_mut.active_proposal_count += 1;
        self.intents.insert(&ikey, &intent_mut);

        wallet.proposal_index = proposal_index + 1;
        self.wallets.insert(&wallet_name, &wallet);

        self.emit("proposal_created", serde_json::json!({
            "wallet": wallet_name, "proposal_id": proposal_index,
            "intent_index": intent_index, "proposer": proposer.to_string(), "message": msg,
        }));

        log!("Proposal #{} created for intent #{}", proposal_index, intent_index);
    }

    pub fn amend_proposal(
        &mut self,
        wallet_name: String,
        proposal_id: u64,
        param_values: String,
        expires_at: u64,
        proposer_pubkey: String,
        signature: String,
    ) {
        assert_direct_call();

        let caller = env::predecessor_account_id();
        let pkey = proposal_key(&wallet_name, proposal_id);
        let mut proposal = self.proposals.get(&pkey).expect("ERR_PROPOSAL_NOT_FOUND");

        assert_eq!(proposal.proposer, caller, "ERR_NOT_PROPOSER");
        assert!(proposal.status == ProposalStatus::Active, "ERR_NOT_ACTIVE");
        assert!(expires_at > env::block_timestamp(), "ERR_EXPIRED");
        assert!(expires_at <= env::block_timestamp() + MAX_EXPIRY_NS, "ERR_EXPIRY_TOO_FAR");

        let ikey = intent_key(&wallet_name, proposal.intent_index);
        let intent = self.intents.get(&ikey).expect("ERR_INTENT_NOT_FOUND");
        assert!(intent.active, "ERR_INTENT_INACTIVE");

        let params: serde_json::Value = serde_json::from_str(&param_values).expect("ERR_INVALID_JSON");
        self.validate_params(&intent, &params);

        let msg = message::build_message(&wallet_name, proposal_id, expires_at, "amend", &intent, &params);

        message::verify_signature(&proposer_pubkey, &signature, &msg);
        assert_eq!(proposer_pubkey, signer_pk_hex(), "ERR_PK_MISMATCH");

        proposal.reset_votes();
        proposal.param_values = param_values;
        proposal.expires_at = expires_at;
        proposal.message = msg;
        proposal.intent_params_hash = hash_params(&intent.params);

        self.proposals.insert(&pkey, &proposal);

        self.emit("proposal_amended", serde_json::json!({
            "wallet": wallet_name, "proposal_id": proposal_id, "proposer": caller.to_string(),
        }));

        log!("Proposal #{} amended", proposal_id);
    }

    pub fn approve(
        &mut self,
        wallet_name: String,
        proposal_id: u64,
        approver_index: u16,
        signature: String,
        expires_at: u64,
    ) {
        assert_direct_call();
        self.verify_approver(wallet_name.clone(), proposal_id, approver_index, signature, expires_at, "approve");
    }

    pub fn cancel_vote(
        &mut self,
        wallet_name: String,
        proposal_id: u64,
        approver_index: u16,
        signature: String,
        expires_at: u64,
    ) {
        assert_direct_call();
        self.verify_approver(wallet_name.clone(), proposal_id, approver_index, signature, expires_at, "cancel");
    }

    // ── Views ──────────────────────────────────────────────────────────

    pub fn get_wallet(&self, name: String) -> Option<Wallet> {
        self.wallets.get(&name)
    }

    pub fn get_intent(&self, wallet_name: String, index: u32) -> Option<Intent> {
        self.intents.get(&intent_key(&wallet_name, index))
    }

    pub fn list_intents(&self, wallet_name: String) -> Vec<Intent> {
        let Some(wallet) = self.wallets.get(&wallet_name) else { return Vec::new(); };
        (0..wallet.intent_index)
            .filter_map(|i| self.intents.get(&intent_key(&wallet_name, i)))
            .collect()
    }

    pub fn get_proposal(&self, wallet_name: String, id: u64) -> Option<Proposal> {
        self.proposals.get(&proposal_key(&wallet_name, id))
    }

    pub fn list_proposals(&self, wallet_name: String) -> Vec<Proposal> {
        let Some(wallet) = self.wallets.get(&wallet_name) else { return Vec::new(); };
        (0..wallet.proposal_index)
            .filter_map(|i| self.proposals.get(&proposal_key(&wallet_name, i)))
            .collect()
    }

    pub fn get_proposal_message(&self, wallet_name: String, id: u64) -> Option<String> {
        self.proposals.get(&proposal_key(&wallet_name, id)).map(|p| p.message)
    }

    pub fn get_allowed_tokens(&self, wallet_name: String) -> Vec<AccountId> {
        self.wallets.get(&wallet_name)
            .map(|w| w.allowed_tokens)
            .unwrap_or_default()
    }

    pub fn get_delegation(&self, wallet_name: String, intent_index: u32, approver_index: u16) -> Option<AccountId> {
        self.delegations.get(&delegation_key(&wallet_name, intent_index, approver_index as usize))
    }

    pub fn get_event_nonce(&self) -> u64 {
        self.event_nonce
    }
}

// ── Private Helpers ────────────────────────────────────────────────────────

impl Contract {
    fn emit(&mut self, event: &str, data: serde_json::Value) {
        self.event_nonce += 1;
        env::log_str(&format!(
            "EVENT_JSON:{}",
            serde_json::json!({
                "standard": "clear-msig",
                "version": "1.0.0",
                "event": event,
                "nonce": self.event_nonce,
                "data": data,
            })
        ));
    }

    /// Shared logic for approve and cancel_vote to avoid duplication.
    fn verify_approver(
        &mut self,
        wallet_name: String,
        proposal_id: u64,
        approver_index: u16,
        signature: String,
        expires_at: u64,
        action: &str, // "approve" or "cancel"
    ) {
        let caller = env::predecessor_account_id();
        let pkey = proposal_key(&wallet_name, proposal_id);
        let mut proposal = self.proposals.get(&pkey).expect("ERR_PROPOSAL_NOT_FOUND");

        assert!(proposal.status == ProposalStatus::Active, "ERR_NOT_ACTIVE");
        assert!(proposal.expires_at > env::block_timestamp(), "ERR_PROPOSAL_EXPIRED");
        assert!(expires_at > env::block_timestamp(), "ERR_SIG_EXPIRED");

        let ikey = intent_key(&wallet_name, proposal.intent_index);
        let intent = self.intents.get(&ikey).expect("ERR_INTENT_NOT_FOUND");
        assert!((approver_index as usize) < intent.approvers.len(), "ERR_INVALID_APPROVER_INDEX");

        // Check caller is the approver or their delegate
        let original = &intent.approvers[approver_index as usize];
        let dkey = delegation_key(&wallet_name, proposal.intent_index, approver_index as usize);
        let delegate = self.delegations.get(&dkey);
        assert!(
            caller == *original || delegate.as_ref() == Some(&caller),
            "ERR_NOT_APPROVER_OR_DELEGATE"
        );

        let params: serde_json::Value = serde_json::from_str(&proposal.param_values).unwrap_or_default();
        let msg = message::build_message(&wallet_name, proposal_id, expires_at, action, &intent, &params);

        message::verify_signature(&signer_pk_hex(), &signature, &msg);

        match action {
            "approve" => {
                assert!(!proposal.has_approved(approver_index as usize), "ERR_ALREADY_APPROVED");
                proposal.set_approval(approver_index as usize);

                if proposal.approval_count() >= intent.approval_threshold as u32 {
                    proposal.status = ProposalStatus::Approved;
                    proposal.approved_at = env::block_timestamp();

                    self.emit("proposal_approved", serde_json::json!({
                        "wallet": wallet_name, "proposal_id": proposal_id,
                        "approval_count": proposal.approval_count(),
                    }));

                    log!("Proposal #{} approved", proposal_id);
                }
            }
            "cancel" => {
                proposal.set_cancellation(approver_index as usize);

                if proposal.cancellation_count() >= intent.cancellation_threshold as u32 {
                    proposal.status = ProposalStatus::Cancelled;
                    let mut intent_mut = intent.clone();
                    intent_mut.active_proposal_count = intent_mut.active_proposal_count.saturating_sub(1);
                    self.intents.insert(&ikey, &intent_mut);

                    self.emit("proposal_cancelled", serde_json::json!({
                        "wallet": wallet_name, "proposal_id": proposal_id,
                        "cancellation_count": proposal.cancellation_count(),
                    }));
                }
            }
            _ => env::panic_str("ERR_INVALID_ACTION"),
        }

        self.proposals.insert(&pkey, &proposal);
    }

    fn create_meta_intents(&mut self, name: &str, owner: &AccountId) {
        let make = |index: u32, itype: IntentType, iname: &str, template: &str, params: Vec<ParamDef>| Intent {
            wallet_name: name.to_string(),
            index,
            intent_type: itype,
            name: iname.to_string(),
            template: template.to_string(),
            proposers: vec![owner.clone()],
            approvers: vec![owner.clone()],
            approval_threshold: 1,
            cancellation_threshold: 1,
            timelock_seconds: 0,
            params,
            execution_gas_tgas: DEFAULT_EXECUTION_GAS_TGAS,
            active: true,
            active_proposal_count: 0,
        };

        self.intents.insert(&intent_key(name, 0), &make(
            0, IntentType::AddIntent, "AddIntent", "add intent definition_hash: {hash}",
            vec![ParamDef { name: "hash".to_string(), param_type: ParamType::String, max_value: None }],
        ));
        self.intents.insert(&intent_key(name, 1), &make(
            1, IntentType::RemoveIntent, "RemoveIntent", "remove intent {index}",
            vec![ParamDef { name: "index".to_string(), param_type: ParamType::U64, max_value: None }],
        ));
        self.intents.insert(&intent_key(name, 2), &make(
            2, IntentType::UpdateIntent, "UpdateIntent", "update intent {index}",
            vec![ParamDef { name: "index".to_string(), param_type: ParamType::U64, max_value: None }],
        ));
    }

    fn validate_intent(&self, intent: &Intent) {
        assert!(intent.approvers.len() <= MAX_APPROVERS, "ERR_MAX_APPROVERS");
        assert!(intent.approval_threshold as usize <= intent.approvers.len(), "ERR_THRESHOLD_EXCEEDS");
        assert!(intent.cancellation_threshold as usize <= intent.approvers.len(), "ERR_CANCEL_THRESHOLD");
        assert!(!intent.params.is_empty(), "ERR_EMPTY_PARAMS");
        assert!(intent.execution_gas_tgas <= MAX_EXECUTION_GAS_TGAS, "ERR_GAS_TOO_HIGH");
    }

    fn validate_params(&self, intent: &Intent, params: &serde_json::Value) {
        for pd in &intent.params {
            match params.get(&pd.name) {
                None => env::panic_str(&format!("ERR_MISSING_PARAM: {}", pd.name)),
                Some(val) => match pd.param_type {
                    ParamType::AccountId => {
                        let s = val.as_str().unwrap_or_else(|| env::panic_str(&format!("ERR_EXPECTED_STRING: {}", pd.name)));
                        s.parse::<AccountId>().unwrap_or_else(|_| env::panic_str(&format!("ERR_INVALID_ACCOUNT: {}", pd.name)));
                    }
                    ParamType::U64 => {
                        let v = val.as_u64()
                            .or_else(|| val.as_str().and_then(|s| s.parse::<u64>().ok()))
                            .unwrap_or_else(|| env::panic_str(&format!("ERR_EXPECTED_U64: {}", pd.name)));
                        if let Some(max) = &pd.max_value {
                            assert!((v as u128) <= max.0, "ERR_EXCEEDS_MAX: {}", pd.name);
                        }
                    }
                    ParamType::U128 => {
                        let s = match val {
                            serde_json::Value::String(s) => s.clone(),
                            serde_json::Value::Number(n) => n.to_string(),
                            _ => env::panic_str(&format!("ERR_EXPECTED_U128: {}", pd.name)),
                        };
                        let v: u128 = s.parse().unwrap_or_else(|_| env::panic_str(&format!("ERR_INVALID_U128: {}", pd.name)));
                        if let Some(max) = &pd.max_value {
                            assert!(v <= max.0, "ERR_EXCEEDS_MAX: {}", pd.name);
                        }
                    }
                    ParamType::String => {
                        val.as_str().unwrap_or_else(|| env::panic_str(&format!("ERR_EXPECTED_STRING: {}", pd.name)));
                    }
                    ParamType::Bool => {
                        val.as_bool().unwrap_or_else(|| env::panic_str(&format!("ERR_EXPECTED_BOOL: {}", pd.name)));
                    }
                },
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_render_template() {
        let intent = Intent {
            wallet_name: "test".to_string(),
            index: 3,
            intent_type: IntentType::Custom,
            name: "Transfer NEAR".to_string(),
            template: "transfer {amount} yoctoNEAR to {recipient}".to_string(),
            proposers: vec![],
            approvers: vec![],
            approval_threshold: 1,
            cancellation_threshold: 1,
            timelock_seconds: 0,
            params: vec![
                ParamDef { name: "amount".to_string(), param_type: ParamType::U128, max_value: None },
                ParamDef { name: "recipient".to_string(), param_type: ParamType::AccountId, max_value: None },
            ],
            execution_gas_tgas: 50,
            active: true,
            active_proposal_count: 0,
        };

        let params = serde_json::json!({
            "amount": "1000000000000000000000000",
            "recipient": "bob.near"
        });

        assert_eq!(
            intent.render_template(&params),
            "transfer 1000000000000000000000000 yoctoNEAR to bob.near"
        );
    }

    #[test]
    fn test_proposal_bitmap() {
        let mut p = Proposal {
            id: 0, wallet_name: "w".to_string(), intent_index: 0,
            proposer: "alice.near".parse().unwrap(), status: ProposalStatus::Active,
            proposed_at: 0, approved_at: 0, expires_at: 0,
            approval_bitmap: 0, cancellation_bitmap: 0,
            param_values: "{}".to_string(), message: "".to_string(),
            intent_params_hash: "".to_string(),
        };

        assert_eq!(p.approval_count(), 0);
        assert!(!p.has_approved(0));

        p.set_approval(0);
        assert!(p.has_approved(0));
        assert_eq!(p.approval_count(), 1);

        p.set_cancellation(0);
        assert!(!p.has_approved(0)); // cancelled clears approval
        assert_eq!(p.cancellation_count(), 1);

        p.reset_votes();
        assert_eq!(p.approval_count(), 0);
        assert_eq!(p.cancellation_count(), 0);
    }

    #[test]
    fn test_template_injection_blocked() {
        let intent = Intent {
            wallet_name: "test".to_string(),
            index: 0,
            intent_type: IntentType::Custom,
            name: "test".to_string(),
            template: "do {param}".to_string(),
            proposers: vec![], approvers: vec![],
            approval_threshold: 1, cancellation_threshold: 1,
            timelock_seconds: 0,
            params: vec![ParamDef { name: "param".to_string(), param_type: ParamType::String, max_value: None }],
            execution_gas_tgas: 50,
            active: true,
            active_proposal_count: 0,
        };

        // Pipe should be rejected
        let params = serde_json::json!({ "param": "evil | wallet: fake" });
        let result = std::panic::catch_unwind(|| intent.render_template(&params));
        assert!(result.is_err());
    }

    #[test]
    fn test_safe_json_ft_transfer() {
        let json = safe_json_ft_transfer("bob.near", "1000000");
        let parsed: serde_json::Value = serde_json::from_slice(&json).unwrap();
        assert_eq!(parsed["receiver_id"], "bob.near");
        assert_eq!(parsed["amount"], "1000000");
        assert_eq!(parsed["msg"], "");
    }
}

use near_sdk::borsh::{BorshDeserialize, BorshSerialize};
use near_sdk::collections::LookupMap;
use near_sdk::json_types::U128;
use near_sdk::{
    env, log, near, near_bindgen, AccountId, BorshStorageKey, NearToken,
    PanicOnDefault, Promise, PromiseOrValue,
};

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
    /// Nostr npub hex strings (32-byte x-only public keys)
    pub nostr_approvers: Vec<String>,
    /// Total approvals needed (NEAR + nostr combined)
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
    /// Nostr approver bitmaps (same indexing as nostr_approvers)
    pub nostr_approval_bitmap: u64,
    pub nostr_cancellation_bitmap: u64,
    pub param_values: String,
    pub message: String,
    /// SHA-256 of the intent's params schema at proposal time.
    /// Execution fails if the schema changed after proposal.
    pub intent_params_hash: String,
}

impl Proposal {
    fn approval_count(&self) -> u32 {
        self.approval_bitmap.count_ones() + self.nostr_approval_bitmap.count_ones()
    }

    fn cancellation_count(&self) -> u32 {
        self.cancellation_bitmap.count_ones() + self.nostr_cancellation_bitmap.count_ones()
    }

    fn has_approved(&self, idx: usize) -> bool {
        (self.approval_bitmap & (1u64 << idx)) != 0
    }

    fn has_nostr_approved(&self, idx: usize) -> bool {
        (self.nostr_approval_bitmap & (1u64 << idx)) != 0
    }

    fn set_approval(&mut self, idx: usize) {
        let mask = 1u64 << idx;
        self.cancellation_bitmap &= !mask;
        self.approval_bitmap |= mask;
    }

    fn set_nostr_approval(&mut self, idx: usize) {
        let mask = 1u64 << idx;
        self.nostr_cancellation_bitmap &= !mask;
        self.nostr_approval_bitmap |= mask;
    }

    fn set_cancellation(&mut self, idx: usize) {
        let mask = 1u64 << idx;
        self.approval_bitmap &= !mask;
        self.cancellation_bitmap |= mask;
    }

    fn set_nostr_cancellation(&mut self, idx: usize) {
        let mask = 1u64 << idx;
        self.nostr_approval_bitmap &= !mask;
        self.nostr_cancellation_bitmap |= mask;
    }

    fn reset_votes(&mut self) {
        self.approval_bitmap = 0;
        self.cancellation_bitmap = 0;
        self.nostr_approval_bitmap = 0;
        self.nostr_cancellation_bitmap = 0;
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
    /// Nostr npub hex (32-byte x-only public key) of the contract owner.
    /// All admin actions require a schnorr signature from this key.
    owner_npub: String,
    wallets: LookupMap<String, Wallet>,
    intents: LookupMap<String, Intent>,
    proposals: LookupMap<String, Proposal>,
    delegations: LookupMap<String, AccountId>,
    event_nonce: u64,
}

#[near_bindgen]
impl Contract {
    /// Initialize with the nostr npub of the owner.
    /// The owner controls everything — create wallets, add intents, propose.
    #[init]
    pub fn new(owner_npub: String) -> Self {
        assert!(!owner_npub.is_empty(), "ERR_EMPTY_OWNER_NPUB");
        Self {
            owner_npub,
            wallets: LookupMap::new(StorageKey::Wallets),
            intents: LookupMap::new(StorageKey::Intents),
            proposals: LookupMap::new(StorageKey::Proposals),
            delegations: LookupMap::new(StorageKey::Delegations),
            event_nonce: 0,
        }
    }

    /// Verify the caller is the nostr owner via schnorr signature.
    fn verify_owner(&self, action: &str, signature: &str, expires_at: u64) {
        assert!(expires_at > env::block_timestamp(), "ERR_SIG_EXPIRED");
        let msg = format!("expires {}.000000000: {} | contract: owner", expires_at, action);
        message::verify_schnorr_signature(&self.owner_npub, signature, &msg);
    }

    // ── Wallet Management ──────────────────────────────────────────────

    #[payable]
    pub fn create_wallet(&mut self, name: String, signature: String, expires_at: u64) {
        self.verify_owner(&format!("create_wallet:{}", name), &signature, expires_at);
        let deposit = env::attached_deposit();
        assert!(
            deposit.as_yoctonear() >= STORAGE_DEPOSIT_YOCTO,
            "ERR_STORAGE_DEPOSIT: need {} yoctoNEAR, got {}",
            STORAGE_DEPOSIT_YOCTO,
            deposit.as_yoctonear()
        );

        assert!(self.wallets.get(&name).is_none(), "ERR_WALLET_EXISTS");
        assert!(!name.is_empty(), "ERR_NAME_EMPTY");
        assert!(name.len() <= 64, "ERR_NAME_TOO_LONG");
        assert!(
            name.chars().all(|c| c.is_alphanumeric() || c == '-' || c == '_'),
            "ERR_NAME_INVALID_CHARS"
        );

        let owner_display = self.owner_npub.clone();
        let initial_storage = env::storage_usage();
        let wallet = Wallet {
            name: name.clone(),
            owner: env::predecessor_account_id(),
            proposal_index: 0,
            intent_index: 3,
            created_at: env::block_timestamp(),
            storage_deposit: deposit.as_yoctonear(),
            storage_used: 0,
            allowed_tokens: Vec::new(),
            ft_token_count: 0,
        };
        self.wallets.insert(&name, &wallet);
        self.create_meta_intents(&name, &env::predecessor_account_id());
        let storage_used = env::storage_usage() - initial_storage;

        // Update with actual storage usage
        let mut w = self.wallets.get(&name).unwrap();
        w.storage_used = storage_used;
        self.wallets.insert(&name, &w);

        self.emit("wallet_created", serde_json::json!({
            "wallet": name,
            "owner_npub": owner_display,
            "deposit": deposit.as_yoctonear().to_string(),
            "storage_used": storage_used,
        }));

        log!("Wallet '{}' created ({} bytes storage)", name, storage_used);
    }

    /// Test: verify a nostr schnorr signature. Returns true if valid.
    pub fn test_verify_nostr(
        &self,
        message: String,
        pubkey_hex: String,
        signature: String,
    ) -> bool {
        message::verify_schnorr_signature(&pubkey_hex, &signature, &message);
        true
    }

    pub fn delete_wallet(&mut self, name: String, signature: String, expires_at: u64) {
        self.verify_owner(&format!("delete_wallet:{}", name), &signature, expires_at);
        let wallet = self.wallets.get(&name).expect("ERR_WALLET_NOT_FOUND");

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

    pub fn transfer_ownership(&mut self, wallet_name: String, new_owner: AccountId, signature: String, expires_at: u64) {
        self.verify_owner(&format!("transfer_ownership:{}", wallet_name), &signature, expires_at);
        let mut wallet = self.wallets.get(&wallet_name).expect("ERR_WALLET_NOT_FOUND");
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
    pub fn add_allowed_token(&mut self, wallet_name: String, token: AccountId, signature: String, expires_at: u64) {
        self.verify_owner(&format!("add_allowed_token:{}", wallet_name), &signature, expires_at);
        let mut wallet = self.wallets.get(&wallet_name).expect("ERR_WALLET_NOT_FOUND");
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
    pub fn remove_allowed_token(&mut self, wallet_name: String, token: AccountId, signature: String, expires_at: u64) {
        self.verify_owner(&format!("remove_allowed_token:{}", wallet_name), &signature, expires_at);
        let mut wallet = self.wallets.get(&wallet_name).expect("ERR_WALLET_NOT_FOUND");
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

    // ── Proposal Lifecycle ─────────────────────────────────────────────

    pub fn propose(
        &mut self,
        wallet_name: String,
        intent_index: u32,
        param_values: String,
        expires_at: u64,
        signature: String,
    ) {
        let mut wallet = self.wallets.get(&wallet_name).expect("ERR_WALLET_NOT_FOUND");
        let ikey = intent_key(&wallet_name, intent_index);
        let intent = self.intents.get(&ikey).expect("ERR_INTENT_NOT_FOUND");

        assert!(intent.active, "ERR_INTENT_INACTIVE");
        assert!(expires_at > env::block_timestamp(), "ERR_EXPIRED");
        assert!(expires_at <= env::block_timestamp() + MAX_EXPIRY_NS, "ERR_EXPIRY_TOO_FAR");
        assert!(intent.active_proposal_count < MAX_ACTIVE_PROPOSALS, "ERR_MAX_PROPOSALS");

        let params: serde_json::Value = serde_json::from_str(&param_values).expect("ERR_INVALID_JSON");
        self.validate_params(&intent, &params);

        let proposal_index = wallet.proposal_index;
        let msg = message::build_message(&wallet_name, proposal_index, expires_at, "propose", &intent, &params);

        // Verify nostr owner signature
        self.verify_owner(&format!("propose:{}:{}", wallet_name, proposal_index), &signature, expires_at);

        let proposal = Proposal {
            id: proposal_index,
            wallet_name: wallet_name.clone(),
            intent_index,
            proposer: env::predecessor_account_id(),
            status: ProposalStatus::Active,
            proposed_at: env::block_timestamp(),
            approved_at: 0,
            expires_at,
            approval_bitmap: 0,
            cancellation_bitmap: 0,
            nostr_approval_bitmap: 0,
            nostr_cancellation_bitmap: 0,
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
            "intent_index": intent_index, "message": msg,
        }));

        log!("Proposal #{} created for intent #{}", proposal_index, intent_index);
    }

    pub fn amend_proposal(
        &mut self,
        wallet_name: String,
        proposal_id: u64,
        param_values: String,
        expires_at: u64,
        signature: String,
    ) {
        let pkey = proposal_key(&wallet_name, proposal_id);
        let mut proposal = self.proposals.get(&pkey).expect("ERR_PROPOSAL_NOT_FOUND");

        assert!(proposal.status == ProposalStatus::Active, "ERR_NOT_ACTIVE");
        assert!(expires_at > env::block_timestamp(), "ERR_EXPIRED");
        assert!(expires_at <= env::block_timestamp() + MAX_EXPIRY_NS, "ERR_EXPIRY_TOO_FAR");

        let ikey = intent_key(&wallet_name, proposal.intent_index);
        let intent = self.intents.get(&ikey).expect("ERR_INTENT_NOT_FOUND");
        assert!(intent.active, "ERR_INTENT_INACTIVE");

        let params: serde_json::Value = serde_json::from_str(&param_values).expect("ERR_INVALID_JSON");
        self.validate_params(&intent, &params);

        let msg = message::build_message(&wallet_name, proposal_id, expires_at, "amend", &intent, &params);

        // Verify nostr owner signature
        self.verify_owner(&format!("amend:{}:{}", wallet_name, proposal_id), &signature, expires_at);

        proposal.reset_votes();
        proposal.param_values = param_values;
        proposal.expires_at = expires_at;
        proposal.message = msg;
        proposal.intent_params_hash = hash_params(&intent.params);

        self.proposals.insert(&pkey, &proposal);

        self.emit("proposal_amended", serde_json::json!({
            "wallet": wallet_name, "proposal_id": proposal_id,
        }));

        log!("Proposal #{} amended", proposal_id);
    }

    /// Approve a proposal using a nostr schnorr signature.
    /// `approver_index` indexes into `intent.nostr_approvers`.
    pub fn approve(
        &mut self,
        wallet_name: String,
        proposal_id: u64,
        approver_index: u16,
        pubkey_hex: String,
        signature: String,
        expires_at: u64,
    ) {
        self.verify_nostr_approver(
            wallet_name, proposal_id, approver_index, pubkey_hex, signature, expires_at, "approve",
        );
    }

    /// Cancel-vote a proposal using a nostr schnorr signature.
    pub fn cancel_vote(
        &mut self,
        wallet_name: String,
        proposal_id: u64,
        approver_index: u16,
        pubkey_hex: String,
        signature: String,
        expires_at: u64,
    ) {
        self.verify_nostr_approver(
            wallet_name, proposal_id, approver_index, pubkey_hex, signature, expires_at, "cancel",
        );
    }


    // ── Execution ────────────────────────────────────────────────────

    /// Execute an approved proposal. Requires owner nostr signature.
    pub fn execute(&mut self, wallet_name: String, proposal_id: u64, signature: String, expires_at: u64) {
        self.verify_owner(&format!("execute:{}:{}", wallet_name, proposal_id), &signature, expires_at);
        let pkey = proposal_key(&wallet_name, proposal_id);
        let mut proposal = self.proposals.get(&pkey).expect("ERR_PROPOSAL_NOT_FOUND");
        assert!(proposal.status == ProposalStatus::Approved, "ERR_NOT_APPROVED");

        let ikey = intent_key(&wallet_name, proposal.intent_index);
        let intent = self.intents.get(&ikey).expect("ERR_INTENT_NOT_FOUND");
        
        // Check timelock
        if intent.timelock_seconds > 0 {
            let elapsed = env::block_timestamp() as u128 - proposal.proposed_at as u128;
            let timelock_ns = (intent.timelock_seconds as u128) * 1_000_000_000u128;
            assert!(elapsed >= timelock_ns, "ERR_TIMELOCK_NOT_EXPIRED");
        }

        // Verify params haven't changed since proposal
        let current_hash = hash_params(&intent.params);
        assert_eq!(
            proposal.intent_params_hash, current_hash,
            "ERR_PARAMS_CHANGED: intent schema was modified after proposal"
        );

        let params: serde_json::Value = serde_json::from_str(&proposal.param_values).unwrap_or_default();
        let definition = params.get("definition").and_then(|v| v.as_str());

        match intent.intent_type {
            IntentType::AddIntent => {
                let new_intent: Intent = if let Some(def) = definition {
                    near_sdk::serde_json::from_str(def)
                        .expect("ERR_INVALID_INTENT_DEFINITION")
                } else {
                    // Build intent from top-level params with defaults
                    let proposers: Vec<AccountId> = params.get("proposers")
                        .and_then(|v| v.as_array())
                        .map(|arr| arr.iter().filter_map(|v| v.as_str().and_then(|s| s.parse().ok())).collect())
                        .unwrap_or_default();
                    let approvers: Vec<AccountId> = params.get("approvers")
                        .and_then(|v| v.as_array())
                        .map(|arr| arr.iter().filter_map(|v| v.as_str().and_then(|s| s.parse().ok())).collect())
                        .unwrap_or_default();
                    let pdefs: Vec<ParamDef> = params.get("params")
                        .and_then(|v| v.as_array())
                        .map(|arr| arr.iter().filter_map(|v| near_sdk::serde_json::from_value(v.clone()).ok()).collect())
                        .unwrap_or_default();
                    Intent {
                        wallet_name: wallet_name.clone(),
                        index: 0, // will be overwritten
                        intent_type: IntentType::Custom,
                        name: params.get("name").and_then(|v| v.as_str()).unwrap_or("Custom").to_string(),
                        template: params.get("template").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                        proposers,
                        approvers,
                        nostr_approvers: params.get("nostr_approvers")
                            .and_then(|v| v.as_array())
                            .map(|arr| arr.iter().filter_map(|v| v.as_str().map(|s| s.to_string())).collect())
                            .unwrap_or_default(),
                        approval_threshold: params.get("approval_threshold").and_then(|v| v.as_u64()).unwrap_or(1) as u16,
                        cancellation_threshold: params.get("cancellation_threshold").and_then(|v| v.as_u64()).unwrap_or(1) as u16,
                        timelock_seconds: params.get("timelock_seconds").and_then(|v| v.as_u64()).unwrap_or(0),
                        params: pdefs,
                        execution_gas_tgas: params.get("execution_gas_tgas").and_then(|v| v.as_u64()).unwrap_or(DEFAULT_EXECUTION_GAS_TGAS),
                        active: true,
                        active_proposal_count: 0,
                    }
                };
                let mut wallet = self.wallets.get(&wallet_name).expect("ERR_WALLET_NOT_FOUND");
                let new_index = wallet.intent_index;
                self.intents.insert(&intent_key(&wallet_name, new_index), &new_intent);
                wallet.intent_index += 1;
                self.wallets.insert(&wallet_name, &wallet);
                log!("Intent #{} added to wallet {}", new_index, wallet_name);
            }
            IntentType::RemoveIntent => {
                let idx = params["index"].as_u64().expect("ERR_MISSING_INDEX") as u32;
                let mut ri = self.intents.get(&intent_key(&wallet_name, idx)).expect("ERR_INTENT_NOT_FOUND");
                ri.active = false;
                self.intents.insert(&intent_key(&wallet_name, idx), &ri);
                log!("Intent #{} deactivated", idx);
            }
            IntentType::UpdateIntent => {
                let idx = params["index"].as_u64().expect("ERR_MISSING_INDEX") as u32;
                let def = params.get("definition").and_then(|v| v.as_str());
                let updated: Intent = if let Some(d) = def {
                    near_sdk::serde_json::from_str(d).expect("ERR_INVALID_DEFINITION")
                } else {
                    // Merge: start from existing intent, overlay params
                    let mut existing = self.intents.get(&intent_key(&wallet_name, idx)).expect("ERR_INTENT_NOT_FOUND");
                    if let Some(v) = params.get("name").and_then(|v| v.as_str()) { existing.name = v.to_string(); }
                    if let Some(v) = params.get("template").and_then(|v| v.as_str()) { existing.template = v.to_string(); }
                    if let Some(v) = params.get("approval_threshold").and_then(|v| v.as_u64()) { existing.approval_threshold = v as u16; }
                    if let Some(v) = params.get("cancellation_threshold").and_then(|v| v.as_u64()) { existing.cancellation_threshold = v as u16; }
                    if let Some(v) = params.get("timelock_seconds").and_then(|v| v.as_u64()) { existing.timelock_seconds = v; }
                    if let Some(v) = params.get("execution_gas_tgas").and_then(|v| v.as_u64()) { existing.execution_gas_tgas = v; }
                    if let Some(v) = params.get("active").and_then(|v| v.as_bool()) { existing.active = v; }
                    if let Some(arr) = params.get("proposers").and_then(|v| v.as_array()) {
                        existing.proposers = arr.iter().filter_map(|v| v.as_str().and_then(|s| s.parse().ok())).collect();
                    }
                    if let Some(arr) = params.get("approvers").and_then(|v| v.as_array()) {
                        existing.approvers = arr.iter().filter_map(|v| v.as_str().and_then(|s| s.parse().ok())).collect();
                    }
                    if let Some(arr) = params.get("nostr_approvers").and_then(|v| v.as_array()) {
                        existing.nostr_approvers = arr.iter().filter_map(|v| v.as_str().map(|s| s.to_string())).collect();
                    }
                    if let Some(arr) = params.get("params").and_then(|v| v.as_array()) {
                        existing.params = arr.iter().filter_map(|v| near_sdk::serde_json::from_value(v.clone()).ok()).collect();
                    }
                    existing
                };
                self.intents.insert(&intent_key(&wallet_name, idx), &updated);
                log!("Intent #{} updated", idx);
            }
            IntentType::Custom => {
                if intent.template.contains("deposit") || intent.name.to_lowercase().contains("deposit") {
                    // Deposit NEAR: credit attached deposit to wallet's internal balance
                    let deposit_amount = env::attached_deposit().as_yoctonear();
                    if deposit_amount > 0 {
                        self.credit_near(&wallet_name, deposit_amount);
                    }
                    log!("Deposited {} yoctoNEAR to wallet '{}'", deposit_amount, wallet_name);
                } else if intent.template.contains("transfer") || intent.name.contains("transfer") {
                    let amount_str = params["amount"].as_str()
                        .map(String::from)
                        .or_else(|| params["amount"].as_u64().map(|v| v.to_string()))
                        .unwrap_or_default();
                    let recipient: AccountId = params["recipient"].as_str().expect("ERR_MISSING_RECIPIENT")
                        .parse().expect("ERR_INVALID_RECIPIENT");
                    let amount: u128 = amount_str.parse().expect("ERR_INVALID_AMOUNT");

                    // Check if this is a FT transfer or NEAR transfer
                    if let Some(token_str) = params.get("token").and_then(|v| v.as_str()) {
                        // FT transfer
                        self.debit_ft(&wallet_name, token_str, amount);
                        // TODO: actual FT transfer call to token contract
                        log!("FT transfer: {} of {} to {} (debit recorded)", amount, token_str, recipient);
                    } else {
                        // NEAR transfer from internal balance
                        self.debit_near(&wallet_name, amount);
                        Promise::new(recipient.clone()).transfer(NearToken::from_yoctonear(amount));
                        log!("Transferred {} yoctoNEAR to {}", amount, recipient);
                    }
                } else {
                    let truncated: String = proposal.param_values.chars().take(200).collect();
                    log!("Custom '{}' executed: {}", intent.name, truncated);
                }
            }
        }

        proposal.status = ProposalStatus::Executed;
        self.proposals.insert(&pkey, &proposal);
        let mut intent_mut = intent.clone();
        intent_mut.active_proposal_count = intent_mut.active_proposal_count.saturating_sub(1);
        self.intents.insert(&ikey, &intent_mut);
        self.emit("proposal_executed", serde_json::json!({
            "wallet": wallet_name, "proposal_id": proposal_id,
        }));
    }

    /// Remove an executed/cancelled proposal to reclaim storage. Owner only.
    pub fn cleanup(&mut self, wallet_name: String, proposal_id: u64, signature: String, expires_at: u64) {
        self.verify_owner(&format!("cleanup:{}:{}", wallet_name, proposal_id), &signature, expires_at);

        let pkey = proposal_key(&wallet_name, proposal_id);
        let proposal = self.proposals.get(&pkey).expect("ERR_PROPOSAL_NOT_FOUND");
        assert!(
            proposal.status == ProposalStatus::Executed || proposal.status == ProposalStatus::Cancelled,
            "ERR_NOT_EXECUTABLE: only executed or cancelled proposals can be cleaned up"
        );

        self.proposals.remove(&pkey);
        self.emit("proposal_cleaned", serde_json::json!({
            "wallet": wallet_name, "proposal_id": proposal_id,
        }));
        log!("Proposal #{} cleaned up from wallet '{}'", proposal_id, wallet_name);
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
    /// Shared logic for nostr schnorr approve and cancel_vote.
    fn verify_nostr_approver(
        &mut self,
        wallet_name: String,
        proposal_id: u64,
        approver_index: u16,
        pubkey_hex: String,
        signature: String,
        expires_at: u64,
        action: &str,
    ) {
        let pkey = proposal_key(&wallet_name, proposal_id);
        let mut proposal = self.proposals.get(&pkey).expect("ERR_PROPOSAL_NOT_FOUND");

        assert!(proposal.status == ProposalStatus::Active, "ERR_NOT_ACTIVE");
        assert!(proposal.expires_at > env::block_timestamp(), "ERR_PROPOSAL_EXPIRED");
        assert!(expires_at > env::block_timestamp(), "ERR_SIG_EXPIRED");

        let ikey = intent_key(&wallet_name, proposal.intent_index);
        let intent = self.intents.get(&ikey).expect("ERR_INTENT_NOT_FOUND");
        assert!((approver_index as usize) < intent.nostr_approvers.len(), "ERR_INVALID_NOSTR_APPROVER_INDEX");

        // Verify the pubkey matches the slot
        let expected_pk = &intent.nostr_approvers[approver_index as usize];
        assert_eq!(pubkey_hex, *expected_pk, "ERR_NOSTR_PK_MISMATCH");

        // Build message and verify schnorr signature
        let params: serde_json::Value = serde_json::from_str(&proposal.param_values).unwrap_or_default();
        let msg = message::build_message(&wallet_name, proposal_id, expires_at, action, &intent, &params);

        message::verify_schnorr_signature(&pubkey_hex, &signature, &msg);

        match action {
            "approve" => {
                assert!(!proposal.has_nostr_approved(approver_index as usize), "ERR_NOSTR_ALREADY_APPROVED");
                proposal.set_nostr_approval(approver_index as usize);

                if proposal.approval_count() >= intent.approval_threshold as u32 {
                    proposal.status = ProposalStatus::Approved;
                    proposal.approved_at = env::block_timestamp();

                    self.emit("proposal_approved", serde_json::json!({
                        "wallet": wallet_name, "proposal_id": proposal_id,
                        "approval_count": proposal.approval_count(),
                        "nostr": true,
                    }));

                    log!("Proposal #{} approved (nostr)", proposal_id);
                }
            }
            "cancel" => {
                proposal.set_nostr_cancellation(approver_index as usize);

                if proposal.cancellation_count() >= intent.cancellation_threshold as u32 {
                    proposal.status = ProposalStatus::Cancelled;
                    let mut intent_mut = intent.clone();
                    intent_mut.active_proposal_count = intent_mut.active_proposal_count.saturating_sub(1);
                    self.intents.insert(&ikey, &intent_mut);

                    self.emit("proposal_cancelled", serde_json::json!({
                        "wallet": wallet_name, "proposal_id": proposal_id,
                        "cancellation_count": proposal.cancellation_count(),
                        "nostr": true,
                    }));
                }
            }
            _ => env::panic_str("ERR_INVALID_ACTION"),
        }

        self.proposals.insert(&pkey, &proposal);
    }

    fn create_meta_intents(&mut self, name: &str, owner: &AccountId) {
        let owner_npub = self.owner_npub.clone();
        let make = |index: u32, itype: IntentType, iname: &str, template: &str, params: Vec<ParamDef>| Intent {
            wallet_name: name.to_string(),
            index,
            intent_type: itype,
            name: iname.to_string(),
            template: template.to_string(),
            proposers: vec![owner.clone()],
            approvers: vec![owner.clone()],
            nostr_approvers: vec![owner_npub.clone()],
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
// mod verification;
#[cfg(test)]
// mod integration_tests;
#[cfg(test)]
// mod vm_tests;

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
            nostr_approvers: vec![],
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
            nostr_approval_bitmap: 0, nostr_cancellation_bitmap: 0,
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
            proposers: vec![], approvers: vec![], nostr_approvers: vec![],
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

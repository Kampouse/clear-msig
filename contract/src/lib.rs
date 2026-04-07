use near_sdk::borsh::{BorshDeserialize, BorshSerialize};
use near_sdk::collections::LookupMap;
use near_sdk::json_types::U128;
use near_sdk::serde::{Deserialize, Serialize};
use near_sdk::{
    env, log, near, near_bindgen, AccountId, BorshStorageKey, CryptoHash, NearToken,
    PanicOnDefault, Promise,
};

mod execute;
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

    fn approver_index(&self, account: &AccountId) -> Option<usize> {
        self.approvers.iter().position(|a| a == account)
    }

    fn effective_approver(&self, index: usize, delegations: &LookupMap<String, AccountId>) -> AccountId {
        let original = &self.approvers[index];
        let key = format!("{}:{}:{}", self.wallet_name, self.index, index);
        delegations.get(&key).unwrap_or_else(|| original.clone())
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
            assert!(
                !value.contains('|'),
                "Param '{}' contains illegal character '|'",
                param.name
            );
            assert!(
                !value.contains('\n'),
                "Param '{}' contains newline",
                param.name
            );
            assert!(
                !value.contains('\r'),
                "Param '{}' contains carriage return",
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

    /// Reset all approvals and cancellations (for amendment).
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
}

impl Wallet {
    fn is_active(&self) -> bool {
        !self.name.is_empty()
    }
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
        "Direct call required (no cross-contract calls)"
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

    /// Create a new wallet. Requires 0.5 NEAR storage deposit.
    pub fn create_wallet(&mut self, name: String) {
        let deposit = env::attached_deposit();
        assert!(
            deposit.as_yoctonear() >= STORAGE_DEPOSIT_YOCTO,
            "Insufficient storage deposit: need {} yoctoNEAR, got {}",
            STORAGE_DEPOSIT_YOCTO,
            deposit.as_yoctonear()
        );

        let predecessor = env::predecessor_account_id();
        assert!(self.wallets.get(&name).is_none(), "Wallet already exists");
        assert!(!name.is_empty(), "Name cannot be empty");
        assert!(name.len() <= 64, "Name too long (max 64 chars)");
        assert!(
            name.chars().all(|c| c.is_alphanumeric() || c == '-' || c == '_'),
            "Name must be alphanumeric, hyphens, or underscores"
        );

        let wallet = Wallet {
            name: name.clone(),
            owner: predecessor.clone(),
            proposal_index: 0,
            intent_index: 3,
            created_at: env::block_timestamp(),
            storage_deposit: deposit.as_yoctonear(),
        };
        self.wallets.insert(&name, &wallet);

        let pk = predecessor.clone();
        self.create_meta_intents(&name, &pk);

        self.emit("wallet_created", serde_json::json!({
            "wallet": name,
            "owner": pk.to_string(),
            "deposit": deposit.as_yoctonear().to_string(),
        }));

        log!("Wallet '{}' created", name);
    }

    /// Delete a wallet. Only owner, no active proposals. Refunds storage deposit.
    pub fn delete_wallet(&mut self, name: String) {
        assert_direct_call();
        let wallet = self.wallets.get(&name).expect("Wallet not found");
        assert_eq!(
            env::predecessor_account_id(),
            wallet.owner,
            "Only owner can delete"
        );

        // Verify no active proposals exist
        for i in 0..wallet.intent_index {
            if let Some(intent) = self.intents.get(&intent_key(&name, i)) {
                assert!(
                    intent.active_proposal_count == 0,
                    "Cannot delete wallet with active proposals (intent #{} has {})",
                    i,
                    intent.active_proposal_count
                );
            }
        }

        // Clean up all intents
        for i in 0..wallet.intent_index {
            self.intents.remove(&intent_key(&name, i));
        }

        // Clean up all proposals
        for i in 0..wallet.proposal_index {
            self.proposals.remove(&proposal_key(&name, i));
        }

        // Clean up delegations
        for i in 0..wallet.intent_index {
            if let Some(intent) = self.intents.get(&intent_key(&name, i)) {
                for j in 0..intent.approvers.len() {
                    let dkey = delegation_key(&name, i, j);
                    self.delegations.remove(&dkey);
                }
            }
        }

        // Refund storage deposit to owner
        let refund = NearToken::from_yoctonear(wallet.storage_deposit);
        Promise::new(wallet.owner.clone()).transfer(refund);

        // Remove wallet last
        self.wallets.remove(&name);

        self.emit("wallet_deleted", serde_json::json!({
            "wallet": name,
            "refund": wallet.storage_deposit.to_string(),
        }));

        log!("Wallet '{}' deleted", name);
    }

    /// Transfer ownership to a new account. Owner only.
    pub fn transfer_ownership(&mut self, wallet_name: String, new_owner: AccountId) {
        assert_direct_call();
        let mut wallet = self.wallets.get(&wallet_name).expect("Wallet not found");
        assert_eq!(
            env::predecessor_account_id(),
            wallet.owner,
            "Only owner can transfer ownership"
        );
        assert_ne!(new_owner, wallet.owner, "Already the owner");

        let old_owner = wallet.owner.clone();
        wallet.owner = new_owner.clone();
        self.wallets.insert(&wallet_name, &wallet);

        // Update meta-intent proposers/approvers to include new owner
        for i in 0..3u32 {
            let ikey = intent_key(&wallet_name, i);
            if let Some(mut intent) = self.intents.get(&ikey) {
                // Replace old owner with new owner in proposers
                if let Some(pos) = intent.proposers.iter().position(|a| a == &old_owner) {
                    intent.proposers[pos] = new_owner.clone();
                }
                // Replace in approvers
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

    /// Add a custom intent. Owner only (for bootstrapping).
    /// For production, use AddIntent meta-intent proposals.
    pub fn add_intent(&mut self, wallet_name: String, intent: Intent) {
        let mut wallet = self.wallets.get(&wallet_name).expect("Wallet not found");
        assert_eq!(
            env::predecessor_account_id(),
            wallet.owner,
            "Only owner can add intents directly"
        );
        self.validate_intent(&intent);

        let index = wallet.intent_index;
        let key = intent_key(&wallet_name, index);
        let mut i = intent;
        i.wallet_name = wallet_name.clone();
        i.index = index;
        i.active = true;
        i.active_proposal_count = 0;
        if i.execution_gas_tgas == 0 {
            i.execution_gas_tgas = DEFAULT_EXECUTION_GAS_TGAS;
        }
        self.intents.insert(&key, &i);

        wallet.intent_index = index + 1;
        self.wallets.insert(&wallet_name, &wallet);

        self.emit("intent_added", serde_json::json!({
            "wallet": wallet_name,
            "index": index,
            "name": i.name,
        }));

        log!("Intent #{} added to '{}'", index, wallet_name);
    }

    /// Delegate your approver slot to another account.
    /// The delegate can approve/cancel on your behalf.
    /// Pass your own account to revoke delegation.
    pub fn delegate_approver(
        &mut self,
        wallet_name: String,
        intent_index: u32,
        approver_index: u16,
        delegate: AccountId,
    ) {
        assert_direct_call();
        let caller = env::predecessor_account_id();

        let ikey = intent_key(&wallet_name, intent_index);
        let intent = self.intents.get(&ikey).expect("Intent not found");
        assert!(
            (approver_index as usize) < intent.approvers.len(),
            "Invalid approver index"
        );
        assert_eq!(
            intent.approvers[approver_index as usize],
            caller,
            "Not your approver slot"
        );

        let dkey = delegation_key(&wallet_name, intent_index, approver_index as usize);

        if delegate == caller {
            // Revoke delegation
            self.delegations.remove(&dkey);
            self.emit("delegation_revoked", serde_json::json!({
                "wallet": wallet_name,
                "intent_index": intent_index,
                "approver_index": approver_index,
            }));
        } else {
            self.delegations.insert(&dkey, &delegate);
            self.emit("delegation_set", serde_json::json!({
                "wallet": wallet_name,
                "intent_index": intent_index,
                "approver_index": approver_index,
                "delegate": delegate.to_string(),
            }));
        }
    }

    // ── Proposal Lifecycle ─────────────────────────────────────────────

    /// Create a proposal with a clear-signed message.
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
        let mut wallet = self.wallets.get(&wallet_name).expect("Wallet not found");
        let ikey = intent_key(&wallet_name, intent_index);
        let intent = self.intents.get(&ikey).expect("Intent not found");

        assert!(intent.active, "Intent inactive");
        assert!(
            intent.is_proposer(&proposer) || proposer == wallet.owner,
            "Not a proposer"
        );
        assert!(expires_at > env::block_timestamp(), "Must expire in future");
        assert!(
            expires_at <= env::block_timestamp() + MAX_EXPIRY_NS,
            "Expiry too far in future (max 1 year)"
        );
        assert!(
            intent.active_proposal_count < MAX_ACTIVE_PROPOSALS,
            "Max active proposals reached ({})",
            MAX_ACTIVE_PROPOSALS
        );

        let params: serde_json::Value =
            serde_json::from_str(&param_values).expect("Invalid JSON");
        self.validate_params(&intent, &params);

        let proposal_index = wallet.proposal_index;
        let msg = message::build_message(
            &wallet_name, proposal_index, expires_at, "propose", &intent, &params,
        );

        // Verify signature
        message::verify_signature(&proposer_pubkey, &signature, &msg);
        assert_eq!(proposer_pubkey, signer_pk_hex(), "Signer pubkey mismatch");

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

        self.proposals
            .insert(&proposal_key(&wallet_name, proposal_index), &proposal);

        // Increment active proposal count
        let mut intent_mut = intent.clone();
        intent_mut.active_proposal_count += 1;
        self.intents.insert(&ikey, &intent_mut);

        wallet.proposal_index = proposal_index + 1;
        self.wallets.insert(&wallet_name, &wallet);

        self.emit("proposal_created", serde_json::json!({
            "wallet": wallet_name,
            "proposal_id": proposal_index,
            "intent_index": intent_index,
            "proposer": proposer.to_string(),
            "message": msg,
        }));

        log!("Proposal #{} created for intent #{}", proposal_index, intent_index);
    }

    /// Amend an active proposal. Only the original proposer can amend.
    /// Resets all approvals and cancellations.
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
        let mut proposal = self.proposals.get(&pkey).expect("Proposal not found");

        assert_eq!(proposal.proposer, caller, "Only original proposer can amend");
        assert!(proposal.status == ProposalStatus::Active, "Only active proposals can be amended");
        assert!(expires_at > env::block_timestamp(), "Must expire in future");
        assert!(
            expires_at <= env::block_timestamp() + MAX_EXPIRY_NS,
            "Expiry too far in future (max 1 year)"
        );

        let ikey = intent_key(&wallet_name, proposal.intent_index);
        let intent = self.intents.get(&ikey).expect("Intent not found");

        let params: serde_json::Value =
            serde_json::from_str(&param_values).expect("Invalid JSON");
        self.validate_params(&intent, &params);

        // Build new message with action "amend"
        let msg = message::build_message(
            &wallet_name, proposal_id, expires_at, "amend", &intent, &params,
        );

        message::verify_signature(&proposer_pubkey, &signature, &msg);
        assert_eq!(proposer_pubkey, signer_pk_hex(), "Signer pubkey mismatch");

        // Reset all votes
        proposal.reset_votes();
        proposal.param_values = param_values;
        proposal.expires_at = expires_at;
        proposal.message = msg;
        proposal.intent_params_hash = hash_params(&intent.params);

        self.proposals.insert(&pkey, &proposal);

        self.emit("proposal_amended", serde_json::json!({
            "wallet": wallet_name,
            "proposal_id": proposal_id,
            "proposer": caller.to_string(),
        }));

        log!("Proposal #{} amended", proposal_id);
    }

    /// Approve a proposal with a clear-signed message.
    /// Supports both direct approvers and their delegates.
    pub fn approve(
        &mut self,
        wallet_name: String,
        proposal_id: u64,
        approver_index: u16,
        signature: String,
        expires_at: u64,
    ) {
        assert_direct_call();

        let caller = env::predecessor_account_id();
        let pkey = proposal_key(&wallet_name, proposal_id);
        let mut proposal = self.proposals.get(&pkey).expect("Proposal not found");

        assert!(proposal.status == ProposalStatus::Active, "Not active");
        assert!(proposal.expires_at > env::block_timestamp(), "Proposal expired");
        assert!(expires_at > env::block_timestamp(), "Signature expired");

        let ikey = intent_key(&wallet_name, proposal.intent_index);
        let intent = self.intents.get(&ikey).expect("Intent not found");
        assert!(
            (approver_index as usize) < intent.approvers.len(),
            "Invalid approver index"
        );

        // Check if caller is the direct approver OR a valid delegate
        let original_approver = &intent.approvers[approver_index as usize];
        let dkey = delegation_key(&wallet_name, proposal.intent_index, approver_index as usize);
        let delegate = self.delegations.get(&dkey);
        let is_direct = caller == *original_approver;
        let is_delegate = delegate.as_ref() == Some(&caller);
        assert!(
            is_direct || is_delegate,
            "Not the approver or delegate for slot {}",
            approver_index
        );

        assert!(
            !proposal.has_approved(approver_index as usize),
            "Slot already approved"
        );

        let params: serde_json::Value =
            serde_json::from_str(&proposal.param_values).unwrap_or_default();
        let msg = message::build_message(
            &wallet_name, proposal_id, expires_at, "approve", &intent, &params,
        );

        message::verify_signature(&signer_pk_hex(), &signature, &msg);

        proposal.set_approval(approver_index as usize);

        if proposal.approval_count() >= intent.approval_threshold as u32 {
            proposal.status = ProposalStatus::Approved;
            proposal.approved_at = env::block_timestamp();

            self.emit("proposal_approved", serde_json::json!({
                "wallet": wallet_name,
                "proposal_id": proposal_id,
                "approval_count": proposal.approval_count(),
            }));

            log!("Proposal #{} approved", proposal_id);
        }

        self.proposals.insert(&pkey, &proposal);
    }

    /// Cancel-vote a proposal (requires clear-signed message).
    pub fn cancel_vote(
        &mut self,
        wallet_name: String,
        proposal_id: u64,
        approver_index: u16,
        signature: String,
        expires_at: u64,
    ) {
        assert_direct_call();

        let caller = env::predecessor_account_id();
        let pkey = proposal_key(&wallet_name, proposal_id);
        let mut proposal = self.proposals.get(&pkey).expect("Proposal not found");

        assert!(proposal.status == ProposalStatus::Active, "Not active");
        assert!(proposal.expires_at > env::block_timestamp(), "Proposal expired");
        assert!(expires_at > env::block_timestamp(), "Signature expired");

        let ikey = intent_key(&wallet_name, proposal.intent_index);
        let mut intent = self.intents.get(&ikey).expect("Intent not found");
        assert!(
            (approver_index as usize) < intent.approvers.len(),
            "Invalid approver index"
        );

        // Check delegate
        let original = &intent.approvers[approver_index as usize];
        let dkey = delegation_key(&wallet_name, proposal.intent_index, approver_index as usize);
        let delegate = self.delegations.get(&dkey);
        assert!(
            caller == *original || delegate.as_ref() == Some(&caller),
            "Not the approver or delegate for slot {}",
            approver_index
        );

        let params: serde_json::Value =
            serde_json::from_str(&proposal.param_values).unwrap_or_default();
        let msg = message::build_message(
            &wallet_name, proposal_id, expires_at, "cancel", &intent, &params,
        );

        message::verify_signature(&signer_pk_hex(), &signature, &msg);

        proposal.set_cancellation(approver_index as usize);

        if proposal.cancellation_count() >= intent.cancellation_threshold as u32 {
            proposal.status = ProposalStatus::Cancelled;
            intent.active_proposal_count = intent.active_proposal_count.saturating_sub(1);
            self.intents.insert(&ikey, &intent);

            self.emit("proposal_cancelled", serde_json::json!({
                "wallet": wallet_name,
                "proposal_id": proposal_id,
                "cancellation_count": proposal.cancellation_count(),
            }));
        }

        self.proposals.insert(&pkey, &proposal);
    }

    // ── Views ──────────────────────────────────────────────────────────

    pub fn get_wallet(&self, name: String) -> Option<Wallet> {
        self.wallets.get(&name)
    }

    pub fn get_intent(&self, wallet_name: String, index: u32) -> Option<Intent> {
        self.intents.get(&intent_key(&wallet_name, index))
    }

    pub fn list_intents(&self, wallet_name: String) -> Vec<Intent> {
        let Some(wallet) = self.wallets.get(&wallet_name) else {
            return Vec::new();
        };
        (0..wallet.intent_index)
            .filter_map(|i| self.intents.get(&intent_key(&wallet_name, i)))
            .collect()
    }

    pub fn get_proposal(&self, wallet_name: String, id: u64) -> Option<Proposal> {
        self.proposals.get(&proposal_key(&wallet_name, id))
    }

    pub fn list_proposals(&self, wallet_name: String) -> Vec<Proposal> {
        let Some(wallet) = self.wallets.get(&wallet_name) else {
            return Vec::new();
        };
        (0..wallet.proposal_index)
            .filter_map(|i| self.proposals.get(&proposal_key(&wallet_name, i)))
            .collect()
    }

    pub fn get_proposal_message(&self, wallet_name: String, id: u64) -> Option<String> {
        self.proposals
            .get(&proposal_key(&wallet_name, id))
            .map(|p| p.message)
    }

    pub fn get_delegation(
        &self,
        wallet_name: String,
        intent_index: u32,
        approver_index: u16,
    ) -> Option<AccountId> {
        let dkey = delegation_key(&wallet_name, intent_index, approver_index as usize);
        self.delegations.get(&dkey)
    }

    pub fn get_event_nonce(&self) -> u64 {
        self.event_nonce
    }
}

// ── Private Helpers ────────────────────────────────────────────────────────

impl Contract {
    fn emit(&mut self, event: &str, data: serde_json::Value) {
        self.event_nonce += 1;
        let nonce = self.event_nonce;
        env::log_str(&format!(
            "EVENT_JSON:{}",
            serde_json::json!({
                "standard": "clear-msig",
                "version": "1.0.0",
                "event": event,
                "nonce": nonce,
                "data": data,
            })
        ));
    }

    fn create_meta_intents(&mut self, name: &str, owner: &AccountId) {
        let make_intent = |index: u32, itype: IntentType, iname: &str, template: &str, params: Vec<ParamDef>| Intent {
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

        self.intents.insert(
            &intent_key(name, 0),
            &make_intent(
                0,
                IntentType::AddIntent,
                "AddIntent",
                "add intent definition_hash: {hash}",
                vec![ParamDef { name: "hash".to_string(), param_type: ParamType::String, max_value: None }],
            ),
        );
        self.intents.insert(
            &intent_key(name, 1),
            &make_intent(
                1,
                IntentType::RemoveIntent,
                "RemoveIntent",
                "remove intent {index}",
                vec![ParamDef { name: "index".to_string(), param_type: ParamType::U64, max_value: None }],
            ),
        );
        self.intents.insert(
            &intent_key(name, 2),
            &make_intent(
                2,
                IntentType::UpdateIntent,
                "UpdateIntent",
                "update intent {index}",
                vec![ParamDef { name: "index".to_string(), param_type: ParamType::U64, max_value: None }],
            ),
        );
    }

    fn validate_intent(&self, intent: &Intent) {
        assert!(
            intent.approvers.len() <= MAX_APPROVERS,
            "Max {} approvers (bitmap limit)",
            MAX_APPROVERS
        );
        assert!(
            intent.approval_threshold as usize <= intent.approvers.len(),
            "Threshold exceeds approvers"
        );
        assert!(
            intent.cancellation_threshold as usize <= intent.approvers.len(),
            "Cancellation threshold exceeds approvers"
        );
        assert!(
            !intent.params.is_empty(),
            "Intent must have at least one param definition"
        );
        assert!(
            intent.execution_gas_tgas <= MAX_EXECUTION_GAS_TGAS,
            "Execution gas exceeds max ({} Tgas)",
            MAX_EXECUTION_GAS_TGAS
        );
    }

    fn validate_params(&self, intent: &Intent, params: &serde_json::Value) {
        for pd in &intent.params {
            match params.get(&pd.name) {
                None => panic!("Missing param: {}", pd.name),
                Some(val) => match pd.param_type {
                    ParamType::AccountId => {
                        let s = val.as_str().unwrap_or_else(|| panic!("{}: expected string", pd.name));
                        s.parse::<AccountId>().unwrap_or_else(|_| panic!("{}: invalid account ID", pd.name));
                    }
                    ParamType::U64 => {
                        let v = val
                            .as_u64()
                            .or_else(|| val.as_str().and_then(|s| s.parse::<u64>().ok()))
                            .unwrap_or_else(|| panic!("{}: expected u64, got {:?}", pd.name, val));
                        if let Some(max) = &pd.max_value {
                            assert!((v as u128) <= max.0, "{} exceeds max", pd.name);
                        }
                    }
                    ParamType::U128 => {
                        let s = match val {
                            serde_json::Value::String(s) => s.clone(),
                            serde_json::Value::Number(n) => n.to_string(),
                            _ => panic!("{}: expected number, got {:?}", pd.name, val),
                        };
                        let v: u128 = s
                            .parse()
                            .unwrap_or_else(|_| panic!("{}: invalid u128 '{}'", pd.name, s));
                        if let Some(max) = &pd.max_value {
                            assert!(v <= max.0, "{} exceeds max", pd.name);
                        }
                    }
                    ParamType::String => {
                        val.as_str()
                            .unwrap_or_else(|| panic!("{}: expected string", pd.name));
                    }
                    ParamType::Bool => {
                        val.as_bool()
                            .unwrap_or_else(|| panic!("{}: expected bool", pd.name));
                    }
                },
            }
        }
    }
}

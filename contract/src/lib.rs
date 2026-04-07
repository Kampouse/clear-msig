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

pub type Balance = u128;

// ── Storage Keys ──────────────────────────────────────────────────────────

#[derive(BorshSerialize, BorshStorageKey)]
#[borsh(crate = "near_sdk::borsh")]
enum StorageKey {
    Wallets,
    Intents,
    Proposals,
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

    fn render_template(&self, param_values: &serde_json::Value) -> String {
        let mut result = self.template.clone();
        for param in &self.params {
            let placeholder = format!("{{{}}}", param.name);
            let value = match param_values.get(&param.name) {
                Some(v) => match param.param_type {
                    ParamType::AccountId => v.as_str().unwrap_or("unknown").to_string(),
                    ParamType::U64 => {
                        v.as_u64().map(|n| n.to_string()).unwrap_or_default()
                    }
                    ParamType::U128 => {
                        // Handle both string and number representations
                        v.as_str().map(|s| s.to_string())
                            .or_else(|| match v {
                                serde_json::Value::Number(n) => Some(n.to_string()),
                                _ => None,
                            })
                            .unwrap_or_default()
                    }
                    ParamType::String => v.as_str().unwrap_or("").to_string(),
                    ParamType::Bool => v.as_bool().map(|b| b.to_string()).unwrap_or_default(),
                },
                None => continue,
            };
            // Sanitize: reject values containing message format characters
            assert!(!value.contains('|'), "Param {} contains invalid character '|'", param.name);
            assert!(!value.contains('\n'), "Param {} contains newline", param.name);
            assert!(!value.contains('\r'), "Param {} contains carriage return", param.name);
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
}

impl Proposal {
    fn approval_count(&self) -> u32 {
        self.approval_bitmap.count_ones()
    }

    fn cancellation_count(&self) -> u32 {
        self.cancellation_bitmap.count_ones()
    }

    fn has_approved(&self, idx: usize) -> bool {
        self.approval_bitmap & (1u64 << idx) != 0
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
}

#[derive(Clone, Debug)]
#[near(serializers = [borsh, json])]
pub struct Wallet {
    pub name: String,
    pub owner: AccountId,
    pub proposal_index: u64,
    pub intent_index: u32,
    pub created_at: u64,
}

// ── Composite keys for flat storage ────────────────────────────────────────

fn intent_key(wallet: &str, index: u32) -> String {
    format!("{}:i:{}", wallet, index)
}

fn proposal_key(wallet: &str, id: u64) -> String {
    format!("{}:p:{}", wallet, id)
}

// ── Contract ───────────────────────────────────────────────────────────────

#[derive(BorshDeserialize, BorshSerialize, PanicOnDefault)]
#[borsh(crate = "near_sdk::borsh")]
#[near_bindgen]
pub struct Contract {
    wallets: LookupMap<String, Wallet>,
    intents: LookupMap<String, Intent>,
    proposals: LookupMap<String, Proposal>,
}

#[near_bindgen]
impl Contract {
    #[init]
    pub fn new() -> Self {
        Self {
            wallets: LookupMap::new(StorageKey::Wallets),
            intents: LookupMap::new(StorageKey::Intents),
            proposals: LookupMap::new(StorageKey::Proposals),
        }
    }

    /// Create a new wallet with 3 meta-intents
    pub fn create_wallet(&mut self, name: String) {
        assert_eq!(env::attached_deposit(), NearToken::from_yoctonear(0), "No deposit required");
        let predecessor = env::predecessor_account_id();
        assert!(self.wallets.get(&name).is_none(), "Wallet already exists");
        assert!(!name.is_empty(), "Name cannot be empty");
        assert!(name.len() <= 64, "Name too long (max 64 chars)");

        let wallet = Wallet {
            name: name.clone(),
            owner: predecessor.clone(),
            proposal_index: 0,
            intent_index: 3,
            created_at: env::block_timestamp(),
        };
        self.wallets.insert(&name, &wallet);

        let pk = predecessor.clone();
        // Validate approver count for meta-intents
        // (meta-intents have 1 approver, so always valid, but future-proof)
        // Meta-intent 0: AddIntent
        self.intents.insert(
            &intent_key(&name, 0),
            &Intent {
                wallet_name: name.clone(),
                index: 0,
                intent_type: IntentType::AddIntent,
                name: "AddIntent".to_string(),
                template: "add intent definition_hash: {hash}".to_string(),
                proposers: vec![pk.clone()],
                approvers: vec![pk.clone()],
                approval_threshold: 1,
                cancellation_threshold: 1,
                timelock_seconds: 0,
                params: vec![ParamDef { name: "hash".to_string(), param_type: ParamType::String, max_value: None }],
                active: true,
                active_proposal_count: 0,
            },
        );
        // Meta-intent 1: RemoveIntent
        self.intents.insert(
            &intent_key(&name, 1),
            &Intent {
                wallet_name: name.clone(),
                index: 1,
                intent_type: IntentType::RemoveIntent,
                name: "RemoveIntent".to_string(),
                template: "remove intent {index}".to_string(),
                proposers: vec![pk.clone()],
                approvers: vec![pk.clone()],
                approval_threshold: 1,
                cancellation_threshold: 1,
                timelock_seconds: 0,
                params: vec![ParamDef { name: "index".to_string(), param_type: ParamType::U64, max_value: None }],
                active: true,
                active_proposal_count: 0,
            },
        );
        // Meta-intent 2: UpdateIntent
        self.intents.insert(
            &intent_key(&name, 2),
            &Intent {
                wallet_name: name.clone(),
                index: 2,
                intent_type: IntentType::UpdateIntent,
                name: "UpdateIntent".to_string(),
                template: "update intent {index}".to_string(),
                proposers: vec![pk.clone()],
                approvers: vec![pk.clone()],
                approval_threshold: 1,
                cancellation_threshold: 1,
                timelock_seconds: 0,
                params: vec![ParamDef { name: "index".to_string(), param_type: ParamType::U64, max_value: None }],
                active: true,
                active_proposal_count: 0,
            },
        );

        log!("Wallet '{}' created", name);
    }

    /// Add a custom intent via governance (requires approved AddIntent proposal)
    /// Direct owner add is also supported for bootstrapping
    pub fn add_intent(&mut self, wallet_name: String, intent: Intent) {
        let mut wallet = self.wallets.get(&wallet_name).expect("Wallet not found");
        let caller = env::predecessor_account_id();
        assert!(caller == wallet.owner, "Only owner can add intents directly");
        assert!(intent.approvers.len() <= 64, "Max 64 approvers (bitmap limit)");
        assert!(intent.approval_threshold as usize <= intent.approvers.len(), "Threshold exceeds approvers");
        assert!(intent.cancellation_threshold as usize <= intent.approvers.len(), "Cancellation threshold exceeds approvers");

        let index = wallet.intent_index;
        let key = intent_key(&wallet_name, index);
        let mut i = intent;
        i.wallet_name = wallet_name.clone();
        i.index = index;
        i.active = true;
        i.active_proposal_count = 0;
        self.intents.insert(&key, &i);

        wallet.intent_index = index + 1;
        self.wallets.insert(&wallet_name, &wallet);
        log!("Intent #{} added to '{}'", index, wallet_name);
    }

    /// Propose: create proposal with signed message
    pub fn propose(
        &mut self,
        wallet_name: String,
        intent_index: u32,
        param_values: String,
        expires_at: u64,
        proposer_pubkey: String,
        signature: String,
    ) {
        let proposer = env::predecessor_account_id();
        let mut wallet = self.wallets.get(&wallet_name).expect("Wallet not found");
        let ikey = intent_key(&wallet_name, intent_index);
        let intent = self.intents.get(&ikey).expect("Intent not found");

        assert!(intent.active, "Intent inactive");
        assert!(intent.is_proposer(&proposer) || proposer == wallet.owner, "Not a proposer");
        assert!(expires_at > env::block_timestamp(), "Must expire in future");

        let params: serde_json::Value = serde_json::from_str(&param_values).expect("Invalid JSON");
        self.validate_params(&intent, &params);

        let proposal_index = wallet.proposal_index;
        let msg = message::build_message(&wallet_name, proposal_index, expires_at, "propose", &intent, &params);

        message::verify_signature(&proposer_pubkey, &signature, &msg);

        // Verify the pubkey belongs to the signer (prevents forged signatures)
        let signer_pk = env::signer_account_pk();
        let signer_pk_bytes = signer_pk.into_bytes();
        let signer_pk_raw = if signer_pk_bytes.len() == 33 { &signer_pk_bytes[1..] } else { &signer_pk_bytes[..] };
        let signer_pk_hex = hex_encode(signer_pk_raw);
        assert_eq!(proposer_pubkey, signer_pk_hex, "Signer pubkey mismatch");

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
        };

        let pkey = proposal_key(&wallet_name, proposal_index);
        self.proposals.insert(&pkey, &proposal);

        // Increment active proposal count
        let mut intent_mut = intent.clone();
        intent_mut.active_proposal_count += 1;
        self.intents.insert(&ikey, &intent_mut);

        wallet.proposal_index = proposal_index + 1;
        self.wallets.insert(&wallet_name, &wallet);

        env::log_str(&format!("EVENT_JSON:{}", serde_json::json!({
            "standard": "clear-msig",
            "version": "1.0.0",
            "event": "proposal_created",
            "data": {
                "wallet": wallet_name,
                "proposal_id": proposal_index,
                "intent_index": intent_index,
                "proposer": proposer.to_string(),
                "message": msg,
            }
        })));

        log!("Proposal #{} created for intent #{}", proposal_index, intent_index);
    }

    /// Approve a proposal
    pub fn approve(
        &mut self,
        wallet_name: String,
        proposal_id: u64,
        approver_index: u16,
        signature: String,
        expires_at: u64,
    ) {
        let approver = env::predecessor_account_id();
        let pkey = proposal_key(&wallet_name, proposal_id);
        let mut proposal = self.proposals.get(&pkey).expect("Proposal not found");
        assert!(proposal.status == ProposalStatus::Active, "Not active");
        assert!(proposal.expires_at > env::block_timestamp(), "Proposal expired");
        assert!(expires_at > env::block_timestamp(), "Signature expired");

        let ikey = intent_key(&wallet_name, proposal.intent_index);
        let intent = self.intents.get(&ikey).expect("Intent not found");
        assert!((approver_index as usize) < intent.approvers.len(), "Invalid index");
        assert_eq!(intent.approvers[approver_index as usize], approver, "Approver mismatch");
        assert!(!proposal.has_approved(approver_index as usize), "Already approved");

        let params: serde_json::Value = serde_json::from_str(&proposal.param_values).unwrap_or_default();
        let msg = message::build_message(&wallet_name, proposal_id, expires_at, "approve", &intent, &params);
        let pk = env::signer_account_pk();
        let pk_bytes = pk.into_bytes();
        // near-sdk PublicKey includes 1-byte curve prefix (0x00 for ed25519)
        let pk_raw = if pk_bytes.len() == 33 { &pk_bytes[1..] } else { &pk_bytes[..] };
        let pk_hex = hex_encode(pk_raw);
        message::verify_signature(&pk_hex, &signature, &msg);

        proposal.set_approval(approver_index as usize);

        if proposal.approval_count() >= intent.approval_threshold as u32 {
            proposal.status = ProposalStatus::Approved;
            proposal.approved_at = env::block_timestamp();

            env::log_str(&format!("EVENT_JSON:{}", serde_json::json!({
                "standard": "clear-msig",
                "version": "1.0.0",
                "event": "proposal_approved",
                "data": {
                    "wallet": wallet_name,
                    "proposal_id": proposal_id,
                    "approval_count": proposal.approval_count(),
                }
            })));

            log!("Proposal #{} approved", proposal_id);
        }

        self.proposals.insert(&pkey, &proposal);
    }

    /// Cancel-vote a proposal
    pub fn cancel_vote(&mut self, wallet_name: String, proposal_id: u64, approver_index: u16) {
        let approver = env::predecessor_account_id();
        let pkey = proposal_key(&wallet_name, proposal_id);
        let mut proposal = self.proposals.get(&pkey).expect("Proposal not found");
        assert!(proposal.status == ProposalStatus::Active, "Not active");

        let ikey = intent_key(&wallet_name, proposal.intent_index);
        let mut intent = self.intents.get(&ikey).expect("Intent not found");
        assert_eq!(intent.approvers[approver_index as usize], approver, "Approver mismatch");

        proposal.set_cancellation(approver_index as usize);

        if proposal.cancellation_count() >= intent.cancellation_threshold as u32 {
            proposal.status = ProposalStatus::Cancelled;
            intent.active_proposal_count = intent.active_proposal_count.saturating_sub(1);
            self.intents.insert(&ikey, &intent);
        }

        self.proposals.insert(&pkey, &proposal);
    }

    // ── Views ──────────────────────────────────────────────────────────────

    pub fn get_wallet(&self, name: String) -> Option<Wallet> {
        self.wallets.get(&name)
    }

    pub fn get_intent(&self, wallet_name: String, index: u32) -> Option<Intent> {
        self.intents.get(&intent_key(&wallet_name, index))
    }

    pub fn list_intents(&self, wallet_name: String) -> Vec<Intent> {
        let Some(wallet) = self.wallets.get(&wallet_name) else { return Vec::new(); };
        let mut result = Vec::new();
        for i in 0..wallet.intent_index {
            if let Some(intent) = self.intents.get(&intent_key(&wallet_name, i)) {
                result.push(intent);
            }
        }
        result
    }

    pub fn get_proposal(&self, wallet_name: String, id: u64) -> Option<Proposal> {
        self.proposals.get(&proposal_key(&wallet_name, id))
    }

    pub fn list_proposals(&self, wallet_name: String) -> Vec<Proposal> {
        let Some(wallet) = self.wallets.get(&wallet_name) else { return Vec::new(); };
        let mut result = Vec::new();
        for i in 0..wallet.proposal_index {
            if let Some(p) = self.proposals.get(&proposal_key(&wallet_name, i)) {
                result.push(p);
            }
        }
        result
    }

    pub fn get_proposal_message(&self, wallet_name: String, id: u64) -> Option<String> {
        self.proposals.get(&proposal_key(&wallet_name, id)).map(|p| p.message)
    }
}

impl Contract {
    fn validate_params(&self, intent: &Intent, params: &serde_json::Value) {
        for pd in &intent.params {
            match params.get(&pd.name) {
                None => panic!("Missing param: {}", pd.name),
                Some(val) => match pd.param_type {
                    ParamType::AccountId => { val.as_str().expect(&format!("{}: expected string", pd.name)); }
                    ParamType::U64 => {
                        let v = val.as_u64().or_else(|| val.as_str().and_then(|s| s.parse::<u64>().ok())).unwrap_or_else(|| panic!("{}: expected number, got {:?}", pd.name, val));
                        if let Some(max) = pd.max_value { assert!((v as u128) <= max.0, "{} exceeds max", pd.name); }
                    }
                    ParamType::U128 => {
                        // serde_json can't represent large numbers — always parse from string
                        let s = match val {
                            serde_json::Value::String(s) => s.clone(),
                            serde_json::Value::Number(n) => n.to_string(),
                            _ => panic!("{}: expected number, got {:?}", pd.name, val),
                        };
                        let v: u128 = s.parse().unwrap_or_else(|_| panic!("{}: invalid number '{}'", pd.name, s));
                        if let Some(max) = pd.max_value { assert!(v <= max.0, "{} exceeds max", pd.name); }
                    }
                    ParamType::String => { val.as_str().expect(&format!("{}: expected string", pd.name)); }
                    ParamType::Bool => { val.as_bool().expect(&format!("{}: expected bool", pd.name)); }
                },
            }
        }
    }
}

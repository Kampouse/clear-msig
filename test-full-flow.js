const { schnorr } = require('@noble/curves/secp256k1');
const { sha256 } = require('@noble/hashes/sha256');
const { bytesToHex, hexToBytes } = require('@noble/hashes/utils');

const privKey = hexToBytes('a'.repeat(64));
const pubKey = schnorr.getPublicKey(privKey);
const npub = bytesToHex(pubKey);

// Step 1: We need to first add an intent via the AddIntent meta-intent
// The intent will have nostr_approvers set to our npub

console.log('=== STEP 1: AddIntent proposal ===');
const addIntentParams = JSON.stringify({
  hash: "v1",
  name: "Transfer NEAR",
  template: "transfer {amount} NEAR to {recipient}",
  proposers: [],
  approvers: [],
  nostr_approvers: [npub],
  approval_threshold: 1,
  cancellation_threshold: 1,
  params: [
    { name: "amount", param_type: "U128", max_value: null },
    { name: "recipient", param_type: "AccountId", max_value: null }
  ]
});
console.log('AddIntent params:', addIntentParams);

// Step 2: After proposal is created, sign the proposal message
// The message format from the contract:
// "expires <timestamp>: <action> <content> | wallet: treasury proposal: 0"
// For AddIntent: content = "add intent definition_hash: v1"

const expiresNano = 1893456000000000000n;
const expiresStr = '1893456000.000000000';
const msg = `expires ${expiresStr}: approve add intent definition_hash: v1 | wallet: treasury proposal: 0`;
console.log('\n=== STEP 2: Sign proposal message ===');
console.log('Message:', msg);

const hash = sha256(new TextEncoder().encode(msg));
const sig = schnorr.sign(hash, privKey);
console.log('Signature:', bytesToHex(sig));
console.log('Local verify:', schnorr.verify(sig, hash, pubKey));

console.log('\n=== nostr_approve call args ===');
console.log(JSON.stringify({
  wallet_name: "treasury",
  proposal_id: 0,
  approver_index: 0,
  pubkey_hex: npub,
  signature: bytesToHex(sig),
  expires_at: expiresNano.toString()
}, null, 2));

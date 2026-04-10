/**
 * Full nostr multisig flow test:
 * 1. Generate nostr keypair
 * 2. Create intent with nostr_approvers
 * 3. Propose
 * 4. Sign locally with nostr key
 * 5. Submit nostr_approve on-chain
 * 6. Execute
 */
const { schnorr, secp256k1 } = require('@noble/curves/secp256k1');
const { sha256 } = require('@noble/curves/sha256');
const { bytesToHex, hexToBytes } = require('@noble/curves/utils');

// --- Generate nostr keypair ---
const privateKey = hexToBytes('a'.repeat(64)); // test key
const publicKey = schnorr.getPublicKey(privateKey); // 32-byte x-only
const npubHex = bytesToHex(publicKey);

console.log('=== NOSTR KEYPAIR ===');
console.log('Private key (nsec):', bytesToHex(privateKey));
console.log('Public key (npub hex):', npubHex);

// --- Build the message that needs signing ---
// This matches the contract's build_message format:
// "expires <timestamp>: <action> <content> | wallet: <name> proposal: <index>"
const walletName = 'treasury';
const proposalIndex = 0;
const expiresAt = '1893456000.000000000'; // far future
const action = 'approve';
const content = 'transfer 1000000000000000000000000 NEAR to test.near';

const message = `expires ${expiresAt}: ${action} ${content} | wallet: ${walletName} proposal: ${proposalIndex}`;
console.log('\n=== MESSAGE TO SIGN ===');
console.log(message);

// --- Sign with schnorr (BIP-340 / Nostr compatible) ---
// The contract uses SHA-256 to hash the message before verification (matching k256)
const msgHash = sha256(new TextEncoder().encode(message));
const signature = schnorr.sign(msgHash, privateKey);
const sigHex = bytesToHex(signature);

console.log('\n=== SIGNATURE ===');
console.log('Message hash:', bytesToHex(msgHash));
console.log('Schnorr signature:', sigHex);

// --- Verify locally ---
const valid = schnorr.verify(signature, msgHash, publicKey);
console.log('Local verification:', valid ? '✅ VALID' : '❌ INVALID');

// --- Output the contract call args ---
console.log('\n=== CONTRACT CALL ARGS ===');
console.log(JSON.stringify({
  method: 'nostr_approve',
  args: {
    wallet_name: walletName,
    proposal_id: proposalIndex,
    approver_index: 0,
    pubkey_hex: npubHex,
    signature: sigHex,
    expires_at: 1893456000000000000n.toString()
  }
}, null, 2));

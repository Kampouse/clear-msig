/**
 * Full nostr multisig flow test
 */
import { schnorr } from '@noble/curves/secp256k1';
import { sha256 } from '@noble/hashes/sha256';
import { bytesToHex, hexToBytes } from '@noble/hashes/utils';

// --- Generate nostr keypair ---
const privateKey = hexToBytes('a'.repeat(64));
const publicKey = schnorr.getPublicKey(privateKey);
const npubHex = bytesToHex(publicKey);

console.log('=== NOSTR KEYPAIR ===');
console.log('nsec:', bytesToHex(privateKey));
console.log('npub hex:', npubHex);

// --- Build message (matches contract's build_message format) ---
const walletName = 'treasury';
const proposalIndex = 0;
const expiresAt = '1893456000.000000000';
const action = 'approve';
const content = 'transfer 1000000000000000000000000 NEAR to test.near';

const message = `expires ${expiresAt}: ${action} ${content} | wallet: ${walletName} proposal: ${proposalIndex}`;
console.log('\n=== MESSAGE ===');
console.log(message);

// --- Sign (SHA-256 hash then BIP-340 schnorr) ---
const msgHash = sha256(new TextEncoder().encode(message));
const signature = schnorr.sign(msgHash, privateKey);
const sigHex = bytesToHex(signature);

console.log('\n=== SIGNATURE ===');
console.log('msg hash:', bytesToHex(msgHash));
console.log('schnorr sig:', sigHex);

// --- Verify locally ---
const valid = schnorr.verify(signature, msgHash, publicKey);
console.log('local verify:', valid ? '✅' : '❌');

// --- Contract call args ---
console.log('\n=== CONTRACT CALL ===');
console.log(JSON.stringify({
  wallet_name: walletName,
  proposal_id: 0,
  approver_index: 0,
  pubkey_hex: npubHex,
  signature: sigHex,
  expires_at: '1893456000000000000'
}, null, 2));

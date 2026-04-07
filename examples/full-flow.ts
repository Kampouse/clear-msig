/**
 * Example: Full clear-msig flow
 *
 * Run: npx ts-node examples/full-flow.ts
 */

import { KeyPair } from 'near-api-js';
import { ClearMsig, buildMessage, signMessage, nearToYocto, u128 } from '../reference/index';

// ── Config ─────────────────────────────────────────────────────────────────

const CONTRACT_ID = 'clear-msig.kampouse.testnet';
const NETWORK = 'testnet';

async function main() {
  // In production, load from key store or wallet connection
  const aliceKey = KeyPair.fromRandom('ed25519');
  const bobKey = KeyPair.fromRandom('ed25519');

  const client = new ClearMsig(CONTRACT_ID, NETWORK);

  console.log('╔══════════════════════════════════════╗');
  console.log('║   clear-msig Reference Flow Demo     ║');
  console.log('╚══════════════════════════════════════╝\n');

  // ── Step 1: Message Building ──────────────────────────────────────────

  console.log('1. Building a clear-sign message\n');

  const intent = {
    wallet_name: 'treasury',
    index: 3,
    intent_type: 'Custom' as const,
    name: 'Transfer NEAR',
    template: 'transfer {amount} yoctoNEAR to {recipient}',
    proposers: [],
    approvers: [],
    approval_threshold: 2,
    cancellation_threshold: 1,
    timelock_seconds: 0,
    params: [
      { name: 'amount', param_type: 'U128' as const, max_value: null },
      { name: 'recipient', param_type: 'AccountId' as const, max_value: null },
    ],
    active: true,
    active_proposal_count: 0,
  };

  const params = {
    amount: nearToYocto('1.5'), // "1500000000000000000000000"
    recipient: 'bob.testnet',
  };

  const expiresAtNs = BigInt(1893456000) * BigInt(1_000_000_000);

  const proposeMessage = buildMessage('treasury', 0, expiresAtNs, 'propose', intent, params);
  console.log(`   Message: "${proposeMessage}"\n`);

  // ── Step 2: Signing ───────────────────────────────────────────────────

  console.log('2. Signing the message\n');

  const signature = signMessage(aliceKey, proposeMessage);
  console.log(`   Signature: ${signature.slice(0, 32)}...`);
  console.log(`   Public key: ${aliceKey.getPublicKey().toString()}\n`);

  // ── Step 3: Approve Message ───────────────────────────────────────────

  console.log('3. Building approve message\n');

  const approveMessage = buildMessage('treasury', 0, expiresAtNs, 'approve', intent, params);
  console.log(`   Message: "${approveMessage}"\n`);

  const approveSignature = signMessage(bobKey, approveMessage);
  console.log(`   Signature: ${approveSignature.slice(0, 32)}...\n`);

  // ── Step 4: U128 Safety ───────────────────────────────────────────────

  console.log('4. U128 precision handling\n');

  // ❌ DANGEROUS: JavaScript Number loses precision
  const bad = 1000000000000000000000000;
  console.log(`   Number:     ${bad} (precision lost!)`);

  // ✅ SAFE: Use string or BigInt
  const good = u128('1000000000000000000000000');
  console.log(`   u128:       ${good} (exact)\n`);

  // NEAR conversion helpers
  console.log(`   1 NEAR    = ${nearToYocto('1')} yocto`);
  console.log(`   1.5 NEAR  = ${nearToYocto('1.5')} yocto`);
  console.log(`   0.001 NEAR = ${nearToYocto('0.001')} yocto\n`);

  console.log('✅ Demo complete!');
  console.log('\nIn production, replace aliceKey/bobKey with connected wallet accounts');
  console.log('and call client.propose() / client.approve() / client.execute()');
}

main().catch(console.error);

/**
 * Example: Full clear-msig flow with all features
 *
 * Run: npx ts-node examples/full-flow.ts
 */

import { KeyPair } from 'near-api-js';
import {
  ClearMsig, buildMessage, signMessage, nearToYocto, u128,
  expiryFromNow, STORAGE_DEPOSIT, DEFAULT_EXECUTION_GAS_TGAS,
} from '../reference/index';

const CONTRACT_ID = 'clear-msig.kampouse.testnet';
const NETWORK = 'testnet' as const;

async function main() {
  const aliceKey = KeyPair.fromRandom('ed25519');
  const bobKey = KeyPair.fromRandom('ed25519');
  const client = new ClearMsig(CONTRACT_ID, NETWORK);

  console.log('╔══════════════════════════════════════════╗');
  console.log('║   clear-msig Reference Flow Demo (v2)    ║');
  console.log('╚══════════════════════════════════════════╝\n');

  // ── 1. Message Building ────────────────────────────────────────────

  const intent = {
    wallet_name: 'treasury',
    index: 3,
    intent_type: 'Custom' as const,
    name: 'Transfer NEAR',
    template: 'transfer {amount} yoctoNEAR to {recipient}',
    proposers: [], approvers: [],
    approval_threshold: 2, cancellation_threshold: 1,
    timelock_seconds: 0,
    params: [
      { name: 'amount', param_type: 'U128' as const, max_value: null },
      { name: 'recipient', param_type: 'AccountId' as const, max_value: null },
    ],
    execution_gas_tgas: DEFAULT_EXECUTION_GAS_TGAS,
    active: true, active_proposal_count: 0,
  };

  const params = {
    amount: nearToYocto('1.5'),
    recipient: 'bob.testnet',
  };

  const expiry = expiryFromNow(86400); // 1 day from now

  console.log('1. Message building\n');
  const msg = buildMessage('treasury', 0, expiry, 'propose', intent, params);
  console.log(`   ${msg}\n`);

  // ── 2. Signing ─────────────────────────────────────────────────────

  console.log('2. Signing\n');
  const sig = signMessage(aliceKey, msg);
  console.log(`   Signature: ${sig.slice(0, 32)}...`);
  console.log(`   Public key: ${aliceKey.getPublicKey().toString()}\n`);

  // ── 3. Amendment ───────────────────────────────────────────────────

  console.log('3. Proposal amendment\n');
  const amendedParams = { amount: nearToYocto('2.0'), recipient: 'bob.testnet' };
  const amendMsg = buildMessage('treasury', 0, expiry, 'amend', intent, amendedParams);
  console.log(`   ${amendMsg}\n`);
  console.log('   (Resets all approvals)\n');

  // ── 4. Delegation ──────────────────────────────────────────────────

  console.log('4. Delegation\n');
  console.log('   // Approver #0 delegates to bob.testnet');
  console.log('   await client.delegateApprover(account, "treasury", 3, 0, "bob.testnet");\n');
  console.log('   // Bob can now approve on behalf of approver #0\n');

  // ── 5. Ownership Transfer ──────────────────────────────────────────

  console.log('5. Ownership transfer\n');
  console.log('   // Current owner transfers to new account');
  console.log('   await client.transferOwnership(account, "treasury", "new-owner.testnet");\n');
  console.log('   // Meta-intents updated to include new owner\n');

  // ── 6. Wallet Deletion ─────────────────────────────────────────────

  console.log('6. Wallet lifecycle\n');
  console.log(`   Storage deposit: 0.5 NEAR (${STORAGE_DEPOSIT} yocto)`);
  console.log('   Delete wallet → refund storage deposit');
  console.log('   await client.deleteWallet(account, "old-treasury");\n');

  // ── 7. U128 Safety ─────────────────────────────────────────────────

  console.log('7. U128 precision\n');
  console.log(`   1 NEAR     = ${nearToYocto('1')} yocto`);
  console.log(`   1.5 NEAR   = ${nearToYocto('1.5')} yocto`);
  console.log(`   0.001 NEAR = ${nearToYocto('0.001')} yocto\n`);

  console.log('✅ Demo complete!');
}

main().catch(console.error);

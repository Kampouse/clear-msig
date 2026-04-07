/**
 * clear-msig Reference Client
 *
 * TypeScript client for interacting with the clear-msig multisig contract.
 * Handles message building, signing, and contract calls.
 *
 * Usage:
 *   import { ClearMsig } from './index';
 *   const client = new ClearMsig('clear-msig.kampouse.testnet', 'testnet');
 *   await client.createWallet('treasury');
 *   const proposal = await client.propose('treasury', 3, params, keyPair);
 */

import { KeyPair, utils } from 'near-api-js';

// ── Types ──────────────────────────────────────────────────────────────────

export type ParamType = 'AccountId' | 'U64' | 'U128' | 'String' | 'Bool';

export interface ParamDef {
  name: string;
  param_type: ParamType;
  max_value: string | null;
}

export type IntentType = 'Custom' | 'AddIntent' | 'RemoveIntent' | 'UpdateIntent';
export type ProposalStatus = 'Active' | 'Approved' | 'Executed' | 'Cancelled';

export interface Intent {
  wallet_name: string;
  index: number;
  intent_type: IntentType;
  name: string;
  template: string;
  proposers: string[];
  approvers: string[];
  approval_threshold: number;
  cancellation_threshold: number;
  timelock_seconds: number;
  params: ParamDef[];
  active: boolean;
  active_proposal_count: number;
}

export interface Proposal {
  id: number;
  wallet_name: string;
  intent_index: number;
  proposer: string;
  status: ProposalStatus;
  proposed_at: number;
  approved_at: number;
  expires_at: number;
  approval_bitmap: number;
  cancellation_bitmap: number;
  param_values: string;
  message: string;
}

export interface Wallet {
  name: string;
  owner: string;
  proposal_index: number;
  intent_index: number;
  created_at: number;
}

export interface ProposeParams {
  [key: string]: string | number | boolean;
}

// ── Message Builder ────────────────────────────────────────────────────────

/**
 * Build a human-readable clear-sign message.
 * This MUST match the contract's `build_message` exactly.
 */
export function buildMessage(
  walletName: string,
  proposalIndex: number,
  expiresAt: bigint, // nanoseconds
  action: 'propose' | 'approve',
  intent: Intent,
  params: ProposeParams,
): string {
  const content = buildContent(intent, params);
  const expiresSecs = expiresAt / BigInt(1_000_000_000);
  const expiresNanos = expiresAt % BigInt(1_000_000_000);
  const expiresDisplay = `${expiresSecs}.${expiresNanos.toString().padStart(9, '0')}`;

  return `expires ${expiresDisplay}: ${action} ${content} | wallet: ${walletName} proposal: ${proposalIndex}`;
}

function buildContent(intent: Intent, params: ProposeParams): string {
  switch (intent.intent_type) {
    case 'AddIntent': {
      const hash = (params['hash'] as string) ?? 'unknown';
      return `add intent definition_hash: ${hash}`;
    }
    case 'RemoveIntent': {
      const idx = Number(params['index'] ?? 0);
      return `remove intent ${idx}`;
    }
    case 'UpdateIntent': {
      const idx = Number(params['index'] ?? 0);
      return `update intent ${idx}`;
    }
    case 'Custom':
      return renderTemplate(intent.template, intent.params, params);
  }
}

/**
 * Render a template by substituting {param} placeholders with values.
 * U128 values are always rendered as full decimal strings.
 */
export function renderTemplate(
  template: string,
  paramDefs: ParamDef[],
  params: ProposeParams,
): string {
  let result = template;
  for (const pd of paramDefs) {
    const placeholder = `{${pd.name}}`;
    const raw = params[pd.name];
    if (raw === undefined) continue;

    const value = renderParam(pd.param_type, raw);
    result = result.replace(placeholder, value);
  }
  return result;
}

function renderParam(type: ParamType, value: string | number | boolean): string {
  switch (type) {
    case 'AccountId':
      return String(value);
    case 'U64':
      return String(value);
    case 'U128':
      // Always full decimal string — no scientific notation
      return value instanceof bigint ? value.toString() : String(value);
    case 'String':
      return String(value);
    case 'Bool':
      return String(value);
  }
}

// ── Signing ────────────────────────────────────────────────────────────────

/**
 * Sign a message with an ed25519 key pair.
 * Returns the signature as a hex string.
 */
export function signMessage(keyPair: KeyPair, message: string): string {
  const msgBytes = new TextEncoder().encode(message);
  const { signature } = keyPair.sign(msgBytes);
  return Buffer.from(signature).toString('hex');
}

/**
 * Get the hex representation of a public key (without ed25519: prefix).
 */
export function publicKeyToHex(keyPair: KeyPair): string {
  const pk = keyPair.getPublicKey();
  return Buffer.from(pk.data).toString('hex');
}

// ── U128 Helper ────────────────────────────────────────────────────────────

/**
 * Safe U128 representation.
 * JavaScript Number loses precision above 2^53.
 * Always use BigInt or string for U128 values.
 *
 * @example
 *   const amount = u128("1000000000000000000000000"); // 1 NEAR in yocto
 *   const max = u128(10_000_000) * u128("1000000000000000000000000"); // 10M NEAR
 */
export function u128(value: string | number | bigint): string {
  return BigInt(value).toString();
}

/**
 * NEAR to yoctoNEAR conversion.
 * @param near - NEAR amount (supports decimal)
 * @returns yoctoNEAR as string
 */
export function nearToYocto(near: string): string {
  const [intPart, decPart = ''] = near.split('.');
  const padded = decPart.padEnd(24, '0').slice(0, 24);
  return BigInt(intPart + padded).toString();
}

/**
 * YoctoNEAR to NEAR conversion.
 * @param yocto - yoctoNEAR amount as string
 * @returns NEAR amount as string
 */
export function yoctoToNear(yocto: string): string {
  const y = BigInt(yocto);
  const whole = y / BigInt('1000000000000000000000000');
  const frac = y % BigInt('1000000000000000000000000');
  if (frac === 0n) return whole.toString();
  const fracStr = frac.toString().padStart(24, '0').replace(/0+$/, '');
  return `${whole}.${fracStr}`;
}

// ── Contract Client ────────────────────────────────────────────────────────

export class ClearMsig {
  constructor(
    public readonly contractId: string,
    public readonly networkId: 'testnet' | 'mainnet',
  ) {}

  // ── View Methods ──────────────────────────────────────────────────────

  async getWallet(account: any, name: string): Promise<Wallet | null> {
    return account.viewFunction(this.contractId, 'get_wallet', { name });
  }

  async getIntent(account: any, walletName: string, index: number): Promise<Intent | null> {
    return account.viewFunction(this.contractId, 'get_intent', {
      wallet_name: walletName,
      index,
    });
  }

  async listIntents(account: any, walletName: string): Promise<Intent[]> {
    return account.viewFunction(this.contractId, 'list_intents', {
      wallet_name: walletName,
    });
  }

  async getProposal(account: any, walletName: string, id: number): Promise<Proposal | null> {
    return account.viewFunction(this.contractId, 'get_proposal', {
      wallet_name: walletName,
      id,
    });
  }

  async listProposals(account: any, walletName: string): Promise<Proposal[]> {
    return account.viewFunction(this.contractId, 'list_proposals', {
      wallet_name: walletName,
    });
  }

  async getProposalMessage(account: any, walletName: string, id: number): Promise<string | null> {
    return account.viewFunction(this.contractId, 'get_proposal_message', {
      wallet_name: walletName,
      id,
    });
  }

  // ── Write Methods ─────────────────────────────────────────────────────

  async createWallet(account: any, name: string): Promise<void> {
    await account.functionCall({
      contractId: this.contractId,
      methodName: 'create_wallet',
      args: { name },
    });
  }

  async addIntent(account: any, walletName: string, intent: Omit<Intent, 'wallet_name' | 'index' | 'active' | 'active_proposal_count'>): Promise<void> {
    await account.functionCall({
      contractId: this.contractId,
      methodName: 'add_intent',
      args: {
        wallet_name: walletName,
        intent: {
          ...intent,
          wallet_name: walletName,
          index: 0, // contract assigns the real index
          active: true,
          active_proposal_count: 0,
        },
      },
    });
  }

  /**
   * Create a proposal with clear-signing.
   *
   * @example
   * ```ts
   * const result = await client.propose('treasury', 3, {
   *   amount: '1000000000000000000000000',
   *   recipient: 'bob.testnet',
   * }, keyPair, account, {
   *   expiresAtNs: BigInt(Date.now() + 86400000) * BigInt(1_000_000),
   * });
   * console.log('Proposal:', result.proposalId);
   * console.log('Message:', result.message);
   * ```
   */
  async propose(
    walletName: string,
    intentIndex: number,
    params: ProposeParams,
    keyPair: KeyPair,
    account: any,
    options: { expiresAtNs: bigint },
  ): Promise<{ proposalId: number; message: string }> {
    // Fetch intent to build the message
    const intent = await this.getIntent(account, walletName, intentIndex);
    if (!intent) throw new Error(`Intent #${intentIndex} not found`);

    const wallet = await this.getWallet(account, walletName);
    if (!wallet) throw new Error(`Wallet '${walletName}' not found`);

    const proposalIndex = wallet.proposal_index;

    // Build the message (must match contract exactly)
    const message = buildMessage(
      walletName,
      proposalIndex,
      options.expiresAtNs,
      'propose',
      intent,
      params,
    );

    // Sign
    const signature = signMessage(keyPair, message);
    const proposerPubkey = publicKeyToHex(keyPair);

    // Build param_values — ensure U128 values are strings
    const paramValues: Record<string, any> = {};
    for (const pd of intent.params) {
      const v = params[pd.name];
      if (v !== undefined) {
        paramValues[pd.name] = pd.param_type === 'U128' ? String(v) : v;
      }
    }

    await account.functionCall({
      contractId: this.contractId,
      methodName: 'propose',
      args: {
        wallet_name: walletName,
        intent_index: intentIndex,
        param_values: JSON.stringify(paramValues),
        expires_at: options.expiresAtNs.toString(),
        proposer_pubkey: proposerPubkey,
        signature,
      },
      gas: new utils.format.Gas('100').intoGas(), // 100 Tgas
    });

    return { proposalId: proposalIndex, message };
  }

  /**
   * Approve a proposal with clear-signing.
   *
   * @example
   * ```ts
   * const result = await client.approve('treasury', 0, 0, keyPair, account, {
   *   expiresAtNs: BigInt(Date.now() + 86400000) * BigInt(1_000_000),
   * });
   * ```
   */
  async approve(
    walletName: string,
    proposalId: number,
    approverIndex: number,
    keyPair: KeyPair,
    account: any,
    options: { expiresAtNs: bigint },
  ): Promise<{ message: string }> {
    // Fetch proposal and intent
    const proposal = await this.getProposal(account, walletName, proposalId);
    if (!proposal) throw new Error(`Proposal #${proposalId} not found`);

    const intent = await this.getIntent(account, walletName, proposal.intent_index);
    if (!intent) throw new Error('Intent not found');

    const params: ProposeParams = JSON.parse(proposal.param_values);

    // Build approve message
    const message = buildMessage(
      walletName,
      proposalId,
      options.expiresAtNs,
      'approve',
      intent,
      params,
    );

    // Sign
    const signature = signMessage(keyPair, message);

    await account.functionCall({
      contractId: this.contractId,
      methodName: 'approve',
      args: {
        wallet_name: walletName,
        proposal_id: proposalId,
        approver_index: approverIndex,
        signature,
        expires_at: options.expiresAtNs.toString(),
      },
      gas: new utils.format.Gas('100').intoGas(),
    });

    return { message };
  }

  /**
   * Cancel-vote a proposal.
   */
  async cancelVote(
    account: any,
    walletName: string,
    proposalId: number,
    approverIndex: number,
  ): Promise<void> {
    await account.functionCall({
      contractId: this.contractId,
      methodName: 'cancel_vote',
      args: {
        wallet_name: walletName,
        proposal_id: proposalId,
        approver_index: approverIndex,
      },
    });
  }

  /**
   * Execute an approved proposal.
   */
  async execute(
    account: any,
    walletName: string,
    proposalId: number,
  ): Promise<void> {
    await account.functionCall({
      contractId: this.contractId,
      methodName: 'execute',
      args: {
        wallet_name: walletName,
        proposal_id: proposalId,
      },
      gas: new utils.format.Gas('100').intoGas(),
    });
  }

  /**
   * Clean up an executed or cancelled proposal.
   */
  async cleanup(
    account: any,
    walletName: string,
    proposalId: number,
  ): Promise<void> {
    await account.functionCall({
      contractId: this.contractId,
      methodName: 'cleanup',
      args: {
        wallet_name: walletName,
        proposal_id: proposalId,
      },
    });
  }
}

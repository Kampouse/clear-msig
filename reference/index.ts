/**
 * clear-msig Reference Client
 *
 * TypeScript client for the clear-msig multisig contract on NEAR Protocol.
 * Handles message building, signing, delegation, amendments, and all contract calls.
 *
 * @example
 * ```ts
 * import { ClearMsig, nearToYocto } from './index';
 * const client = new ClearMsig('clear-msig.kampouse.testnet', 'testnet');
 * await client.createWallet(account, 'treasury', NearToken.from_yoctonear('500000000000000000000000'));
 * ```
 */

import { KeyPair, utils, ConnectedWalletAccount } from 'near-api-js';

// ── Types ──────────────────────────────────────────────────────────────────

export type ParamType = 'AccountId' | 'U64' | 'U128' | 'String' | 'Bool';
export type IntentType = 'Custom' | 'AddIntent' | 'RemoveIntent' | 'UpdateIntent';
export type ProposalStatus = 'Active' | 'Approved' | 'Executed' | 'Cancelled';
export type MessageAction = 'propose' | 'approve' | 'cancel' | 'amend';

export interface ParamDef {
  name: string;
  param_type: ParamType;
  max_value: string | null;
}

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
  execution_gas_tgas: number;
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
  intent_params_hash: string;
}

export interface Wallet {
  name: string;
  owner: string;
  proposal_index: number;
  intent_index: number;
  created_at: number;
  storage_deposit: string;
}

export interface ProposeParams {
  [key: string]: string | number | boolean;
}

// ── Constants ──────────────────────────────────────────────────────────────

/** Storage deposit required to create a wallet (0.5 NEAR in yocto) */
export const STORAGE_DEPOSIT = '500000000000000000000000';
/** Default execution gas in teragas */
export const DEFAULT_EXECUTION_GAS_TGAS = 50;
/** Maximum execution gas in teragas */
export const MAX_EXECUTION_GAS_TGAS = 300;

// ── Message Builder ────────────────────────────────────────────────────────

/**
 * Build a human-readable clear-sign message.
 * This MUST match the contract's `build_message` exactly.
 */
export function buildMessage(
  walletName: string,
  proposalIndex: number,
  expiresAtNs: bigint,
  action: MessageAction,
  intent: Intent,
  params: ProposeParams,
): string {
  const content = buildContent(intent, params);
  const expiresSecs = expiresAtNs / BigInt(1_000_000_000);
  const expiresNanos = expiresAtNs % BigInt(1_000_000_000);
  const expiresDisplay = `${expiresSecs}.${expiresNanos.toString().padStart(9, '0')}`;
  return `expires ${expiresDisplay}: ${action} ${content} | wallet: ${walletName} proposal: ${proposalIndex}`;
}

function buildContent(intent: Intent, params: ProposeParams): string {
  switch (intent.intent_type) {
    case 'AddIntent':
      return `add intent definition_hash: ${params['hash'] ?? 'unknown'}`;
    case 'RemoveIntent':
      return `remove intent ${Number(params['index'] ?? 0)}`;
    case 'UpdateIntent':
      return `update intent ${Number(params['index'] ?? 0)}`;
    case 'Custom':
      return renderTemplate(intent.template, intent.params, params);
  }
}

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
    result = result.replace(placeholder, renderParam(pd.param_type, raw));
  }
  return result;
}

function renderParam(type: ParamType, value: string | number | boolean): string {
  switch (type) {
    case 'AccountId': return String(value);
    case 'U64': return String(value);
    case 'U128': return value instanceof bigint ? value.toString() : String(value);
    case 'String': return String(value);
    case 'Bool': return String(value);
  }
}

// ── Signing ────────────────────────────────────────────────────────────────

export function signMessage(keyPair: KeyPair, message: string): string {
  const msgBytes = new TextEncoder().encode(message);
  const { signature } = keyPair.sign(msgBytes);
  return Buffer.from(signature).toString('hex');
}

export function publicKeyToHex(keyPair: KeyPair): string {
  const pk = keyPair.getPublicKey();
  return Buffer.from(pk.data).toString('hex');
}

// ── U128 Helpers ───────────────────────────────────────────────────────────

export function u128(value: string | number | bigint): string {
  return BigInt(value).toString();
}

export function nearToYocto(near: string): string {
  const [intPart, decPart = ''] = near.split('.');
  const padded = decPart.padEnd(24, '0').slice(0, 24);
  return BigInt(intPart + padded).toString();
}

export function yoctoToNear(yocto: string): string {
  const y = BigInt(yocto);
  const whole = y / BigInt('1000000000000000000000000');
  const frac = y % BigInt('1000000000000000000000000');
  if (frac === 0n) return whole.toString();
  const fracStr = frac.toString().padStart(24, '0').replace(/0+$/, '');
  return `${whole}.${fracStr}`;
}

/** Generate an expiry nanoseconds `secondsFromNow` seconds in the future */
export function expiryFromNow(secondsFromNow: number): bigint {
  return BigInt(Math.floor(Date.now() / 1000 + secondsFromNow)) * BigInt(1_000_000_000);
}

// ── Contract Client ────────────────────────────────────────────────────────

export class ClearMsig {
  constructor(
    public readonly contractId: string,
    public readonly networkId: 'testnet' | 'mainnet',
  ) {}

  // ── Views ──────────────────────────────────────────────────────────

  async getWallet(account: any, name: string): Promise<Wallet | null> {
    return account.viewFunction(this.contractId, 'get_wallet', { name });
  }

  async getIntent(account: any, walletName: string, index: number): Promise<Intent | null> {
    return account.viewFunction(this.contractId, 'get_intent', {
      wallet_name: walletName, index,
    });
  }

  async listIntents(account: any, walletName: string): Promise<Intent[]> {
    return account.viewFunction(this.contractId, 'list_intents', {
      wallet_name: walletName,
    });
  }

  async getProposal(account: any, walletName: string, id: number): Promise<Proposal | null> {
    return account.viewFunction(this.contractId, 'get_proposal', {
      wallet_name: walletName, id,
    });
  }

  async listProposals(account: any, walletName: string): Promise<Proposal[]> {
    return account.viewFunction(this.contractId, 'list_proposals', {
      wallet_name: walletName,
    });
  }

  async getProposalMessage(account: any, walletName: string, id: number): Promise<string | null> {
    return account.viewFunction(this.contractId, 'get_proposal_message', {
      wallet_name: walletName, id,
    });
  }

  async getDelegation(
    account: any,
    walletName: string,
    intentIndex: number,
    approverIndex: number,
  ): Promise<string | null> {
    return account.viewFunction(this.contractId, 'get_delegation', {
      wallet_name: walletName,
      intent_index: intentIndex,
      approver_index: approverIndex,
    });
  }

  async getEventNonce(account: any): Promise<number> {
    return account.viewFunction(this.contractId, 'get_event_nonce');
  }

  // ── Wallet Management ─────────────────────────────────────────────

  async createWallet(account: any, name: string): Promise<void> {
    await account.functionCall({
      contractId: this.contractId,
      methodName: 'create_wallet',
      args: { name },
      attachedDeposit: utils.format.parseNearToken('0.5'),
    });
  }

  async deleteWallet(account: any, name: string): Promise<void> {
    await account.functionCall({
      contractId: this.contractId,
      methodName: 'delete_wallet',
      args: { name },
    });
  }

  async transferOwnership(account: any, walletName: string, newOwner: string): Promise<void> {
    await account.functionCall({
      contractId: this.contractId,
      methodName: 'transfer_ownership',
      args: { wallet_name: walletName, new_owner: newOwner },
    });
  }

  // ── Intent Management ─────────────────────────────────────────────

  async addIntent(
    account: any,
    walletName: string,
    intent: {
      intent_type: IntentType;
      name: string;
      template: string;
      proposers: string[];
      approvers: string[];
      approval_threshold: number;
      cancellation_threshold: number;
      timelock_seconds: number;
      params: ParamDef[];
      execution_gas_tgas?: number;
    },
  ): Promise<void> {
    await account.functionCall({
      contractId: this.contractId,
      methodName: 'add_intent',
      args: {
        wallet_name: walletName,
        intent: {
          ...intent,
          execution_gas_tgas: intent.execution_gas_tgas ?? DEFAULT_EXECUTION_GAS_TGAS,
          wallet_name: walletName,
          index: 0,
          active: true,
          active_proposal_count: 0,
        },
      },
    });
  }

  // ── Delegation ────────────────────────────────────────────────────

  async delegateApprover(
    account: any,
    walletName: string,
    intentIndex: number,
    approverIndex: number,
    delegate: string,
  ): Promise<void> {
    await account.functionCall({
      contractId: this.contractId,
      methodName: 'delegate_approver',
      args: {
        wallet_name: walletName,
        intent_index: intentIndex,
        approver_index: approverIndex,
        delegate,
      },
    });
  }

  // ── Proposals ─────────────────────────────────────────────────────

  async propose(
    walletName: string,
    intentIndex: number,
    params: ProposeParams,
    keyPair: KeyPair,
    account: any,
    options: { expiresAtNs: bigint },
  ): Promise<{ proposalId: number; message: string }> {
    const intent = await this.getIntent(account, walletName, intentIndex);
    if (!intent) throw new Error(`Intent #${intentIndex} not found`);

    const wallet = await this.getWallet(account, walletName);
    if (!wallet) throw new Error(`Wallet '${walletName}' not found`);

    const proposalIndex = wallet.proposal_index;
    const message = buildMessage(walletName, proposalIndex, options.expiresAtNs, 'propose', intent, params);
    const signature = signMessage(keyPair, message);
    const proposerPubkey = publicKeyToHex(keyPair);

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
      gas: new utils.format.Gas('100').intoGas(),
    });

    return { proposalId: proposalIndex, message };
  }

  async amendProposal(
    walletName: string,
    proposalId: number,
    newParams: ProposeParams,
    keyPair: KeyPair,
    account: any,
    options: { expiresAtNs: bigint },
  ): Promise<{ message: string }> {
    const proposal = await this.getProposal(account, walletName, proposalId);
    if (!proposal) throw new Error(`Proposal #${proposalId} not found`);

    const intent = await this.getIntent(account, walletName, proposal.intent_index);
    if (!intent) throw new Error('Intent not found');

    const message = buildMessage(walletName, proposalId, options.expiresAtNs, 'amend', intent, newParams);
    const signature = signMessage(keyPair, message);
    const proposerPubkey = publicKeyToHex(keyPair);

    const paramValues: Record<string, any> = {};
    for (const pd of intent.params) {
      const v = newParams[pd.name];
      if (v !== undefined) {
        paramValues[pd.name] = pd.param_type === 'U128' ? String(v) : v;
      }
    }

    await account.functionCall({
      contractId: this.contractId,
      methodName: 'amend_proposal',
      args: {
        wallet_name: walletName,
        proposal_id: proposalId,
        param_values: JSON.stringify(paramValues),
        expires_at: options.expiresAtNs.toString(),
        proposer_pubkey: proposerPubkey,
        signature,
      },
      gas: new utils.format.Gas('100').intoGas(),
    });

    return { message };
  }

  async approve(
    walletName: string,
    proposalId: number,
    approverIndex: number,
    keyPair: KeyPair,
    account: any,
    options: { expiresAtNs: bigint },
  ): Promise<{ message: string }> {
    const proposal = await this.getProposal(account, walletName, proposalId);
    if (!proposal) throw new Error(`Proposal #${proposalId} not found`);

    const intent = await this.getIntent(account, walletName, proposal.intent_index);
    if (!intent) throw new Error('Intent not found');

    const params: ProposeParams = JSON.parse(proposal.param_values);
    const message = buildMessage(walletName, proposalId, options.expiresAtNs, 'approve', intent, params);
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

  async cancelVote(
    walletName: string,
    proposalId: number,
    approverIndex: number,
    keyPair: KeyPair,
    account: any,
    options: { expiresAtNs: bigint },
  ): Promise<{ message: string }> {
    const proposal = await this.getProposal(account, walletName, proposalId);
    if (!proposal) throw new Error(`Proposal #${proposalId} not found`);

    const intent = await this.getIntent(account, walletName, proposal.intent_index);
    if (!intent) throw new Error('Intent not found');

    const params: ProposeParams = JSON.parse(proposal.param_values);
    const message = buildMessage(walletName, proposalId, options.expiresAtNs, 'cancel', intent, params);
    const signature = signMessage(keyPair, message);

    await account.functionCall({
      contractId: this.contractId,
      methodName: 'cancel_vote',
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

  async execute(account: any, walletName: string, proposalId: number): Promise<void> {
    await account.functionCall({
      contractId: this.contractId,
      methodName: 'execute',
      args: { wallet_name: walletName, proposal_id: proposalId },
      gas: new utils.format.Gas('100').intoGas(),
    });
  }

  async cleanup(account: any, walletName: string, proposalId: number): Promise<void> {
    await account.functionCall({
      contractId: this.contractId,
      methodName: 'cleanup',
      args: { wallet_name: walletName, proposal_id: proposalId },
    });
  }
}

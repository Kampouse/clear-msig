# clear-msig

**Clear-signing multisig for NEAR Protocol.**

> **Note:** This is a NEAR Protocol port of [ChewingGlass/clear-msig](https://github.com/ChewingGlass/clear-msig), the original clear-sign multisig built for Solana using [Quasar](https://github.com/blueshift-gg/quasar).

Signers see exactly what they're approving — human-readable messages, not opaque transaction bytes.

## How It Works

Traditional multisigs have signers approve hashes of serialized transactions. **clear-msig** fixes this:

1. **Intents** define what operations a wallet can perform (transfer NEAR, transfer FTs, deposit, custom actions)
2. **Proposals** fill in the parameters and generate a human-readable message
3. **Signers** read the message, then sign it with their ed25519 key
4. **Execution** happens only when enough approvals are collected (after timelock)

### Message Format

```
expires <timestamp>: <action> <content> | wallet: <name> proposal: <index>
```

Example:
```
expires 1893456000.000000000: propose transfer 1000000000000000000000000 yoctoNEAR to bob.near | wallet: treasury proposal: 0
```

No ambiguity. Signers know exactly what they're approving.

## Features

| Feature | Description |
|---------|-------------|
| Clear-signing | Human-readable messages signed with ed25519 |
| Intent-based governance | Define allowed operations, proposers, approvers, thresholds |
| Meta-intents | Self-governance: AddIntent, RemoveIntent, UpdateIntent |
| Proposal lifecycle | Propose → Amend → Approve → Execute (with timelock) |
| NEP-141 FT support | Receive, hold, and transfer fungible tokens |
| Token allowlists | Per-wallet FT allowlist to prevent griefing |
| Balance tracking | Internal accounting for NEAR and FT balances per wallet |
| Delegation | Approvers can delegate their vote to another account |
| Ownership transfer | Owner can transfer wallet ownership (meta-intents updated) |
| Proposal amendment | Proposer can amend active proposals (resets votes) |
| Wallet deletion | Owner can delete wallet, storage deposit refunded |
| Event nonces | Monotonic counter for strict event ordering |
| Configurable gas | Per-intent execution gas (default 50, max 300 Tgas) |
| Storage accounting | Tracks actual bytes, accurate refunds |
| Cross-contract protection | All signed methods reject contract-to-contract calls |
| Intent schema pinning | SHA-256 hash prevents post-proposal schema changes |

## Concepts

### Wallets

A wallet is a named container with:
- An **owner** (creator, can be transferred)
- A set of **intents** defining allowed operations
- A set of **proposals** (pending, approved, executed, cancelled)
- A **token allowlist** for FT reception
- Internal **balance tracking** for NEAR and FTs
- A **storage deposit** (0.5 NEAR required on creation)

Every wallet is created with 3 **meta-intents** for self-governance:
| Index | Intent | Purpose |
|-------|--------|---------|
| 0 | `AddIntent` | Add new operation types via proposal |
| 1 | `RemoveIntent` | Deactivate an intent |
| 2 | `UpdateIntent` | Modify intent parameters |

### Intents

All intent changes go through the meta-intent proposal flow. There is no owner bypass — the multisig is fully governed by its thresholds.

An intent defines an allowed operation:

```json
{
  "name": "Transfer NEAR",
  "template": "transfer {amount} yoctoNEAR to {recipient}",
  "proposers": ["alice.near", "bob.near"],
  "approvers": ["alice.near", "bob.near", "carol.near"],
  "approval_threshold": 2,
  "cancellation_threshold": 2,
  "timelock_seconds": 86400,
  "execution_gas_tgas": 50,
  "params": [
    { "name": "amount", "param_type": "U128", "max_value": "10000000000000000000000000" },
    { "name": "recipient", "param_type": "AccountId", "max_value": null }
  ]
}
```

### Proposals

A proposal is created when a proposer fills in parameters for an intent:

1. **Active** → awaiting approvals
2. **Approved** → threshold reached, awaiting execution (after timelock)
3. **Executed** → action performed
4. **Cancelled** → vetoed by cancellation threshold

Proposals can be **amended** by the original proposer (resets all votes, requires clear-signed message).

### Parameter Types

| Type | JSON Representation | Example |
|------|-------------------|---------|
| `AccountId` | String | `"bob.near"` |
| `U64` | Number or string | `1000` or `"1000"` |
| `U128` | **String** (avoids precision loss) | `"1000000000000000000000000"` |
| `String` | String | `"hello"` |
| `Bool` | Boolean | `true` |

> **Important**: Always pass `U128` values as strings. JavaScript `Number` loses precision above 2^53.

## Token & Balance Management

### NEAR Balances

The contract tracks NEAR per wallet internally:
- **Deposit NEAR**: Execute a "Deposit NEAR" intent with attached deposit
- **Transfer NEAR**: Debits from wallet's tracked balance, sends to recipient
- **View balance**: `get_wallet_near_balance(wallet_name)`

### FT (NEP-141) Support

The contract implements `ft_on_transfer` to receive tokens:
- Call `ft_transfer_call(contract_id, amount, wallet_name)` on the FT contract
- Tokens are credited to the named wallet's internal balance
- **Token allowlist**: Empty = accept all. Non-empty = only listed tokens accepted
- `add_allowed_token(wallet, token)` / `remove_allowed_token(wallet, token)` — owner only
- `get_ft_balance(wallet, token)` — view balance

Storage is charged per unique token tracked (100 bytes per token from the storage deposit).

## Contract API

### Deployed

- **Testnet**: `cmsig.kampouse.testnet`
- **Repo**: [github.com/Kampouse/clear-msig](https://github.com/Kampouse/clear-msig)

### Wallet Management

| Method | Payable | Description |
|--------|---------|-------------|
| `create_wallet(name)` | Yes (0.5 NEAR) | Create wallet with 3 meta-intents |
| `delete_wallet(name)` | No | Delete wallet, refund storage. No active proposals. |
| `transfer_ownership(wallet, new_owner)` | No | Transfer ownership, update meta-intents |

### Token Management

| Method | Description |
|--------|-------------|
| `add_allowed_token(wallet, token)` | Add FT to wallet's allowlist (owner only) |
| `remove_allowed_token(wallet, token)` | Remove FT from allowlist (owner only) |
| `ft_on_transfer(sender, amount, msg)` | NEP-141 receiver. `msg` = wallet name |

### Proposal Lifecycle

| Method | Signed | Description |
|--------|--------|-------------|
| `propose(wallet, intent, params, expires, pubkey, sig)` | Yes | Create proposal with clear-signed message |
| `amend_proposal(wallet, id, params, expires, pubkey, sig)` | Yes | Amend proposal (resets votes, proposer only) |
| `approve(wallet, id, approver_idx, sig, expires)` | Yes | Approve with clear-signed message |
| `cancel_vote(wallet, id, approver_idx, sig, expires)` | Yes | Cancel-vote with clear-signed message |
| `execute(wallet, id)` | Optional* | Execute approved proposal |
| `cleanup(wallet, id)` | No | Remove executed/cancelled proposal |

*`execute` is payable for "Deposit NEAR" intent; attaches NEAR which is credited to the wallet.

### Delegation

| Method | Description |
|--------|-------------|
| `delegate_approver(wallet, intent, idx, delegate)` | Delegate approver slot to another account. Pass own account to revoke. |

### Views

| Method | Returns |
|--------|---------|
| `get_wallet(name)` | Wallet info (owner, storage, allowed tokens) |
| `get_intent(wallet, index)` | Intent by index |
| `list_intents(wallet)` | All intents |
| `get_proposal(wallet, id)` | Proposal by ID |
| `list_proposals(wallet)` | All proposals |
| `get_proposal_message(wallet, id)` | The human-readable message |
| `get_wallet_near_balance(wallet)` | Tracked NEAR balance |
| `get_ft_balance(wallet, token)` | Tracked FT balance |
| `get_allowed_tokens(wallet)` | Token allowlist |
| `get_delegation(wallet, intent, idx)` | Delegate for approver slot |
| `get_event_nonce()` | Current event counter |

## Built-in Executions

| Intent Name | Parameters | Action |
|-------------|-----------|--------|
| `Transfer NEAR` | `amount` (U128), `recipient` (AccountId) | Sends yoctoNEAR from wallet balance |
| `Transfer FT` | `token` (AccountId), `amount` (U128), `recipient` (AccountId) | Calls `ft_transfer`, debits tracked balance |
| `Deposit NEAR` | `amount` (U128) | Credits attached deposit to wallet balance |
| Custom (any other name) | Any params | Emits `custom_execution` event |

## Events

All state changes emit NEP-297 compliant events with monotonic nonces:

```json
{
  "standard": "clear-msig",
  "version": "1.0.0",
  "event": "transfer_near",
  "nonce": 42,
  "data": {
    "wallet": "treasury",
    "recipient": "bob.near",
    "amount": "1000000000000000000000000"
  }
}
```

| Event | Trigger |
|-------|---------|
| `wallet_created` | Wallet created |
| `wallet_deleted` | Wallet deleted |
| `ownership_transferred` | Owner changed |
| `token_allowed` | FT added to allowlist |
| `intent_added_via_proposal` | Intent added via AddIntent proposal |
| `intent_removed` | Intent deactivated |
| `intent_updated` | Intent modified |
| `proposal_created` | Proposal created |
| `proposal_amended` | Proposal amended |
| `proposal_approved` | Approval threshold reached |
| `proposal_cancelled` | Cancellation threshold reached |
| `proposal_executed` | Proposal executed |
| `proposal_cleaned` | Proposal removed from storage |
| `transfer_near` | NEAR transferred |
| `transfer_ft` | FT transferred |
| `near_deposited` | NEAR deposited to wallet |
| `ft_received` | FT received by wallet |
| `delegation_set` | Approver delegated |
| `delegation_revoked` | Delegation removed |

## Message Building Reference

### Template Rendering

Placeholders `{param_name}` are replaced with parameter values:

| ParamType | Rendering |
|-----------|-----------|
| `AccountId` | As-is string |
| `U64` | Decimal string |
| `U128` | Full decimal string (no truncation) |
| `String` | As-is string |
| `Bool` | `"true"` or `"false"` |

### Actions

| Action | Context |
|--------|---------|
| `propose` | Creating a proposal |
| `approve` | Approving a proposal |
| `cancel` | Cancel-voting a proposal |
| `amend` | Amending a proposal |

## Threat Model

### Protected against

- Blind signing / opaque transactions
- Parameter tampering (signed into message)
- Cross-wallet / cross-proposal replay
- Expired signature reuse
- Unauthorized proposals (pubkey verified)
- Cross-contract call attacks (`assert_direct_call`)
- Template injection (`|`, newlines rejected)
- Intent schema drift (SHA-256 pinning)
- Proposal spam (100 per intent, 1 year max expiry)
- U128 precision loss (always strings)
- Balance overdraft (checked before transfer)
- Unauthorized cancellations (clear-signed)
- FT griefing (token allowlist + storage accounting)
- Owner bypass (no `add_intent`, all through governance)

### Trust assumptions

| Trust | Who |
|-------|-----|
| ed25519 is secure | Cryptography |
| NEAR runtime verifies signatures | NEAR Protocol |
| Approvers control their keys | Key management |
| Contract logic is correct | **Needs audit** |

## Building & Deploying

```bash
# Build
cd contract && cargo near build non-reproducible-wasm

# Deploy + init (testnet)
near contract deploy <contract-id> use-file target/near/clear_msig.wasm \
  without-init-call network-config testnet sign-with-keychain send
near call <contract-id> new --accountId <account-id> --networkId testnet
```

## Reference Implementation

TypeScript client in `reference/index.ts`.

```typescript
import { ClearMsig, nearToYocto, expiryFromNow } from './reference';

const client = new ClearMsig('cmsig.kampouse.testnet', 'testnet');

// Create wallet (0.5 NEAR deposit)
await client.createWallet(account, 'treasury');

// Propose transfer
const { proposalId, message } = await client.propose(
  'treasury', 3,
  { amount: nearToYocto('1.5'), recipient: 'bob.testnet' },
  keyPair, account,
  { expiresAtNs: expiryFromNow(86400) },
);

// Approve
await client.approve('treasury', proposalId, 0, bobKeyPair, account, {
  expiresAtNs: expiryFromNow(86400),
});

// Execute
await client.execute(account, 'treasury', proposalId);
```

### Example

```bash
npx ts-node examples/full-flow.ts
```

## Project Structure

```
clear-msig/
├── contract/
│   └── src/
│       ├── lib.rs       # Contract state, wallet/intent/proposal CRUD
│       ├── execute.rs   # Proposal execution (NEAR, FT, deposit, custom)
│       ├── ft.rs        # NEP-141 receiver, balance tracking, allowlist
│       └── message.rs   # Clear-signing message builder & ed25519 verification
├── reference/
│   └── index.ts         # TypeScript client (reference implementation)
├── examples/
│   └── full-flow.ts     # Full flow demo
└── README.md
```

## License

MIT

# clear-msig

**Clear-signing multisig for NEAR Protocol.**

Signers see exactly what they're approving — human-readable messages, not opaque transaction bytes.

## How It Works

Traditional multisigs have signers approve hashes of serialized transactions. You can't tell what you're signing without specialized tooling. **clear-msig** fixes this:

1. **Intents** define what operations a wallet can perform (transfer NEAR, transfer FTs, custom actions)
2. **Proposals** fill in the parameters and generate a human-readable message
3. **Signers** read the message, then sign it with their ed25519 key
4. **Execution** happens only when enough approvals are collected

### Message Format

Every message follows this format:

```
expires <timestamp>: <action> <content> | wallet: <name> proposal: <index>
```

Example:
```
expires 1893456000.000000000: propose transfer 1000000000000000000000000 yoctoNEAR to bob.near | wallet: treasury proposal: 0
```

No ambiguity. Signers know exactly what they're approving.

## Concepts

### Wallets

A wallet is a named container with:
- An **owner** (creator)
- A set of **intents** defining allowed operations
- A set of **proposals** (pending, approved, executed, cancelled)

Every wallet is created with 3 **meta-intents** for self-governance:
| Index | Intent | Purpose |
|-------|--------|---------|
| 0 | `AddIntent` | Add new operation types |
| 1 | `RemoveIntent` | Deactivate an intent |
| 2 | `UpdateIntent` | Modify intent parameters |

### Intents

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
  "params": [
    { "name": "amount", "param_type": "U128", "max_value": "10000000000000000000000000" },
    { "name": "recipient", "param_type": "AccountId", "max_value": null }
  ]
}
```

- **proposers**: who can create proposals
- **approvers**: who can approve/cancel proposals
- **approval_threshold**: number of approvals needed
- **cancellation_threshold**: number of cancellations to veto
- **timelock_seconds**: delay between approval and execution
- **params**: typed parameters with optional max values
- **template**: human-readable template with `{param}` placeholders

### Proposals

A proposal is created when a proposer fills in parameters for an intent:

1. **Active** → awaiting approvals
2. **Approved** → threshold reached, awaiting execution (after timelock)
3. **Executed** → action performed
4. **Cancelled** → vetoed by cancellation threshold

### Parameter Types

| Type | JSON Representation | Example |
|------|-------------------|---------|
| `AccountId` | String | `"bob.near"` |
| `U64` | Number or string | `1000` or `"1000"` |
| `U128` | **String** (avoids precision loss) | `"1000000000000000000000000"` |
| `String` | String | `"hello"` |
| `Bool` | Boolean | `true` |

> **Important**: Always pass `U128` values as strings in JSON. JavaScript `Number` cannot represent values > 2^53 without precision loss. Use a `BigNumber` or `BigInt` library on the client side.

## Contract API

### Deployed

- **Testnet**: `clear-msig.kampouse.testnet`
- **Repo**: [github.com/Kampouse/clear-msig](https://github.com/Kampouse/clear-msig)

### Write Methods

#### `create_wallet(name: String)`
Create a new wallet. Caller becomes the owner.

```bash
near call clear-msig.kampouse.testnet create_wallet '{"name":"treasury"}' \
  --accountId alice.testnet --networkId testnet
```

#### `add_intent(wallet_name: String, intent: Intent)`
Add a custom intent (owner only).

```bash
near call clear-msig.kampouse.testnet add_intent '{
  "wallet_name": "treasury",
  "intent": {
    "index": 0,
    "intent_type": "Custom",
    "name": "Transfer NEAR",
    "template": "transfer {amount} yoctoNEAR to {recipient}",
    "proposers": ["alice.testnet"],
    "approvers": ["alice.testnet", "bob.testnet"],
    "approval_threshold": 2,
    "cancellation_threshold": 1,
    "timelock_seconds": 0,
    "params": [
      {"name": "amount", "param_type": "U128", "max_value": null},
      {"name": "recipient", "param_type": "AccountId", "max_value": null}
    ],
    "active": true,
    "active_proposal_count": 0,
    "wallet_name": "treasury"
  }
}' --accountId alice.testnet --networkId testnet
```

#### `propose(wallet_name, intent_index, param_values, expires_at, proposer_pubkey, signature)`
Create a proposal with a clear-signed message.

**Signing (client-side):**
```javascript
import { sign } from 'near-api-js/lib/utils/key_pair';

const message = `expires 1893456000.000000000: propose transfer 1000000000000000000000000 yoctoNEAR to bob.testnet | wallet: treasury proposal: 0`;
const signature = keyPair.sign(Buffer.from(message)).toString('hex');
```

**On-chain:**
```bash
near call clear-msig.kampouse.testnet propose '{
  "wallet_name": "treasury",
  "intent_index": 3,
  "param_values": "{\"amount\":\"1000000000000000000000000\",\"recipient\":\"bob.testnet\"}",
  "expires_at": "1893456000000000000",
  "proposer_pubkey": "<hex_public_key>",
  "signature": "<hex_signature>"
}' --accountId alice.testnet --networkId testnet --gas 100000000000000
```

#### `approve(wallet_name, proposal_id, approver_index, signature, expires_at)`
Approve a proposal with a clear-signed message.

**Signing:**
```javascript
const message = `expires 1893456000.000000000: approve transfer 1000000000000000000000000 yoctoNEAR to bob.testnet | wallet: treasury proposal: 0`;
const signature = keyPair.sign(Buffer.from(message)).toString('hex');
```

**On-chain:**
```bash
near call clear-msig.kampouse.testnet approve '{
  "wallet_name": "treasury",
  "proposal_id": 0,
  "approver_index": 0,
  "signature": "<hex_signature>",
  "expires_at": "1893456000000000000"
}' --accountId bob.testnet --networkId testnet --gas 100000000000000
```

#### `cancel_vote(wallet_name, proposal_id, approver_index)`
Cancel-vote a proposal. Clears any prior approval from this approver.

#### `execute(wallet_name, proposal_id)`
Execute an approved proposal (after timelock, if any).

```bash
near call clear-msig.kampouse.testnet execute '{"wallet_name":"treasury","proposal_id":0}' \
  --accountId alice.testnet --networkId testnet --gas 100000000000000
```

#### `cleanup(wallet_name, proposal_id)`
Remove an executed or cancelled proposal from storage.

### View Methods

| Method | Returns |
|--------|---------|
| `get_wallet(name)` | Wallet info |
| `get_intent(wallet_name, index)` | Intent by index |
| `list_intents(wallet_name)` | All intents |
| `get_proposal(wallet_name, id)` | Proposal by ID |
| `list_proposals(wallet_name)` | All proposals |
| `get_proposal_message(wallet_name, id)` | The human-readable message |

## Built-in Executions

| Intent Name | Parameters | Action |
|-------------|-----------|--------|
| `Transfer NEAR` | `amount` (U128), `recipient` (AccountId) | Sends yoctoNEAR |
| `Transfer FT` | `token` (AccountId), `amount` (U128 string), `recipient` (AccountId) | Calls `ft_transfer` |

Custom intent names that don't match emit an event:
```json
{
  "standard": "clear-msig",
  "version": "1.0.0",
  "event": "custom_execution",
  "data": { "wallet": "...", "intent": "...", "params": {} }
}
```

## Message Building Reference

Messages are built from the intent template with parameters substituted:

```
expires <unix_seconds>.<nanoseconds>: <action> <rendered_template> | wallet: <name> proposal: <id>
```

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

## Security Considerations

1. **U128 precision**: Always pass `U128` params as strings (`"1000000000000000000000000"`). JavaScript `Number` loses precision above 2^53.
2. **Message unambiguous**: Each message includes the wallet name and proposal ID, preventing replay across wallets or proposals.
3. **Expiry**: Proposals and signatures include expiry timestamps. Expired proposals can't be approved or executed.
4. **Vote switching**: Approving clears any prior cancellation vote for that approver, and vice versa.
5. **Timelock**: Optional delay between approval and execution gives time for cancellation.
6. **Max values**: Intent params can define `max_value` for U64/U128 types to limit proposals.

## Building & Deploying

```bash
# Build
cd contract && cargo near build non-reproducible-wasm

# Deploy (testnet)
near contract deploy <contract-id> use-file target/near/clear_msig.wasm \
  without-init-call network-config testnet sign-with-keychain send

# Initialize
near call <contract-id> new --accountId <account-id> --networkId testnet
```

## Reference Implementation

A TypeScript client library is included in `reference/index.ts`.

### Install

```bash
npm install near-api-js
```

### Quick Start

```typescript
import { ClearMsig, nearToYocto } from './reference';

const client = new ClearMsig('clear-msig.kampouse.testnet', 'testnet');

// Create wallet
await client.createWallet(account, 'treasury');

// Add a transfer intent
await client.addIntent(account, 'treasury', {
  intent_type: 'Custom',
  name: 'Transfer NEAR',
  template: 'transfer {amount} yoctoNEAR to {recipient}',
  proposers: ['alice.testnet'],
  approvers: ['alice.testnet', 'bob.testnet'],
  approval_threshold: 2,
  cancellation_threshold: 1,
  timelock_seconds: 0,
  params: [
    { name: 'amount', param_type: 'U128', max_value: null },
    { name: 'recipient', param_type: 'AccountId', max_value: null },
  ],
});

// Propose
const { proposalId, message } = await client.propose(
  'treasury', 3,
  { amount: nearToYocto('1.5'), recipient: 'bob.testnet' },
  keyPair, account,
  { expiresAtNs: BigInt(Date.now() + 86400000) * BigInt(1_000_000) },
);
console.log('Message to sign:', message);

// Approve
await client.approve('treasury', proposalId, 0, bobKeyPair, account, {
  expiresAtNs: BigInt(Date.now() + 86400000) * BigInt(1_000_000),
});

// Execute
await client.execute(account, 'treasury', proposalId);
```

### API

| Function | Description |
|----------|-------------|
| `buildMessage(wallet, index, expires, action, intent, params)` | Build the human-readable message |
| `signMessage(keyPair, message)` | Sign a message, returns hex signature |
| `publicKeyToHex(keyPair)` | Get hex public key (no prefix) |
| `renderTemplate(template, defs, params)` | Render intent template with params |
| `u128(value)` | Safe U128 string from number/string/BigInt |
| `nearToYocto(near)` | Convert NEAR to yoctoNEAR string |
| `yoctoToNear(yocto)` | Convert yoctoNEAR to NEAR string |

### Example

```bash
npx ts-node examples/full-flow.ts
```

Runs through message building, signing, approve flow, and U128 safety demos.

## Project Structure

```
clear-msig/
├── contract/
│   └── src/
│       ├── lib.rs       # Contract state, wallet/intent/proposal CRUD
│       ├── execute.rs   # Proposal execution (NEAR transfer, FT transfer, custom events)
│       └── message.rs   # Clear-signing message builder & ed25519 verification
├── reference/
│   └── index.ts         # TypeScript client library (reference implementation)
├── examples/
│   └── full-flow.ts     # Full flow demo
└── README.md
```

## License

MIT

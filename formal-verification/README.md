# Formal Verification

Mathematical proofs of clear-msig core invariants using [Verus](https://github.com/verus-lang/verus).

## Properties Proved

| ID | Property | Statement |
|----|----------|-----------|
| P1 | Bitmap mutual exclusion | `approval_bitmap & cancellation_bitmap == 0` holds after every operation |
| P2 | Threshold/count correspondence | `count_ones(approval_bitmap) >= threshold` implies Approved status |
| P3 | State transition validity | Only valid paths: Active→Approved→Executed, Active→Cancelled |
| P4 | Set/clear symmetry | `set_approval` clears cancel bit, `set_cancellation` clears approval bit |
| P5 | Reset correctness | `reset_votes()` always zeros both bitmaps |
| P6 | Balance conservation | `total_deposited - total_withdrawn == tracked_balance` always |

## Prerequisites

```bash
# Install Verus
cargo install verus
```

## Run Verification

```bash
cd formal-verification
verus src/main.rs
```

Expected output:
```
verification results::
  verified: 20
  errors:   0
```

## Architecture

The proofs extract the core logic from the NEAR contract into a standalone crate:

```
formal-verification/
├── Cargo.toml
├── README.md
└── src/
    └── main.rs    # Verus proof annotations + assertions
```

### Why separate crate?

Verus can't handle `near_sdk` types (AccountId, LookupMap, etc.) directly.
The standard approach is to extract the critical logic and prove it in isolation,
then argue (manually) that the contract code matches the verified model.

### Proof coverage

| Contract module | Lines verified | Method |
|----------------|---------------|--------|
| Bitmap ops | ~30 | Full formal proof |
| State transitions | ~20 | Full formal proof |
| Balance accounting | ~15 | Full formal proof |
| Message integrity | — | Covered by proptest (45 tests) |
| Template rendering | — | Covered by proptest (45 tests) |

## Extending the proofs

To add a new property:

1. Write the spec (what should be true):
```rust
spec fn my_invariant(state: &BitmapState) -> bool {
    // mathematical statement
}
```

2. Add proof annotations to the function:
```rust
fn my_operation(&mut self)
    requires self.my_invariant()
    ensures self.my_invariant()
{
    // implementation
}
```

3. Run `verus src/main.rs` — if it passes, the property is mathematically proven.

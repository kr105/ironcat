# Consensus Layer

## Overview

The consensus layer determines whether blocks and transactions are valid. It covers difficulty adjustment (6 algorithms spanning Catcoin's history), structural and contextual block validation, transaction checks, script execution, UTXO management, and proof-of-work verification.

Key modules:
- `difficulty` -- 6 CIP algorithms, `ChainLookup` trait, `ConsensusParams`
- `validation` -- block structure, transaction rules, subsidy calculation
- `script` -- stack-based VM, opcodes, ECDSA signature verification
- `chainstate` -- UTXO set, block connection/disconnection, undo data
- `pow` -- scrypt hash computation and target comparison
- `headers` -- header chain with batch validation and `BatchLookup` overlay

## Difficulty Algorithms

Catcoin has gone through 6 difficulty adjustment algorithms (CIPs), each activated at a specific block height:

| Height Range | Algorithm | Description |
|-------------|-----------|-------------|
| 0 -- 20288 | CIP01 | Original Bitcoin-style 2016-block retarget with 0.25x--4x clamping |
| 20289 | -- | Hardcoded difficulty 16 reset (nBits = `0x1c0ffff0`) |
| 20289 -- 21345 | CIP02 | 36-block retarget with 0.25x--4x clamping |
| 21346 -- 27259 | CIP03 | Every-block retarget with tight +/-12% bounds |
| 27260 -- 46330 | CIP04 | PID controller with 8-block lookback and dead zone |
| 46331 -- 396999 | CIP05 | Time-gated DigiShield with CIP04 fallback |
| 397000+ | CIP06 | LWMA-1 with 45-block weighted moving average |

All algorithms target 600-second block intervals.

### ChainLookup Trait

Difficulty algorithms need historical headers (timestamp and nBits) by height. The `ChainLookup` trait abstracts this so algorithms don't depend on any specific storage implementation. `HeaderStore` implements it for committed chain state.

### Batch Validation

When validating a batch of up to 2000 headers, header N may depend on headers 0..N-1 from the same batch that aren't committed to storage yet. `BatchLookup` overlays the pending validated headers on top of the store's `ChainLookup`, using O(1) indexed access since batch heights are sequential. If any header fails, the entire batch is rejected atomically.

Validation runs in two phases:
1. **Sequential:** Chain continuity (each `prev_hash` connects to the previous), difficulty target (nBits matches expected), checkpoint hash verification
2. **Parallel:** Scrypt proof-of-work via rayon for headers above the last checkpoint height. Headers at or below the last checkpoint skip PoW (their hashes are trusted)

### ConsensusParams

All activation heights, algorithm constants, and checkpoint data are captured in `ConsensusParams`, parameterized for future testnet support.

## Block Validation

### Structural Checks (`check_block`)

- Transaction count > 0
- First transaction must be coinbase; no other transaction may be coinbase
- Block weight <= 4,000,000

### Merkle Root

The merkle root is computed from double-SHA256 of transaction IDs. It must match `header.merkle_root`. Blocks with duplicate trailing transactions are rejected (CVE-2012-2459 merkle tree malleability protection).

### Contextual Checks (`check_block_context`)

- Block timestamp must strictly exceed the median-time-past (median of the previous 11 block timestamps)
- Block timestamp must not exceed `adjusted_time + 150 seconds`
- BIP34 (height in coinbase): the coinbase scriptSig must encode the block height using CScript serialization (OP_0 for height 0, OP_1--OP_16 for 1--16, then minimal-length LE push for larger heights)
- Coinbase output value must not exceed `subsidy + total_fees`

## Transaction Validation

### Context-Free Checks (`check_transaction`)

- At least one input and one output
- All output values non-negative and within MAX_MONEY
- Sum of outputs within MAX_MONEY
- No duplicate inputs (same outpoint)
- Coinbase scriptSig must be 2--100 bytes
- Non-coinbase transactions must not have null outpoints

### Coinbase Maturity

Coinbase outputs cannot be spent until 100 blocks after the block they appear in.

### Fees

Fee = sum of input values - sum of output values. Checked for arithmetic overflow.

## Subsidy

Block subsidy starts at 50 CAT and halves every 210,000 blocks. After 64 halvings the subsidy is zero.

| Constant | Value |
|----------|-------|
| COIN | 100,000,000 (base units per CAT) |
| Initial subsidy | 50 * COIN |
| Halving interval | 210,000 blocks |
| MAX_MONEY | 21,000,000 * COIN |

Note: the codebase uses raw `i64` values for amounts. "Catoshi" is a documentation convention for the base unit (1 CAT = 100M catoshis), analogous to satoshi in Bitcoin.

## Script Engine

Ironcat includes a stack-based script VM for transaction validation.

### Limits

| Parameter | Value |
|-----------|-------|
| Max script size | 10,000 bytes |
| Max operations | 201 (push ops excluded) |
| Max stack size | 1,000 elements (main + alt combined) |
| Max push size | 520 bytes |
| Max multisig keys | 20 |

### Execution Model

Script verification runs in two phases:

1. Execute `scriptSig` -- leaves data on the stack
2. Execute `scriptPubKey` -- consumes the stack; transaction is valid if the top element is truthy

### P2SH (Pay-to-Script-Hash)

If `scriptPubKey` matches the pattern `OP_HASH160 PUSH(20) <hash> OP_EQUAL` (exactly 23 bytes), P2SH rules apply:

1. `scriptSig` must be push-only (no opcodes other than data pushes)
2. The serialized redeem script (top stack element after executing `scriptSig`) is hashed and compared to `<hash>`
3. If it matches, the redeem script is deserialized and executed with the remaining stack

### Opcodes

45+ opcodes are implemented, including:

- **Stack:** OP_DUP, OP_DROP, OP_SWAP, OP_OVER, OP_ROT, OP_PICK, OP_ROLL, etc.
- **Arithmetic:** OP_ADD, OP_SUB, OP_NEGATE, OP_ABS, OP_MIN, OP_MAX, OP_WITHIN, etc. (using i64 script numbers)
- **Crypto:** OP_SHA256, OP_HASH160, OP_HASH256, OP_RIPEMD160, OP_SHA1
- **Signature:** OP_CHECKSIG, OP_CHECKSIGVERIFY, OP_CHECKMULTISIG, OP_CHECKMULTISIGVERIFY
- **Control flow:** OP_IF, OP_ELSE, OP_ENDIF, OP_VERIFY, OP_RETURN
- **Disabled:** Several opcodes (0x7e--0x81, 0x83--0x86, etc.) cause immediate script failure

### Signature Verification

- ECDSA via the `k256` crate (secp256k1)
- Lenient DER parsing that tolerates pre-BIP66 encoding quirks (extra leading zeros, wrong total length, trailing garbage)
- SigHash types: ALL, NONE, SINGLE, each with optional ANYONECANPAY flag
- Legacy sighash algorithm: hash type appended as u32 LE to the serialized transaction before double-SHA256

## Chainstate

The chainstate module manages the UTXO (unspent transaction output) set and handles block connection and disconnection.

### Block Connection (`connect_block`)

1. For each non-coinbase transaction input: look up the UTXO, check coinbase maturity, remove from the set, record in undo data
2. Verify scripts for all non-coinbase inputs in parallel via rayon
3. Calculate fees (input sum - output sum for each transaction)
4. Verify coinbase value <= subsidy + total fees
5. Insert all new outputs into the UTXO set
6. Store undo data to flat file, update undo index in chainstate.redb
7. Update chain tip

### Block Disconnection (`disconnect_block`)

Reverses a connected block using stored undo data: removes the block's outputs from the UTXO set and restores the inputs that were spent.

### Storage

UTXO set and undo index are stored in `chainstate.redb`. See [storage.md](storage.md) for details on the database schema, undo flat files, and data formats.

## Proof of Work

Catcoin uses scrypt for its proof-of-work hash, inherited from Litecoin.

### Parameters

| Parameter | Value |
|-----------|-------|
| N | 1024 (2^10) |
| r | 1 |
| p | 1 |
| dkLen | 32 bytes |
| Input | 80-byte block header |

### Implementation

The scrypt computation:

1. **PBKDF2-SHA256** (c=1): derives a 128-byte initial block from the header + password. Uses SHA256 midstate optimization -- the HMAC ipad/opad each fit in one SHA256 block, so the midstate is computed once and cloned for each PBKDF2 block
2. **SMix:** Phase 1 fills a 128KB scratchpad (1024 entries of 128 bytes) sequentially. Phase 2 does 1024 random lookups XORing entries (memory-hard step)
3. **BlockMix** with **Salsa20/8** (4 double-rounds): mixes 128-byte blocks within each SMix step
4. **PBKDF2-SHA256** (c=1): derives the final 32-byte hash

A thread-local `RefCell<Scratchpad>` reuses the 128KB allocation across calls, avoiding per-header allocation during initial block download.

`check_proof_of_work()` converts the header's nBits to a 256-bit target and compares the scrypt hash. Returns false if the target is invalid (negative, overflow, or zero).

# TODO

## Immediate

### Outgoing Connection Limits

No cap on simultaneous outgoing connections. Should limit to 8 full-relay + 2 block-relay-only connections (matching Bitcoin Core's defaults) to avoid resource exhaustion and reduce fingerprinting surface.

### Chain Reorganization

The chainstate module has `disconnect_block()` with undo data, but there is no reorg detection or orchestration. When a competing chain with more work arrives, ironcat needs to disconnect blocks back to the fork point and reconnect the new chain. Without this, any chain split will stall the node.

### JSON-RPC Interface

No programmatic access to node state. A JSON-RPC server would enable external tools, monitoring, wallet integration, and automation. Priority endpoints: `getblockcount`, `getblockhash`, `getblock`, `getmempoolinfo`, `getpeerinfo`, `getbestblockhash`.

## Next Phase

### Wallet

No wallet functionality. Requires key management (generation, import, encryption), address derivation, coin selection, transaction construction and signing, and balance tracking. Large subsystem that depends on a working chainstate and mempool.

### Transaction Index

Currently there is no way to look up a transaction by its txid without scanning blocks. A persistent index (txid -> block hash + position) would enable efficient transaction queries needed by the wallet and RPC interface.

### Fee Estimation

The mempool tracks fee rates but doesn't predict future confirmation times. A fee estimator would observe how long transactions at various fee rates take to confirm and provide target-based fee recommendations (e.g., "pay X catoshis/byte for confirmation within N blocks").

### Transaction Relay Policies

Basic acceptance checks exist but the mempool lacks full relay policies: dust threshold (outputs too small to be economically spendable), signature operation limits, witness size constraints, and standardness rules that prevent non-standard scripts from propagating.

## Later

### Compact Blocks (BIP 152)

Blocks are currently transferred in full. Compact blocks would transmit only short transaction IDs, reconstructing the block from the receiver's mempool. Reduces bandwidth significantly for nodes with overlapping mempools.

### Mining Support

No block template creation. Would require selecting transactions from the mempool by fee rate, constructing a coinbase transaction, and assembling a valid block template for external miners.

### Replace-by-Fee (BIP 125)

Transactions in the mempool cannot be replaced. RBF would allow a sender to bump the fee on an unconfirmed transaction by submitting a higher-fee replacement, useful when the network is congested.

### Assume-Valid

During initial sync, every block's scripts are verified from genesis. An assume-valid hash (a known-good block deep in the chain) would skip script verification for blocks at or below that height, dramatically speeding up initial sync while still validating all other consensus rules.

### Block Filters (BIP 157/158)

No support for serving compact block filters to light clients. Golomb-coded set filters would let lightweight wallets determine which blocks contain their transactions without downloading full blocks.

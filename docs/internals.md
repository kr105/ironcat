# Internals

## Shutdown

On exit (Ctrl+C or TUI quit), ironcat sends a clean TCP FIN to all connected peers before terminating. Writers are collected without holding DashMap locks, then shut down sequentially to avoid lock contention across await points.

## Connection Lifecycle

See [protocol.md](protocol.md) for the node state machine (Connecting, Handshaking, Connected, Disconnected, Dead, Banned) and retry/backoff details.

## Difficulty Validation

All incoming headers are validated against the expected difficulty target before being stored. The `difficulty` module implements 6 CIP algorithms, each activated at a specific block height. `ConsensusParams` captures all height thresholds and algorithm constants, parameterized for future testnet support.

### ChainLookup trait

Difficulty algorithms need to read historical headers (timestamp and nBits) by height. The `ChainLookup` trait abstracts this so the algorithms don't depend on `HeaderStore` directly. `HeaderStore` implements `ChainLookup` for committed state.

### Batch validation overlay

When validating a batch of 2000 headers, header N may depend on headers 0..N-1 from the same batch that aren't committed yet. `BatchLookup` overlays the pending validated headers on top of the store's `ChainLookup`, using O(1) indexed access since batch heights are sequential. If any header in the batch fails difficulty validation, the entire batch is rejected atomically.

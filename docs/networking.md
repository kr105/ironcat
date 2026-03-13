# Networking Layer

## Overview

NodeManager is the central orchestrator for all peer-to-peer communication. It manages TCP connections, dispatches incoming messages to protocol handlers, and coordinates header synchronization and block download. Built on tokio for async I/O, with DashMap for lock-free concurrent peer state.

Key types:
- `NodeManager` -- concurrent peer tracker, owns all shared state (header store, chainstate, mempool, block store)
- `Node` -- single peer: connection state, handshake data, rate limiting, known inventory
- `NodeState` -- lifecycle enum (Connecting, Handshaking, Connected, Disconnected, Dead, Banned)
- `BlockDownloadManager` -- sliding-window block fetcher, runs independently via channels

## Connection Management

Ironcat distinguishes incoming (peer-initiated) and outgoing (we-initiated) connections. Both share the same `Node` type but differ in handshake ordering (see [protocol.md](protocol.md) for the handshake sequence).

**Limits and policies:**

| Parameter | Value |
|-----------|-------|
| Max tracked nodes | 5000 |
| Max incoming connections | 125 |
| Incoming accept rate | 50/second |
| Connections per IP | 1 (both directions combined) |

One connection per IP is enforced as a sybil/eclipse defense. Incoming connections to an IP that is already connecting, handshaking, or connected are rejected. Incoming connections can replace disconnected or dead nodes at the same IP.

**Writer model:** Each connected peer has a `SharedTcpWriter` (`Arc<Mutex<OwnedWriteHalf>>`) for concurrent message sending. DashMap shard locks are never held across await points -- the version handler uses a 3-phase lock pattern (read, drop lock, write) to avoid deadlocks.

## Peer Lifecycle

### Discovery

On startup (unless `--no-dns-seed`), ironcat resolves all Catcoin DNS seeds in parallel to discover initial peers. Each seed hostname is queried with a 5-second timeout. Results are deduplicated and shuffled before connecting.

Seeds (from Catcoin Core `chainparams.cpp`):

- `catcoin.seeds.multicoin.co`
- `dnsseed.catalyst.ovh`
- `dnsseed.catcointomars.top`
- `dnsseed.wildcat.ovh`
- `dnsseed.catcoin.ovh`
- `dnsseed.bcats.top`
- `dnsseed.jjcatcoin.top`
- `dnsseed.catsonmylap.top`
- `dnsseed.catcoin.party`
- `dnsseed.remembermeasyoupassby.top`
- `seed.catcoinwallets.com`

The `--seed` node always connects regardless of DNS results, serving as a fallback.

After initial discovery, peers are found through addr gossip from connected nodes.

### Addr Gossip

Connected peers exchange `addr` messages containing IP/port/timestamp entries. Incoming entries are filtered and rate-limited:

- **Freshness filter:** Addresses older than 24 hours (with 10-minute future tolerance for clock skew) are silently dropped
- **Token bucket:** 0.1 tokens/second refill rate, 1000 capacity, starts at 10. Each recently-active entry consumes 1 token; stale entries are free. Entries arriving with an empty bucket are silently discarded.

### Addr Relay

When receiving an addr message with 10 or fewer entries from a peer we did NOT send a getaddr to, recently-active entries are relayed to 2 other connected outgoing peers. Addr responses triggered by our own getaddr are full dumps (not organic gossip) and excluded from relay to prevent amplification.

Peer selection is deterministic: each candidate peer is scored using `SipHash-1-3(relay_key, addr_hash, time_bucket_24h, peer_hash)`. The top 2 peers by score are chosen, ensuring the same address goes to the same peers within a 24-hour window.

Each peer maintains an `addr_known` set (max 5000 entries). Addresses already in the set are not sent again. The set is cleared when the 24-hour time bucket rotates, bounding memory.

### Self-Announcement

Every 6 hours, ironcat announces its own address to all connected peers, but only if:

- At least 1 incoming peer is connected (proves our port is publicly reachable)
- At least 3 peers agree on our external IP with >50% of total votes (consensus from `addr_recv` in version messages, applied only after verack; non-routable IPs rejected)

### Backoff and Retry

When a peer disconnects, ironcat retries with exponential backoff:

| Parameter | Value |
|-----------|-------|
| Base delay | 30 seconds |
| Multiplier | 2x per attempt |
| Max delay | 30 minutes |
| Jitter | +/-25% |
| Max attempts | 10 (then Dead) |

TCP connect uses up to 3 attempts per cycle with a 10-second timeout each and 30 seconds between retries.

Dead nodes can be revived by a fresh addr message with a newer timestamp than their `last_seen`.

### Reaper

A background task runs every 30 seconds to:

- Reschedule disconnected nodes whose backoff timer has elapsed
- Clear nodes stuck in Connecting (>300s) or Handshaking (>60s)
- Expire old bans (24-hour expiry)
- Evict external IP votes from disconnected peers

## Message Dispatch

Raw TCP bytes flow through a pipeline:

1. **Read:** 8192-byte reads from the TCP socket
2. **Buffer:** `NetworkQueue` accumulates bytes, extracts complete messages. Buffer capped at 4 MB (2x max message size), auto-shrinks if >25% over-allocated
3. **Parse:** `Message::from_bytes()` validates magic bytes, payload length, and SHA256d checksum. Returns `Incomplete` (wait for more data) or `Corrupt` (disconnect peer)
4. **Route:** Command string dispatched to the appropriate `handler_*.rs` module

See [protocol.md](protocol.md) for wire format details, message definitions, and the handshake state machine.

## Block Download

`BlockDownloadManager` coordinates block fetching after headers are synced. It runs as an independent tokio task, communicating with handlers via channels:

- `block_sender/block_rx` (mpsc): handlers forward received block payloads
- `disconnect_sender/disconnect_rx` (unbounded): reaper notifies of peer disconnects

### Sliding Window

| Parameter | Value |
|-----------|-------|
| Window size | 512 blocks max in flight |
| Per-peer limit | 48 blocks |
| Request timeout | 10 seconds |
| Refill threshold | 128 free slots (25% of window) |
| Scan batch size | 4096 heights per header lookup |
| Flush interval | 500 blocks between disk syncs |
| Max pending (out-of-order) | 1024 blocks |

The manager fills the window in three phases:

1. **Collect:** Gather requestable (height, hash) pairs from the header store, skipping blocks already stored, in-flight, or pending
2. **Assign:** Distribute contiguous chunks to connected peers round-robin, respecting per-peer limits
3. **Send:** Batch getdata messages per peer, track in `in_flight` map

### Out-of-Order Handling

Blocks may arrive out of height order. A `BTreeMap` buffers blocks that arrive ahead of `next_connect_height`. When a block at the expected height arrives, it is connected to chainstate and the pending buffer is drained in height order until a gap appears.

### Timeout and Recovery

After every event (not just timer ticks), a timeout scan checks for in-flight entries older than 10 seconds. Timed-out entries are removed and `next_height` is reset to the minimum timed-out height for immediate re-request.

When a peer disconnects, all its in-flight entries are expired immediately without waiting for the 10-second timeout.

### Chainstate Connection

Blocks are connected in strict height order via `chainstate.connect_block()`, which validates inputs, checks coinbase maturity, verifies scripts in parallel (rayon), and stores undo data. See [consensus.md](consensus.md) for validation details.

On startup, `catch_up_chainstate()` reads already-stored blocks from disk to resume where a previous session left off.

### Reorg Handling

After a chain reorganization, `handle_reorg` resets the download manager: clears all in-flight requests and pending blocks (they may reference the old chain), and sets `next_connect_height` to resume downloading from the new tip. The headers handler triggers reorgs post-IBD when `accept_header` detects a fork with more cumulative work. See [consensus.md](consensus.md) for reorg execution details.

## Shutdown

On exit (Ctrl+C or TUI quit), ironcat sends a clean TCP FIN to all connected peers before terminating. Writers are collected without holding DashMap locks, then shut down sequentially to avoid lock contention across await points.

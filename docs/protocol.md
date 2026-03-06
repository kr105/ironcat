# Catcoin Network Protocol

Catcoin is a Litecoin fork. The wire protocol is nearly identical, with different magic bytes and default port.

## Constants

| Name | Value |
|------|-------|
| Network magic | `0xFC 0xC1 0xB7 0xDC` |
| Default port | 9933 |
| Protocol version | 70012 |
| Max message size | 2,000,000 bytes |
| Max addr entries | 1000 per message |
| Max varstr length | 4096 bytes |
| User agent | `/Ironcat:0.0.7/` |

## Message Framing

Every message on the wire uses the same 24-byte header followed by a variable-length payload.

```
[4] magic           - Network magic bytes (0xFC 0xC1 0xB7 0xDC)
[12] command        - ASCII command name, null-padded
[4] length          - Payload length (u32 LE)
[4] checksum        - First 4 bytes of SHA256d(payload) (u32 LE)
[...] payload       - Variable length
```

Checksum is `SHA256(SHA256(payload))`, truncated to the first 4 bytes, stored as u32 LE.

## Data Types

### VarInt

Variable-length integer encoding:

| Value range | Encoding |
|-------------|----------|
| < 0xFD | 1 byte, value directly |
| <= 0xFFFF | `0xFD` + 2 bytes (u16 LE) |
| <= 0xFFFFFFFF | `0xFE` + 4 bytes (u32 LE) |
| > 0xFFFFFFFF | `0xFF` + 8 bytes (u64 LE) |

### VarStr

Variable-length string: VarInt(length) followed by `length` bytes of UTF-8 data.

### Hash256 (32 bytes)

A 256-bit hash stored in wire order (the raw byte output of SHA-256). All block hashes, transaction IDs, and merkle roots use this type.

Display convention: bytes are reversed from wire order for human-readable output (block explorer format). For example, the Catcoin genesis block hash displays as `bc3b4ec4...9296` but the first wire byte is `0x96`.

`double_sha256(data)` computes `SHA256(SHA256(data))`, used for message checksums, block identity hashes, and transaction IDs.

### InvItem (36 bytes)

Used in inv, getdata, and notfound messages to identify a piece of data.

```
[4] type            - Inventory type (u32 LE)
[32] hash           - Hash256 identifying the data
```

Inventory types:

| Value | Name | Description |
|-------|------|-------------|
| 1 | MSG_TX | Transaction |
| 2 | MSG_BLOCK | Block |
| 3 | MSG_FILTERED_BLOCK | Filtered block (BIP37) |
| 4 | MSG_CMPCT_BLOCK | Compact block (BIP152) |

The witness flag (bit 30) and MWEB flag (bit 29) are stripped before matching. Unknown types are silently skipped.

### NetworkAddress (26 bytes)

Used in version and addr messages. No timestamp prefix in version messages; addr messages prepend a u32 LE timestamp.

```
[8] services        - Service flags (u64 LE)
[16] address        - IPv6 or IPv4-mapped IPv6, network byte order
[2] port            - Port number (u16 BE)
```

IPv4 addresses are encoded as IPv4-mapped IPv6: `::ffff:a.b.c.d`.

## Service Flags

Bitfield (u64 LE):

| Bit | Name | Value | Meaning |
|-----|------|-------|---------|
| 0 | NODE_NETWORK | 1 | Full block serving |
| 1 | NODE_GETUTXO | 2 | UTXO queries (BIP64) |
| 2 | NODE_BLOOM | 4 | Bloom filtering (BIP111) |
| 3 | NODE_WITNESS | 8 | Segregated witness (BIP144) |
| 4 | NODE_XTHIN | 16 | Xtreme Thinblocks (discontinued) |
| 6 | NODE_COMPACT_FILTERS | 64 | Compact block filters (BIP157) |
| 10 | NODE_NETWORK_LIMITED | 1024 | Pruned node, limited blocks |

Ironcat advertises `NODE_NETWORK_LIMITED` (1024). Unknown bits from remote peers are silently dropped via `from_bits_truncate`.

## Messages

### version

Sent as the first message after TCP connect. Both sides must exchange version messages before the handshake completes.

```
[4] version         - Protocol version (u32 LE), we send 70012
[8] services        - Our service flags (u64 LE)
[8] timestamp       - Unix timestamp (i64 LE)
[26] addr_recv      - NetworkAddress of the receiving node
[26] addr_from      - NetworkAddress placeholder (26 zero bytes)
[8] nonce           - Random u64 LE, used for self-connection detection
[var] user_agent    - VarStr, e.g. "/Ironcat:0.0.7/"
[4] start_height    - Last known block height (i32 LE)
[1] relay           - BIP37 relay flag (0x00 or 0x01)
```

The relay field is always sent and always expected. If missing, the message fails to parse and the connection drops.

The nonce is generated once per `NodeManager` lifetime and compared against incoming version nonces to detect self-connections.

### verack

Empty payload. Sent after receiving and processing a version message.

### ping

```
[8] nonce           - Random u64 LE
```

Pre-BIP31 nodes may send 0-byte pings (no nonce). These are accepted silently without a pong reply. Any other payload size that isn't 0 or 8 is treated as misbehavior and results in a ban.

Ironcat sends pings every 180 seconds using `tokio::time::interval` (not reset by data arrival).

### pong

```
[8] nonce           - Echo of the ping nonce (u64 LE)
```

### getaddr

Empty payload. Requests the peer's known addresses.

On receipt, Ironcat responds with an addr message containing up to 1000 outgoing, connected nodes seen within the last 2 hours. Incoming connections are never shared (their port is ephemeral and useless). If no eligible nodes exist, no response is sent. Getaddr responses are rate limited to one per peer per minute.

### addr

```
[var] count         - VarInt, number of entries (max 1000)
```

Repeated `count` times:

```
[4] timestamp       - When this address was last seen (u32 LE)
[26] address        - NetworkAddress
```

Addresses older than 24 hours (with 10-minute future tolerance for clock skew) are filtered out on receipt. New addresses are added as outgoing nodes. Dead nodes can be revived if the addr timestamp is newer than their `last_seen`.

An empty addr list encodes as a single `0x00` byte (varint zero).

#### Addr rate limiting

Incoming addr entries are rate limited per peer using a token bucket:

| Parameter | Value |
|-----------|-------|
| Refill rate | 0.1 tokens/second (1 token per 10 seconds) |
| Bucket capacity | 1000 tokens |

Each entry in an addr message consumes 1 token. Only recently-active entries (within 24h) consume tokens; stale entries are filtered without cost. Entries that arrive when the bucket is empty are silently discarded. The bucket starts at 10 tokens for new connections and refills lazily up to 1000 at processing time.

#### Addr relay

When receiving an addr message with 10 or fewer entries from a peer we did NOT send a getaddr to, recently-active entries are relayed to 2 other connected outgoing peers. Addr responses triggered by our own getaddr are full dumps, not organic gossip, so they are excluded from relay to prevent amplification.

Peer selection is deterministic: each candidate peer is scored using `SipHash-1-3(relay_key, addr_hash, time_bucket_24h, peer_hash)`. The top 2 peers by score are chosen. This ensures the same address goes to the same peers within a 24-hour window, preventing amplification.

Each peer maintains an `addr_known` set. Addresses already in the set are not sent again. The set is cleared when the 24-hour time bucket rotates, bounding memory usage.

#### Self-announcement

Every 6 hours, Ironcat announces its own address to all connected peers, but only if:

- At least 1 incoming peer is connected (proves our port is publicly reachable)
- At least 3 peers agree on our external IP with > 50% of total votes (consensus from `addr_recv` in version messages, applied only after verack; non-routable IPs are rejected)

### inv

Announces available data (transactions, blocks) to a peer.

```
[var] count         - VarInt, number of InvItems (max 50,000)
```

Repeated `count` times:

```
[36] item           - InvItem (4 bytes type + 32 bytes hash)
```

On receipt, Ironcat records the hashes in the peer's `inv_known` set (cleared when it reaches 50,000 entries). If any announced blocks are not in the header store, a getheaders request is sent to sync headers.

### getdata

Requests specific data from a peer. Same wire format as inv.

On receipt, Ironcat responds with notfound for all requested items (no blocks or transactions to serve yet).

### notfound

Indicates that requested data is not available. Same wire format as inv. Sent in response to getdata when the peer does not have the requested items.

On receipt, Ironcat logs the message at debug level. No further action is taken.

### getheaders

Requests block headers starting from a block locator.

```
[4] version         - Protocol version (u32 LE), currently 70012
[var] hash_count    - VarInt, number of locator hashes (max 101)
```

Repeated `hash_count` times:

```
[32] hash           - Block hash (Hash256)
```

Followed by:

```
[32] hash_stop      - Hash of the last desired header, or all zeros for "send to tip"
```

The block locator is an exponentially-spaced list of block hashes from the sender's tip back to genesis: 10 consecutive hashes from the tip, then step sizes doubling (2, 4, 8, ...), always ending with genesis. Maximum 101 hashes.

On receipt, Ironcat finds the first locator hash that exists in its chain, then responds with a headers message containing up to 2000 headers starting after that point. Uses the height index for O(k) response construction.

### headers

Response to getheaders, or unsolicited if peer sent sendheaders.

```
[var] count         - VarInt, number of headers (max 2000)
```

Repeated `count` times:

```
[80] header         - Block header (version + prev_hash + merkle_root + timestamp + bits + nonce)
[var] tx_count      - VarInt, always 0 for headers messages
```

On receipt, Ironcat validates chain continuity (each header's prev_hash must connect to a known header) and difficulty targets (nBits must match the expected value from the active CIP algorithm). Valid headers are stored via batch insertion. If any header in the batch fails validation, the entire batch is rejected. If exactly 2000 headers were received, a follow-up getheaders is sent to continue syncing.

#### Difficulty algorithms

Headers are validated against the correct difficulty adjustment algorithm based on block height:

| Height range | Algorithm | Description |
|--------------|-----------|-------------|
| 0 - 20288 | CIP01 | Original 2016-block retarget with 0.25x-4x clamping |
| 20289 | -- | Hardcoded difficulty 16 reset (0x1c0ffff0) |
| 20289 - 21345 | CIP02 | 36-block retarget with 0.25x-4x clamping |
| 21346 - 27259 | CIP03 | Every-block retarget with tight +/-12% bounds |
| 27260 - 46330 | CIP04 | PID controller with 8-block lookback and dead zone |
| 46331 - 396999 | CIP05 | Time-gated DigiShield with CIP04 fallback |
| 397000+ | CIP06 | LWMA-1 with 45-block weighted moving average |

During batch validation, a `BatchLookup` overlay makes already-validated headers from the current batch visible to the difficulty calculation, even before they are committed to storage.

### sendheaders

Empty payload. Sent after handshake to signal that the sender prefers to receive new block announcements as headers messages instead of inv messages (BIP 130, protocol version >= 70012).

On receipt, Ironcat sets the peer's `prefer_headers` flag.

### alert

Ignored. Legacy alert system, deprecated.

## Handshake Sequence

### Outgoing connection (we initiate)

```
Us              Peer
|-- version      -->  |
|<-- version      --|
|-- verack       -->  |
|<-- verack       --|
|-- getaddr      -->  |
|-- sendheaders  -->  |  (if peer version >= 70012)
|-- getheaders   -->  |
```

1. We connect and immediately send our version
2. Peer responds with their version
3. We send verack (acknowledging their version)
4. Peer sends verack (acknowledging our version)
5. On verack receipt (with version already received), state transitions to Connected
6. We send getaddr to discover more peers
7. If peer version >= 70012, we send sendheaders (BIP 130)
8. We send getheaders with our block locator to begin header sync

### Incoming connection (peer initiates)

```
Peer            Us
|-- version      -->  |
|<-- version      --|
|<-- verack       --|
|-- verack       -->  |
|<-- getaddr      --|
|<-- sendheaders  --|  (if peer version >= 70012)
|<-- getheaders   --|
```

1. Peer connects and sends their version
2. We respond with our version + verack
3. Peer sends verack
4. State transitions to Connected, we send getaddr
5. If peer version >= 70012, we send sendheaders (BIP 130)
6. We send getheaders with our block locator to begin header sync

### Handshake rules

- Each side sends exactly one version message. A second version triggers a ban (ProtocolViolation)
- Self-connections (matching nonce) trigger a ban
- The `version_received` flag tracks whether we got a version from the peer, independent of the version number itself
- On reconnect, `version_received` is reset to false
- Receiving verack before version is a protocol violation and triggers a ban

## Node State Machine

```
Connecting --> Handshaking --> Connected --> Disconnected --> Dead
                                                |
                                                v (retry)
                                           Connecting

Any state --> Banned (permanent, no retry)
Dead --> Connecting (revived by fresh addr timestamp)
```

| State | Description |
|-------|-------------|
| Connecting { since } | TCP connect in progress |
| Handshaking { since } | TCP up, version exchange in progress |
| Connected { writer } | Fully operational |
| Disconnected { retry_at, attempt } | Waiting for backoff timer |
| Dead | All retries exhausted |
| Banned { reason } | Permanent, never retry |

### Timeouts

| Condition | Timeout |
|-----------|---------|
| Stuck in Connecting | 300s, then scheduled for retry |
| Stuck in Handshaking | 60s, then scheduled for retry |
| Idle connection | 360s (2x ping interval), then disconnected |
| Reaper scan interval | 30s |

### Retry logic

Exponential backoff: `base * 2^attempt`, capped at 30 minutes, with +/-25% jitter.

- Base delay: 30 seconds
- Max delay: 1800 seconds (30 minutes)
- Max attempts: 10 (then Dead)
- TCP connect: up to 3 attempts per cycle, 10s timeout each, 30s between retries

### Ban reasons

| Reason | Trigger |
|--------|---------|
| ProtocolViolation | Duplicate version message, self-connection, verack before version |
| Misbehavior | Malformed ping (non-zero, non-8 byte payload), negative start_height |

### Ban duration

All bans expire after 24 hours (86,400 seconds). On startup, expired bans are filtered out. Peer and ban state is persisted to `peers.dat` and `banlist.dat` in the data directory (`~/.ironcat/` by default, configurable via `--datadir`).

## Connection Details

- TCP listener binds to `0.0.0.0:9933`
- Incoming accept rate limited to 50 connections per second
- Maximum 125 concurrent incoming connections
- Read buffer: 8192 bytes per read call, passed directly to `NetworkQueue` for message parsing
- Writer is `Arc<Mutex<OwnedWriteHalf>>` for concurrent access
- Max tracked nodes: 5000
- Nodes are keyed by IP address (one connection per IP, both inbound and outbound, for sybil/eclipse defense)
- Incoming connections that arrive for an IP already active (connecting, handshaking, or connected) are rejected
- Incoming connections can replace disconnected or dead nodes at the same IP, resetting state to Handshaking
- If a disconnected or dead node receives an addr update with a different port, the stored port is updated silently for the next reconnection attempt
- `DashMap` locks are never held across await points (version handler uses 3-phase lock pattern)

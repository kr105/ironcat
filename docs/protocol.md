# Catcoin Network Protocol

Catcoin is a Bitcoin fork. The wire protocol is nearly identical to Bitcoin's, with different magic bytes and default port.

## Constants

| Name | Value |
|------|-------|
| Network magic | `0xFC 0xC1 0xB7 0xDC` |
| Default port | 9933 |
| Protocol version | 70003 |
| Max message size | 5,000,000 bytes |
| Max addr entries | 1000 per message |
| Max varstr length | 4096 bytes |
| User agent | `/Ironcat:0.0.1/` |

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
[4] version         - Protocol version (u32 LE), we send 70003
[8] services        - Our service flags (u64 LE)
[8] timestamp       - Unix timestamp (i64 LE)
[26] addr_recv      - NetworkAddress of the receiving node
[26] addr_from      - NetworkAddress placeholder (26 zero bytes)
[8] nonce           - Random u64 LE, used for self-connection detection
[var] user_agent    - VarStr, e.g. "/Ironcat:0.0.1/"
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

On receipt, Ironcat responds with an addr message containing up to 1000 outgoing, connected nodes seen within the last 2 hours. Incoming connections are never shared (their port is ephemeral and useless).

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

### alert

Ignored. Legacy Bitcoin alert system, deprecated.

## Handshake Sequence

### Outgoing connection (we initiate)

```
Us              Peer
|-- version -->  |
|               |-- version -->|
|<-- version --|
|-- verack -->  |
|<-- verack --|
|-- getaddr --> |
```

1. We connect and immediately send our version
2. Peer responds with their version
3. We send verack (acknowledging their version)
4. Peer sends verack (acknowledging our version)
5. On verack receipt (with version already received), state transitions to Connected
6. We send getaddr to discover more peers

### Incoming connection (peer initiates)

```
Peer            Us
|-- version -->  |
|<-- version --|
|<-- verack  --|
|-- verack -->  |
|<-- getaddr --|
```

1. Peer connects and sends their version
2. We respond with our version + verack
3. Peer sends verack
4. State transitions to Connected, we send getaddr

### Handshake rules

- Each side sends exactly one version message. A second version triggers a ban (ProtocolViolation)
- Self-connections (matching nonce) trigger a ban
- The `version_received` flag tracks whether we got a version from the peer, independent of the version number itself
- On reconnect, `version_received` is reset to false

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
| ProtocolViolation | Duplicate version message, self-connection |
| Misbehavior | Malformed ping (non-zero, non-8 byte payload) |

## DNS Seed Discovery

On startup (unless `--no-dns-seed`), ironcat resolves all Catcoin DNS seeds in parallel to discover peer IPs. Each seed hostname is queried with a 5-second timeout. Results are deduplicated and shuffled before connecting.

DNS seeds (from Catcoin Core `chainparams.cpp`):

```
catcoin.seeds.multicoin.co
dnsseed.catalyst.ovh
dnsseed.catcointomars.top
dnsseed.wildcat.ovh
dnsseed.catcoin.ovh
dnsseed.bcats.top
dnsseed.jjcatcoin.top
dnsseed.catsonmylap.top
dnsseed.catcoin.party
dnsseed.remembermeasyoupassby.top
seed.catcoinwallets.com
```

The `--seed` flag always connects regardless of DNS results, serving as a fallback.

## Connection Details

- TCP listener binds to `0.0.0.0:9933`
- Reader uses `BufReader` with 8192-byte capacity
- Read buffer: 4096 bytes per read call
- Writer is `Arc<Mutex<OwnedWriteHalf>>` for concurrent access
- Max tracked nodes: 5000
- Incoming connections are keyed by their ephemeral port (cannot be retried)
- `DashMap` locks are never held across await points (version handler uses 3-phase lock pattern)

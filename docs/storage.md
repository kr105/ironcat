# Storage Layer

## Data Directory

All persistent data lives in a single directory (default `~/.ironcat/`, configurable via `--datadir`).

```
~/.ironcat/
+-- headers.redb        # Block header database (redb)
+-- index.redb          # Block location index (redb)
+-- chainstate.redb     # UTXO set, chain tip, undo index (redb)
+-- blocks/
|   +-- blk00000.dat    # Block data, append-only
|   +-- blk00001.dat    # Rotated when file exceeds 256 MB
|   +-- rev00000.dat    # Undo data for block disconnection
|   +-- rev00001.dat
|   +-- ...
+-- peers.dat           # Known peers (checksummed bincode)
+-- banlist.dat         # Ban list (checksummed bincode)
+-- debug.log           # Tracing output (append mode, unless --no-logfile)
```

## Header Storage

Block headers are stored in `headers.redb` and loaded entirely into memory at startup for fast access.

### Database Schema

Single redb table:

| Table | Key | Value |
|-------|-----|-------|
| `headers` | u32 (block height) | `[u8; 80]` (raw serialized header) |

Block hashes are not stored -- they are recomputed from the raw bytes on load.

### Write Strategy

Write-through: headers are persisted to redb on every batch insertion, then committed to the in-memory store. The in-memory store uses dual indexing:

- `HashMap<Hash256, StoredHeader>` for O(1) hash lookups
- `Vec<Hash256>` for O(1) height lookups

The HashMap is pre-allocated for 500,000 entries to minimize rehashing during initial block download.

### Loading

On startup, all headers are loaded from redb in a single scan. Heights must be contiguous from 0 -- any gap causes load failure. A safety cap of 10 million headers prevents OOM from corrupted databases.

If genesis doesn't match the expected network hash, the store is cleared. Backend errors cause fallback to in-memory only.

### Backend Abstraction

`HeaderStoreBackend` is a trait allowing pluggable storage backends. Currently only `RedbHeaderStore` implements it.

## Block Storage

Blocks are stored in append-only flat files with a separate redb index for O(1) lookups by hash.

### Flat Files

Files are named `blk00000.dat`, `blk00001.dat`, etc., in the `blocks/` subdirectory. When a file would exceed 256 MB with the next block, a new file is created.

Each block is written as:

```
[4 bytes] magic       - Network magic (0xFC 0xC1 0xB7 0xDC)
[4 bytes] size        - Block data size, u32 LE (excludes this 8-byte prefix)
[N bytes] block data  - Raw serialized block
```

### Index

The index lives in `index.redb`:

| Table | Key | Value |
|-------|-----|-------|
| `block_index` | `[u8; 32]` (block hash) | `(u16, u32, u32)` (file number, byte offset, data size) |

The offset points to the start of the magic prefix. The size is the block data only (excluding the 8-byte prefix).

### Write Strategy

Flat file writes happen immediately (buffered by OS). Index updates are queued in a `pending` buffer and committed in a single redb transaction on `flush()`, which happens every 500 blocks. The flat file is fsynced before the index transaction commits.

`store_block()` is idempotent -- it checks the committed index before writing. `store_block_unchecked()` skips this check when the caller guarantees uniqueness (used by the block download manager which tracks stored hashes in memory).

### Crash Recovery

If ironcat crashes after fsyncing the flat file but before committing the index transaction, blocks exist on disk but are unindexed. They will be re-downloaded on the next sync. On startup, the index is cross-checked against actual file sizes; if the index references data beyond a file's end, the index is cleared and re-download begins.

### In-Memory Hash Set

`BlockDownloadManager` loads all indexed hashes into a `HashSet` at startup (`all_indexed_hashes()`). This avoids redb reads in the hot path -- "do we already have this block?" is an O(1) in-memory check.

## Peer and Ban Persistence

Peer and ban data are saved to `peers.dat` and `banlist.dat` every 15 minutes by a background task.

### Encoding

Both files use the same format: 32-byte SHA256 checksum followed by bincode-serialized data.

```
[32 bytes] SHA256(payload)
[N bytes]  bincode payload
```

All writes are atomic: data is written to a `.tmp` file first, then renamed to the final path. This prevents corruption from partial writes.

### Peer Data (`peers.dat`)

```rust
PeerDb {
    version: u32,               // Schema version for future migrations
    peers: Vec<SavedPeer>,      // Known peers
}

SavedPeer {
    ip: IpAddr,
    port: u16,
    services: u64,              // ServiceMask bits
    last_seen: u64,             // Unix timestamp
    user_agent: String,
    height: i32,                // Last known block height
}
```

### Ban Data (`banlist.dat`)

```rust
BanDb {
    version: u32,
    bans: Vec<SavedBan>,
}

SavedBan {
    ip: IpAddr,
    reason: String,             // "ProtocolViolation" or "Misbehavior"
    created: u64,               // Unix timestamp
    expires: u64,               // Unix timestamp (created + 86400)
}
```

All bans expire after 24 hours. On load, `active_bans()` filters out expired entries. Historical (expired) bans remain in the file until the next save overwrites it.

### Corruption Handling

If a file is missing, ironcat starts fresh with no peers/bans (DNS seeds bootstrap the peer set). If a file is present but the checksum doesn't match or bincode deserialization fails, a warning is logged and the file is treated as missing.

## Chainstate Storage

The UTXO set and chain metadata live in `chainstate.redb`.

### Database Schema

| Table | Key | Value |
|-------|-----|-------|
| `UTXO_SET` | `[u8; 36]` (txid 32 + output index 4, LE) | Serialized `Coin` |
| `META` | String key | Bytes value |
| `UNDO_INDEX` | `[u8; 32]` (block hash) | `(u16, u32, u32)` (file number, offset, size) |

META stores chain tip hash and height.

### Coin Serialization

```
[4 bytes] height        - Block height where this output was created (u32 LE)
[1 byte]  is_coinbase   - 0x00 or 0x01
[8 bytes] value         - Output value in base units (i64 LE)
[4 bytes] script_len    - Script length (u32 LE)
[N bytes] script        - Output script (scriptPubKey)
```

### Undo Files

Undo data enables block disconnection (chain reorgs). For each connected block, the coins spent by its transactions are serialized and stored.

Undo files live in the same `blocks/` directory as block files, named `rev00000.dat`, `rev00001.dat`, etc. They use the same format as block flat files:

```
[4 bytes] magic       - Network magic (0xFC 0xC1 0xB7 0xDC)
[4 bytes] size        - Undo data size, u32 LE
[N bytes] undo data   - Serialized BlockUndo (Vec<TxUndo>, each a Vec<Coin>)
```

Files rotate at 256 MB. The `UNDO_INDEX` table in `chainstate.redb` maps each block hash to the undo file location `(u16 file_number, u32 offset, u32 size)`.

On startup, the undo store verifies index consistency: if a flat file is smaller than the index expects, the index is cleared and undo data is regenerated as blocks are re-connected.

# ironcat

A Catcoin full node written from scratch in Rust.

## What is this

Catcoin is a scrypt-based cryptocurrency launched on December 23, 2013 -- less than three weeks after Dogecoin -- as the first cat-themed proof-of-work coin. Built as "Bitcoin on Scrypt," it mirrors Bitcoin's core parameters (21 million supply cap, 50 coins per block, halving every 210,000 blocks, 10-minute targets) while using Litecoin's scrypt algorithm. Its codebase is a Litecoin fork. Over its history, Catcoin has gone through 6 difficulty adjustment algorithms (CIPs), including the first civilian application of a PID controller for dynamic difficulty adjustment.

Ironcat is an independent full node implementation that doesn't share code with Catcoin Core. It implements the Catcoin P2P protocol from scratch, including header synchronization, block download and validation, a script engine, UTXO management, and a mempool. The goals are a clean Rust implementation with memory safety guarantees, and a deep understanding of the protocol by building it from the ground up.

## Current status

**Working:**
- Full P2P networking (handshake, peer discovery via DNS seeds and addr gossip, connection management)
- Headers-first chain synchronization with difficulty validation across all 6 CIP algorithms
- Block download via sliding-window manager with parallel peer fetching
- Block and transaction validation (structural, contextual, and script verification)
- Script engine with P2SH support, 45+ opcodes, ECDSA signature verification
- Chainstate with UTXO tracking, coinbase maturity, and undo data for future reorg support
- Transaction mempool with fee-based eviction, script verification, and parent chain resolution
- Persistent storage for headers, blocks, peers, bans, and chainstate across restarts
- TUI mode with live stats, peer table, mempool view, and log panel
- Daemon mode for headless servers

**Not yet implemented:**
- Chain reorganization (reorg detection and block disconnection)
- Wallet (key management, address generation, transaction creation)
- JSON-RPC interface for programmatic access
- Mining / block template creation
- Proper address manager with bucketed peer selection

See [TODO.md](TODO.md) for the full roadmap.

## Architecture

```
+------------------------------------------------------+
|                     Networking                        |
|  Peer management, message dispatch, block download    |
|  docs/networking.md                                   |
+------------------------------------------------------+
|                     Consensus                         |
|  Difficulty, validation, script engine, chainstate    |
|  docs/consensus.md                                    |
+------------------------------------------------------+
|                      Storage                          |
|  Headers (redb), blocks (flat files), peers (bincode) |
|  docs/storage.md                                      |
+------------------------------------------------------+
|                     Protocol                          |
|  Wire format, messages, handshake, state machine      |
|  docs/protocol.md                                     |
+------------------------------------------------------+
```

## Building and running

Requires Rust (stable). No C/C++ dependencies.

```bash
cargo build --release
cp target/release/ironcat .
```

### CLI options

```
./ironcat                                    # TUI mode (default)
./ironcat --daemon                           # Daemon mode, logs to stderr
./ironcat --seed 10.0.0.1:9933              # Custom seed node
./ironcat --no-dns-seed                     # Disable DNS peer discovery
./ironcat --datadir /path/to/data           # Custom data directory
./ironcat --no-logfile                      # Disable debug.log file output
./ironcat --help                            # Show all options
```

### TUI mode

The default mode launches a terminal UI with tabs for node overview, mempool stats, connected peers, and live logs. Press `q` to quit.

### Daemon mode

Use `--daemon` to run without a TUI. All logs go to stderr with timestamps. Suitable for headless servers and background operation.

## Configuration

### Data directory

Default: `~/.ironcat/`. Override with `--datadir`. Contains all persistent state (headers, blocks, chainstate, peers, bans, logs). See [docs/storage.md](docs/storage.md) for the full directory layout.

### Log levels

Controlled via the `RUST_LOG` environment variable (powered by `tracing-subscriber`):

```bash
RUST_LOG=info ./ironcat --daemon    # Info and above (default)
RUST_LOG=debug ./ironcat            # Debug and above
RUST_LOG=trace ./ironcat            # Everything
```

All logs are also written to `debug.log` in the data directory (append mode, persists across restarts). Disable with `--no-logfile`.

### Seed nodes

DNS seeds are resolved on startup unless `--no-dns-seed` is passed.

## Notable dependencies

- **redb** -- Pure Rust embedded key-value database for persistent header, block index, and chainstate storage. ACID transactions, zero C/C++ linkage
- **k256** -- secp256k1 elliptic curve for ECDSA signature verification in the script engine
- **scrypt** -- Reference implementation used for test verification of the native scrypt PoW hasher
- **siphasher** -- Keyed SipHash for deterministic addr relay peer selection
- **ratatui** -- Terminal UI framework for the interactive node dashboard
- **rayon** -- Data parallelism for scrypt PoW verification during header sync and script verification during block connection
- **tokio** -- Async runtime for all networking and I/O
- **dashmap** -- Concurrent hash map for lock-free peer state access

## Contributing

```bash
cargo test              # Run all tests
cargo clippy            # No warnings allowed
cargo fmt --check       # Must pass
```

Follow the coding standards in [CLAUDE.md](CLAUDE.md). Key points: TDD methodology, no `unwrap`/`expect` in production code, `anyhow` for errors with `.context()` at module boundaries, structured logging via `tracing`.

AI-assisted contributions are encouraged, but PRs that don't meet minimum quality standards won't be reviewed. At minimum, all contributions must follow CLAUDE.md. All merged PRs are reviewed and approved by a human and by Claude.

## License

Apache-2.0. See [LICENSE](LICENSE).

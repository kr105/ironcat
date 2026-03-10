# ironcat

A Catcoin network client implemented from scratch in Rust.

## Usage

```bash
cargo run                                    # TUI mode (default)
cargo run -- --daemon                        # Daemon mode, logs to stderr
cargo run -- --seed 10.0.0.1:9933            # Custom seed node (default: 161.129.176.92:9933)
cargo run -- --no-dns-seed                   # Disable DNS peer discovery
cargo run -- --datadir /path/to/data         # Custom data directory (default: ~/.ironcat)
cargo run -- --no-logfile                    # Disable debug.log file output
RUST_LOG=debug cargo run                     # Control log level
cargo run -- --help                          # Show help
```

## Features

- Full Catcoin P2P handshake (version/verack)
- Peer discovery via DNS seeds and addr gossip
- One connection per IP (sybil/eclipse defense)
- Proactive addr relay with deterministic peer selection and rate limiting
- Periodic self-announcement when publicly reachable
- Exponential backoff with jitter for disconnected peers
- Headers-first chain synchronization (getheaders/headers, protocol version 70012)
- Difficulty validation for all 6 CIP algorithms (CIP01-CIP06: original retarget, 36-block, tight bounds, PID controller, DigiShield, LWMA)
- sendheaders signal (BIP 130) for header-based block announcements
- In-memory header chain with dual indexing (hash and height)
- Persistent header storage via redb (pure Rust embedded DB, write-through)
- Block download via sliding-window manager (getdata/block, merkle root validation)
- Flat-file block storage (blk*.dat) with redb index for O(1) lookup
- Core blockchain types (block headers, transactions, blocks, inventory)
- Inventory protocol (inv/getdata/notfound) with header-triggered sync
- TUI and daemon modes with block download progress display
- Persistent peer, ban, header, and block storage across restarts

## TUI mode

The default mode launches a terminal UI with node stats, a connected nodes table, and a live log panel. Press `q` to quit.

## Daemon mode

Use `--daemon` to run without a TUI. All logs go to stderr with timestamps. Suitable for headless servers.

## Logging

All logs are written to `debug.log` in the data directory (default `~/.ironcat/debug.log`), in addition to stderr (daemon mode) or the TUI log panel. The file is opened in append mode so logs persist across restarts. Use `--no-logfile` to disable file logging.

## Log levels

Log verbosity is controlled via the `RUST_LOG` environment variable (powered by `tracing-subscriber`'s `EnvFilter`). Examples:

```bash
RUST_LOG=info cargo run -- --daemon    # Only info and above
RUST_LOG=debug cargo run               # Debug and above
RUST_LOG=trace cargo run               # Everything
```

## Notable dependencies

- **redb** -- Pure Rust embedded key-value database for persistent header storage. Copy-on-write B-trees, ACID transactions, zero C/C++ linkage
- **siphasher** -- Keyed SipHash for deterministic addr relay peer selection. Provides stable, cross-platform hashing independent of Rust's `DefaultHasher` which can change between compiler versions

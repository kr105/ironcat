# ironcat

A Catcoin network client implemented from scratch in Rust.

## Usage

```bash
cargo run                                    # TUI mode (default)
cargo run -- --daemon                        # Daemon mode, logs to stderr
cargo run -- --seed 10.0.0.1:9933            # Custom seed node (default: 161.129.176.92:9933)
cargo run -- --no-dns-seed                   # Disable DNS peer discovery
cargo run -- --datadir /path/to/data         # Custom data directory (default: ~/.ironcat)
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
- sendheaders signal (BIP 130) for header-based block announcements
- In-memory header chain with dual indexing (hash and height)
- Core blockchain types (block headers, transactions, inventory)
- Inventory protocol (inv/getdata/notfound) with header-triggered sync
- TUI and daemon modes
- Persistent peer and ban storage across restarts

## TUI mode

The default mode launches a terminal UI with node stats, a connected nodes table, and a live log panel. Press `q` to quit.

## Daemon mode

Use `--daemon` to run without a TUI. All logs go to stderr with timestamps. Suitable for headless servers and piping logs to a file.

## Log levels

Log verbosity is controlled via the `RUST_LOG` environment variable (powered by `tracing-subscriber`'s `EnvFilter`). Examples:

```bash
RUST_LOG=info cargo run -- --daemon    # Only info and above
RUST_LOG=debug cargo run               # Debug and above
RUST_LOG=trace cargo run               # Everything
```

## Notable dependencies

- **siphasher** -- Keyed SipHash for deterministic addr relay peer selection. Provides stable, cross-platform hashing independent of Rust's `DefaultHasher` which can change between compiler versions

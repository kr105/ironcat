# ironcat

A Catcoin network client implemented from scratch in Rust.

## Usage

```bash
cargo run                                    # TUI mode (default)
cargo run -- --daemon                        # Daemon mode, logs to stderr
cargo run -- --seed 10.0.0.1:9933            # Custom seed node
RUST_LOG=debug cargo run                     # Control log level
cargo run -- --help                          # Show help
```

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

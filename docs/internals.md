# Internals

## Shutdown

On exit (Ctrl+C or TUI quit), ironcat sends a clean TCP FIN to all connected peers before terminating. Writers are collected without holding DashMap locks, then shut down sequentially to avoid lock contention across await points.

## Connection Lifecycle

See [protocol.md](protocol.md) for the node state machine (Connecting, Handshaking, Connected, Disconnected, Dead, Banned) and retry/backoff details.

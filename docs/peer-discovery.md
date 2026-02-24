# Peer Discovery

## DNS Seeds

On startup (unless `--no-dns-seed`), ironcat resolves all Catcoin DNS seeds in parallel to discover peers. Each seed hostname is queried with a 5-second timeout. Results are deduplicated and shuffled before connecting.

Seeds from Catcoin Core `chainparams.cpp`:

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

## Addr Propagation

Connected peers exchange `addr` messages containing IP/port/timestamp entries. Addresses are filtered by a 24-hour freshness window before being added to the node set. See [protocol.md](protocol.md) for addr message format.

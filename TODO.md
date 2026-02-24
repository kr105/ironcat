# TODO

## Address Manager (addrman)

Currently all discovered peers go directly to `insert_outgoing` and connect immediately. A proper address manager would:

- Separate "new" (heard about) vs "tried" (connected to) addresses
- Bucket addresses by subnet to resist eclipse attacks
- Persist addresses to disk (peers.dat equivalent)
- Prioritize connection candidates by freshness and diversity

## Peer Persistence

No peers.dat equivalent. Every restart rediscovers peers from scratch via DNS and --seed.

## Max Outgoing Connections

No limit on simultaneous outgoing connections. Should cap at 8-16 to avoid resource exhaustion.

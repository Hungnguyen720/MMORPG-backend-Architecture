# Phase 1: movement and a crowd

Done when (from the architecture doc): 10 humans plus 200 bots in one zone hold
tick p99 under 10 ms and under 10 KB/s per client.

## What was built

- **Content schema v2.** A tenant is a directory of YAML files
  (`tenants/default/`): tenant config, classes, abilities and auras, mobs, zones
  with walls, spawns and (Phase 2) portals. The Phase 0 class and its five
  abilities are unchanged.
- **Bit-packed delta snapshots** (`combat-core/src/codec.rs`). Positions are
  2 cm on a 16-bit grid, facing one byte. Each snapshot is a delta against the
  last one the client acknowledged; unchanged entities cost nothing, a walking
  one about five bytes. Movement input is 17 bytes.
- **Interest management** (`zone-server/src/interest.rs`). 32 m grid, 3×3 cells
  per player, capped at the 80 nearest entities, with your target and every mob
  attacking you always included. Inside 40 m entities update at 20 Hz, to 80 m
  at 10 Hz, beyond at 4 Hz. Snapshots for all clients are built in parallel.
- **Combat events go only to those involved.** Other players' cast bars, auras
  and deaths travel in snapshots, so a crowd fighting around you does not
  multiply your combat traffic.
- **Mobs** with aggro radius, threat table, chase, melee, spellcasting through
  the same `CasterState` rules players use, leash and evade, death and respawn.
  Training dummies are passive mobs.
- **Walls** as axis-aligned blockers from the zone data: the server rejects
  moves through them, spells need line of sight, and the client, bots and mob
  AI all slide along them with the same code.
- **Player health, death and release** to the zone graveyard; out-of-combat
  health regeneration.
- **Network thread per zone** (`zone-server/src/net.rs`). renet, encryption and
  the socket live on their own thread, with batched `sendmmsg`/`recvmmsg`. The
  simulation thread sleeps until the next combat deadline or world tick and
  wakes the moment input arrives.
- **Metrics** on `http://host:9100/metrics` (Prometheus) and
  `/metrics.json`: tick p50/p99/max, bytes per client, view size, spell-queue
  gaps, movement corrections.
- **Bot swarm** (`cargo run --release -p net-client --bin swarm`). Each bot is a
  full client with its own UDP socket: it roams, tab-targets the nearest mob,
  plays the rotation through the spell queue and releases when it dies. At the
  end it reads the server metrics and prints PASS or FAIL.
- **Godot client** updated: zone walls and portals from data, mobs, nameplates
  with level and health, player health bar, death and release (R), Tab cycles
  the nearest hostile.

## Results (cloud container, 4 vCPU, server and all bots on the same machine)

| Run | Tick p50 | Tick p99 (worst 10 s) | Down per client | Up per client |
| --- | --- | --- | --- | --- |
| 200 bots roaming and fighting + 10 "humans" (bots at 150 ms, 2% loss) | 1.2 to 1.4 ms | 3.0 ms | 6.7 KB/s (humans 6.6) | 1.3 KB/s |
| 210 bots packed within 30 m (every view full at 80, all 20 Hz) | 1.3 ms | 2.2 ms | 6.2 KB/s | 1.3 KB/s |

Down is UDP payload as measured on each bot's socket; IP and UDP headers add
about 1.5 KB/s. Raw results: `docs/load-tests/phase1-*.json`.

The Phase 0 check still passes on the new server: at 150 ms and 2% loss, queued
casts start 0.3 to 1.3 ms after the caster is ready (target under 5 ms). The
Godot client passes its headless autotest against the new server.

## Notes and deviations

- **renet sends an ack packet on every flush**, so both ends flush only when
  there is news (server: each 20 Hz tick or a combat message; client: each move
  or press). Flushing every millisecond sent 880 packets a second per client.
- **RTT reads 20 to 30 ms high** because acks ride the 20 Hz flushes. The cast
  latency allowance is `min(RTT/2, 100 ms)`, so casts can end up to about 15 ms
  earlier on the server than the bar. Harmless, but worth knowing when reading
  the debug panel.
- **No ECS yet.** Live state is plain structs in hash maps; at 1,000 players the
  tick is still small (see Phase 3). bevy_ecs can come in when content needs it.
- **Collision is boxes, not a navmesh.** Enough for walls, ruins and camps.
  Mobs path in straight lines and slide along walls.

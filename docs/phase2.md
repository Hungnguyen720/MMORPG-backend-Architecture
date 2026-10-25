# Phase 2: the core game loop

Phase 2 turns the combat prototype into a game you can log in to, level in,
and log out of without losing anything.

**Done check: passed.** `scripts/e2e.sh` logs in, levels a character from 1 to
4 across two zones (crossing a portal), logs out and back in, then loots an
item and `kill -9`s the zone process holding the character. After a restart
the item is still there. The full log is in [e2e/phase2-e2e.log](e2e/phase2-e2e.log).

| Step | Result |
|---|---|
| Register, create character, enter Elderwood | ok, content pack downloaded and hash-checked |
| Level 1 to 3 in Elderwood (wolves) | 24 kills, 1 death, 2 potions used, 32 items looted |
| Walk to the portal, hand off to the Hollow Crypt | ok, new zone process, new connect token |
| Level 3 to 4 in the crypt (skeletons) | ok |
| Log out, log back in | zone, level, XP and all 4 item stacks identical |
| Loot a Gravecloth Mantle, `kill -9` the crypt process, restart it | the mantle survived |
| XP after the crash | the last kill's 160 XP was lost (by design, see below) |

## What runs

```
client ──HTTPS──> login ──> world ──> zone processes (control endpoint)
   │                │          │            │
   └──UDP──────────────────────────────────>│ (signed connect token)
                    └──── Postgres ─────────┤
                    └──── Redis ────────────┘
```

| Binary | Job |
|---|---|
| `login` (axum) | Accounts (argon2), sessions in Redis (1 h), per-IP rate limit, character list and creation, `play`, and the content and asset packs |
| `world` (axum) | Picks the layer a character enters, signs the netcode connect token, opens new layers on zone processes, merges thin ones |
| `zone-server` | The simulation. Without `--db` it is the Phase 1 dev server; with `--db` it verifies tokens, loads and saves characters, heartbeats its layers to Redis, and serves `/spawn` and `/drain` for the world service |
| `packc` | Compiles a tenant's YAML into one validated `.pack` |
| `e2e` | The scripted player behind `scripts/e2e.sh` |

`scripts/run-stack.sh <tenant>` starts all three services for one tenant.

## Persistence

Postgres is the source of truth, one database per tenant
(`crates/persistence`): accounts, characters (with a `jsonb` column for
tenant-specific fields), items by content id (so a recompiled pack never
scrambles inventories), and an `economy_log` with idempotency keys for rare
and epic drops.

* **Inventory is saved the moment it changes** (loot, use, equip, destroy),
  in one transaction with any economy log rows. This is what the crash test
  proves.
* **Progress and position are written behind** every 60 s, and immediately on
  level-up, logout and handoff. A crash can cost up to 60 s of XP and
  position, never an item. If that turns out to feel bad, the interval is one
  constant (`WRITE_BEHIND_MS`).
* All writes for one character go through one ordered queue in the zone's
  services thread, so a handoff's save always lands before the destination
  zone loads the character.
* Failed writes retry with backoff; the in-memory state stays authoritative.

The services thread is a small tokio runtime per zone process. The tick never
waits on Postgres, Redis or HTTP.

## Zones, handoff and layers

* A portal is data (`zones.yaml`). Stepping in (out of combat) freezes the
  player, saves them at the destination, asks the world service for a layer,
  and sends the client a `Handoff { addr, token }`. The client reconnects on
  its own; the old zone drops the player without saving again.
* The world service fills the busiest layer under the tenant's
  `layer_soft_cap`, keeps party members on the same layer (up to 120 % of the
  cap), and asks the least busy zone process to `/spawn` a new layer when all
  are full. A Redis lock stops two world instances opening the same layer.
* Every 10 s it looks for a temporary layer whose players fit in the others at
  80 % of the cap and asks its process to `/drain`. The draining zone hands
  its players off ten per tick (same zone, same spot, another layer) and the
  thread exits when empty. Layers given on the command line are permanent.
* Presence (`<tenant>:presence:<character>`) is refreshed by heartbeat. Login
  refuses `play` while a character is still online, so a second login cannot
  duplicate items; a crashed zone's presence expires in 30 s.

## Gameplay added

* **XP and levels:** per-tenant XP table and rate; mob XP scales with level
  difference (grey mobs give nothing); party members within 80 m share it with
  a 10 % per-member bonus. Level-ups restore health and mana and raise stats.
* **Loot:** personal auto-loot rolled per player from the mob's loot table.
* **Inventory:** bag slots from the tenant config, stacks, five equipment slots
  whose stats add to the class's, consumables with cooldowns (potions use the
  same effect system as spells), destroy.
* **Chat:** say (40 m), world (Redis pub/sub across every zone process),
  party, whispers by name.
* **Party:** invite, accept, leave, leader hand-over, up to 5; stored in Redis
  so it survives zone changes.
* **Content pipeline:** `packc` validates every reference and writes
  `MMOPACK` + schema version + body. The login service serves the pack and the
  client downloads it when its hash differs.

## Client

The Godot client now has a login screen, character list and creation, bags
and equipment (B), chat (Enter, `/w /p /world /invite /accept /leave`), an XP
bar, a party list, level-up banner, and seamless zone changes. `--login URL
--autotest` drives the whole flow headlessly and passes.

## Not done yet

* Login and world speak plain HTTP; put them behind TLS (a reverse proxy) before
  anything leaves the dev machine.
* No auction house, trading, mail or quests: none were in scope.
* Combat-feel numbers are unchanged from Phase 0, waiting on Hung's hands-on
  test.

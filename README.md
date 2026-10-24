# MMO engine

A white label engine for a 2.5D tab-target MMORPG, built from the
[architecture doc](https://claude.ai/artifact/1CNGE2VbeBfsMDY6Lt7TCr): a Rust
backend, UDP game traffic, and a Godot client. One engine build runs several
games; each game (a "tenant") is a directory of YAML plus an asset pack.

| Phase | What it added | Write-up |
|---|---|---|
| 0 | Combat feel: casts, GCD, spell queue, prediction, fake lag | below |
| 1 | Movement netcode, delta snapshots, interest grid, mobs, bot swarm | [docs/phase1.md](docs/phase1.md) |
| 2 | Login, world service, Postgres and Redis, XP, loot, inventory, two zones with handoff, chat, party, content packs | [docs/phase2.md](docs/phase2.md) |
| 3 | Second tenant (Ashfall) from data and assets only, layering, 1,000 bots in one zone | [docs/phase3.md](docs/phase3.md) |

Every phase's done check passed; the logs and load test results are in `docs/`.

## Layout

```
crates/
  combat-core/   Shared rules and wire protocol: content schema, GCD, casts,
                 spell queue, movement checks, snapshot codec. Linked by the
                 server, the bots and the client.
  zone-server/   The simulation. One process hosts many zones and layers, one
                 thread and UDP port each. 20 Hz world tick; combat timers fire
                 the moment they fall due.
  services/      login (accounts, sessions, characters, content download),
                 world (layer placement, connect tokens, merging), packc.
  persistence/   Postgres schema and queries.
  net-client/    Engine-agnostic client: transport, prediction, interpolation,
                 login. Also the bots: bot, swarm (load tests) and e2e.
  client-ext/    GDExtension exposing the client to GDScript.
godot/           The Godot 4.5 client. main.gd builds the world and UI in code.
tenants/
  default/       Elderwood Online (the original fantasy tenant)
  ashfall/       Ashfall: Embers of the Pale King (dark fantasy), plus assets/
scripts/         run-stack.sh, e2e.sh, layering-test.sh, build-assets.sh
```

## Install once

- **Rust** (stable) from https://rustup.rs. On Windows, accept the Visual Studio
  C++ build tools prompt.
- **Godot 4.5 or newer**, the standard build (not .NET), from
  https://godotengine.org/download. Tested with 4.5.1.
- For the full stack only: **Postgres 14+** and **Redis 6+**, running locally.

Then build from the repo root:

```sh
cargo build                 # debug: the GDExtension the Godot editor loads
cargo build --release       # release: servers, bots and tools
```

## Quick start: play without logging in

Development mode needs no database: unsigned connects, nothing saved. This is
the setup for the combat-feel test.

```sh
cargo run --release -p zone-server      # Elderwood on UDP 5000
godot --path godot                      # or open godot/project.godot and press F5
```

To join a server on another machine: `godot --path godot -- --server 192.168.1.20:5000`.
Run the client twice to see another player.

| Key | Action |
| --- | --- |
| W A S D | Move |
| Tab, or click | Target |
| 1 to 6 | Abilities (hover for details) |
| Esc | Clear target |
| R | Release to the graveyard after dying |
| B or I | Bags and equipment (left-click: use or equip, right-click: destroy) |
| Enter | Chat: `/w Name hi`, `/p`, `/world`, `/invite Name`, `/accept`, `/leave` |

The panel at the top right adds 0 to 250 ms of latency and 0 to 5% packet
loss. Set it to 150 ms and 2% and play a rotation: press the next ability during
the last 400 ms of a cast or GCD, the button gets a yellow border (queued), and
the cast starts the instant the previous one ends.

## Full stack: accounts, saving, zones and layers

```sh
createuser -s mmo; psql -c "ALTER USER mmo PASSWORD 'mmo'" postgres   # once
scripts/run-stack.sh default            # login :7000, world :7001, zones on UDP 5000+
godot --path godot -- --login http://127.0.0.1:7000
```

The client opens on a login screen: register, create a character, play. It
downloads the tenant's content pack (and asset pack, if any) from the login
service. `run-stack.sh` creates the tenant's database (`mmo_<tenant>`) and
runs migrations on start. Login and world speak plain HTTP; put them behind TLS
before anything leaves your machine.

Run a second game beside the first on the same build:

```sh
scripts/build-assets.sh ashfall
scripts/run-stack.sh ashfall 8000 6000
godot --path godot -- --login http://127.0.0.1:8000
```

## Making a new game

Copy `tenants/default/` and edit the YAML: `tenant.yaml` (name, XP curve, bag
size, starting items, UI colours), `classes.yaml`, `abilities.yaml`,
`mobs.yaml`, `zones.yaml` (walls, spawns, portals) and `items.yaml`. Check it
with:

```sh
cargo run --release -p services --bin packc -- tenants/mygame -o build/mygame.pack
```

`packc` refuses a pack with a broken reference and says where. For models,
icons and colours, put them under `tenants/mygame/assets/tenant/` the way
`tenants/ashfall/assets/` does and run `scripts/build-assets.sh mygame`.

## Tests

```sh
cargo test --workspace                                  # unit tests
scripts/e2e.sh                                          # Phase 2 done check (Postgres + Redis)
cargo run --release -p net-client --bin bot -- --latency 150 --loss 2 --seconds 30
cargo run --release -p net-client --bin swarm -- --bots 200      # against a dev zone
```

## Troubleshooting

- **"Can't open dynamic library" or `CombatClient` not found in Godot**: run
  `cargo build` (debug) from the repo root, then reopen the project. The editor
  loads `target/debug/`.
- **Windows: build fails because the .dll is in use**: close Godot, rebuild, reopen.
- **macOS: the library is blocked**: run `xattr -dr com.apple.quarantine target/` once.
- **Stuck on "Connecting..."**: the zone server isn't running, or a firewall blocks UDP.
- **"that character is still online"**: the zone holding it crashed; presence
  expires within 30 s.
- **"godot-rust compiled against newer Godot version"**: update Godot to 4.5 or newer.

## Not done yet

- Combat-feel numbers are unchanged since Phase 0, waiting on a hands-on test.
- No TLS on login and world, no auction house, trading, mail or quests.
- Ashfall's art is generated placeholders; real Blender exports replace them file for file.

# Phase 0: combat-feel prototype + greybox client

The first build from the [architecture doc](https://claude.ai/artifact/1CNGE2VbeBfsMDY6Lt7TCr). It started as one question to answer with your hands: does tab-target combat feel crisp at 150 ms and 2% packet loss? It is now also a **greybox client**: grey blocks and capsules at real human scale, an isometric camera, mobs that fight back, pawns, and other players, all run by the server. Art drops in later without code changes.

What's in it:

- **Combat**: one caster class with five abilities (hard cast, instant, channel, DoT, off-GCD cooldown), cast bar, GCD swipe, a 400 ms spell queue, and a debug panel that adds 0 to 250 ms of latency and 0 to 5% packet loss.
- **World**: a 120 x 120 m greybox map with a spawn plaza, ruins that block you, a training dummy, and 16 mobs in 7 camps (Ghoul, Ghoul Brute, Bone Archer, Dire Wolf). Mobs aggro, pull their camp mates, chase, attack, leash home, die and respawn.
- **Pawns**: every player brings a Sentinel (melee, holds threat) and a Mystic (heals, then attacks). They follow you, defend you, assist your target once it's in the fight, and obey two commands.
- **You can die**: players have health, take damage, regenerate out of combat, and respawn at the plaza.
- **Multiplayer**: run the client more than once; everyone sees everyone's characters, pawns and fights.

## Layout

```
crates/
  combat-core/   Shared rules: ability data, GCD, casts, channels, spell queue,
                 movement checks, wire protocol. Linked by server AND client.
  zone-server/   Authoritative UDP server (renet). 20 Hz snapshots, combat
                 timers fire the moment they fall due.
  net-client/    Engine-agnostic client: UDP + fake-lag transport, prediction,
                 interpolation. Also builds the headless `bot`.
  client-ext/    GDExtension (godot-rust) exposing `CombatClient` to GDScript.
godot/
  project.godot  Godot 4.5+ project.
  main.gd        Frame loop, input, entity sync, autotest.
  scripts/
    iso_camera.gd      Fixed isometric follow camera, cursor-to-ground ray.
    greybox_world.gd   The level, built from the content pack's layout.
    unit_view.gd       One character: greybox body or real model, animation hooks.
    visuals.gd         Which model replaces which greybox (the art slot).
    hud.gd             Unit/party/target frames, bars, nameplates, log.
  art/characters/  Drop .glb models here (see its README).
  content/abilities.yaml   Class, abilities, auras, dummy, mobs, camps, pawns. Data, not code.
```

## Run it on your machine

### 1. Install once

- **Rust** (stable) from https://rustup.rs. On Windows, accept the Visual Studio C++ build tools prompt.
- **Godot 4.5 or newer**, the standard build (not the .NET one), from https://godotengine.org/download. Tested with 4.5.1.

### 2. Build

From the repo root:

```sh
cargo build                        # debug build: the GDExtension the Godot editor loads
cargo build --release              # release build: server and bot
```

The first build downloads crates and takes a few minutes.

### 3. Start the server

```sh
cargo run --release -p zone-server
```

It prints `zone-server listening on UDP 0.0.0.0:5000`. Leave it running. Every cast is logged; queued casts show how long after the caster was ready they fired, and every 10 s it prints a summary.

### 4. Start the client

Open Godot, click **Import**, pick `godot/project.godot`, then press **F5** (Run Project).

Or from a terminal (with the Godot executable on your PATH):

```sh
godot --path godot
```

To join a server on another machine: `godot --path godot -- --server 192.168.1.20:5000`. Run the project twice to see another player (their cast bar shows on their nameplate). In the editor: **Debug > Customize Run Instances**, enable multiple instances, set 2.

### 5. Play

| Key | Action |
| --- | --- |
| W A S D | Move (relative to the screen) |
| Hold right mouse | Walk toward the cursor (click once to walk to a spot) |
| Tab | Target the nearest enemy, again for the next one |
| Left click | Target whatever is under the cursor (enemies, pawns, players) |
| 1 | Frost Lance, 2.0 s cast |
| 2 | Ice Shard, instant |
| 3 | Arcane Torrent, 3 s channel (breaks if you move) |
| 4 | Searing Brand, damage over time |
| 5 | Arcane Surge, off the GCD, 15 s cooldown |
| F | Pawns: attack my target now |
| G | Pawns: break off and come back (4 s) |
| Esc | Clear target |
| Mouse wheel | Zoom |

Things to try: walk north-west to the ghoul camp, Tab to a ghoul, open with Searing Brand from range and watch the camp come at you. The target frame shows **targeting: Sentinel** once your tank has pulled it off you. Press G to pull your pawns out, F to send them back in. Stand in a fight without pawns (G repeatedly) to see yourself die.

Set the sliders in the top-right panel to **150 ms** and **2%** and play a rotation. Press the next ability during the last 400 ms of a cast or GCD: the button gets a yellow border (queued) and the cast starts the instant the previous one ends.

## What "done" means for Phase 0

From the doc. At 150 ms and 2% loss:

1. A queued rotation shows no gap between casts in the server log (under 5 ms).
2. A cast bar never visibly corrects in normal play.
3. It feels right to you.

You can check the first one without a human:

```sh
cargo run --release -p net-client --bin bot -- --latency 150 --loss 2 --seconds 30
```

In the cloud build of this repo it reported a median gap of about 0.5 ms and a maximum under 1 ms.

## Automated checks

```sh
cargo test                                            # rules + zone AI (aggro, leash, pawns, death)
godot --path godot -- --autotest                      # cast at the dummy for 8 s
godot --path godot -- --autotest pawns --latency 150 --loss 2   # walk to a ghoul camp, fight it with pawns
godot --path godot -- --autotest idle                 # start a second copy alongside: passes if it sees the other player
```

Add `--screenshot out.png --shot-at 12` to any autotest to save a picture.

## Swapping in art

Everything on screen is a placeholder sized like the real thing: characters 1.8 m, doorways 2.2 m, walls 3 m, one grid square 1 m. An orange scale gauge next to the spawn plaza has a 1 m cube, 2 m and 3 m walls and a 1.8 m figure to check imports against.

To replace a character, put a glTF in `godot/art/characters/` named after what it replaces (`player.glb`, `sentinel.glb`, `ghoul.glb`...). It is scaled to the greybox height and driven by the same animation tags (`Idle`, `Walk`, `Attack`, `Cast`, `Channel`, `Hit`, `Death`) on a shared skeleton, so gameplay code never changes. Details in `godot/art/characters/README.md` and `godot/scripts/visuals.gd`. The environment is one node (`greybox_world.gd`) to replace with a real level later.

## Changing abilities

Edit `godot/content/abilities.yaml`, restart the server and the client. Both load the same file; the client warns if its copy differs from the server's. The effect types the engine understands are `damage` and `apply_aura` for now; the full list from the doc gets added as classes need them.

## Troubleshooting

- **"Can't open dynamic library" or `CombatClient` not found in Godot**: run `cargo build` (debug) from the repo root, then reopen the project. The editor loads `target/debug/`.
- **Windows: build fails because the .dll is in use**: close Godot, rebuild, reopen.
- **macOS: the library is blocked**: run `xattr -dr com.apple.quarantine target/` once.
- **Stuck on "Connecting..."**: the server isn't running, or a firewall is blocking UDP port 5000.
- **"godot-rust compiled against newer Godot version"**: your Godot is older than 4.5; update it.

## Shortcuts taken on purpose (fixed in later phases)

- Connections are unauthenticated (netcode "unsecure" mode). Phase 2 adds the login service and signed connect tokens.
- Messages use bincode. The bit-packed codec and delta snapshots come with the Phase 1 bandwidth work.
- The map is flat; the server checks speed and bounds but has no navmesh yet. Ruins block you, but mobs and pawns walk through them.
- NPC AI is simple: nearest-target aggro, a threat table, straight-line chasing, leash home. No pathing, no abilities for mobs, no party between players.
- Every player gets the same two pawns; hiring, the pawn job board and pawn gear come later. Pawn levels and loot don't exist yet.
- No facing requirement for casts, the dummy never dies.

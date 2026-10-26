# Phase 0: combat-feel prototype + greybox client + aimed abilities (Phase 0.5)

The first build from the [architecture doc](https://claude.ai/artifact/1CNGE2VbeBfsMDY6Lt7TCr). It started as one question to answer with your hands: does tab-target combat feel crisp at 150 ms and 2% packet loss? It is now also a **greybox client**: grey blocks and capsules at real human scale, an isometric camera, mobs that fight back, pawns, and other players, all run by the server. Art drops in later without code changes.

What's in it:

- **Combat**: one caster class with five abilities (hard cast, instant, channel, DoT, off-GCD cooldown), cast bar, GCD swipe, a 400 ms spell queue, and a debug panel that adds 0 to 250 ms of latency and 0 to 5% packet loss.
- **World**: a 120 x 120 m greybox map with a spawn plaza, ruins that block you, a training dummy, and 16 mobs in 7 camps (Ghoul, Ghoul Brute, Bone Archer, Dire Wolf). Mobs aggro, pull their camp mates, chase, attack, leash home, die and respawn.
- **Pawns**: every player brings a Sentinel (melee, holds threat) and a Mystic (heals, then attacks). They follow you, defend you, assist your target once it's in the fight, and obey two commands.
- **You can die**: players have health, take damage, regenerate out of combat, and respawn at the plaza.
- **Multiplayer**: run the client more than once; everyone sees everyone's characters, pawns and fights.
- **Aimed abilities (Phase 0.5)**: the hybrid from the [combat design doc](https://claude.ai/artifact/1XaQQgbJKx6Grtohrhd18b): tab-target stays the spine, plus a skillshot (Glacial Spike), a ground rune (Flame Rune) and a dodge (Blink Step). They quick-cast at the mouse cursor; with no cursor aim they fire at your target with aim assist, which is how a tap will work on mobile. The training dummy walks a circle so you have to lead it, and the Ghoul Brute telegraphs a slam you can step out of.

## Layout

```
crates/
  combat-core/   Shared rules: ability data, GCD, casts, channels, spell queue,
                 movement checks, skillshot flight and hit tests (projectile.rs),
                 wire protocol. Linked by server AND client.
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
    combat_fx.gd       Skillshots in flight, ground runes, mob telegraphs.
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
| 6 | Glacial Spike: skillshot at the cursor (0.25 s wind-up, you can keep moving) |
| 7 | Flame Rune: ground circle at the cursor, lands after 0.9 s, hits up to 8 |
| Space (or 8) | Blink Step: 6 m dodge toward where you are walking, else toward the cursor |
| F | Pawns: attack my target now |
| G | Pawns: break off and come back (4 s) |
| Esc | Clear target |
| Mouse wheel | Zoom |

Things to try: walk north-west to the ghoul camp, Tab to a ghoul, open with Searing Brand from range and watch the camp come at you. The target frame shows **targeting: Sentinel** once your tank has pulled it off you. Press G to pull your pawns out, F to send them back in. Stand in a fight without pawns (G repeatedly) to see yourself die.

Aimed abilities: point at the walking training dummy and press 6. Lead it a little; the spike flies 30 m/s. Drop a Flame Rune (7) where it will be, not where it is. Then pull the Ghoul Brute (north-east, next to the ghouls): when a red circle appears under you, the inner disc grows to the edge and the slam lands. Press Space to get out. The debug panel shows **skillshots matching server**: how many of your shots hit (or missed) on the server exactly as they did on your screen.

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

## What "done" means for Phase 0.5 (aimed abilities)

At 150 ms and 2% loss: a skillshot that visibly connects is confirmed by the server at least 95% of the time; a dodge that visibly clears a telegraph is never hit; and it feels right to you. The first is measured by the bot, which leads skillshots and runes at the walking dummy:

```sh
cargo run --release -p net-client --bin bot -- --mode aimed --latency 150 --loss 2 --seconds 90
cargo run --release -p net-client --bin bot -- --mode aimed --latency 150 --loss 2 --seconds 90 --jitter 1.6   # sloppy aim: near misses too
```

In the cloud build, 100% of on-screen hits were confirmed by the server at 150 ms / 2% and at 250 ms / 5%, with clean or deliberately sloppy aim. With the server's rewind turned off it dropped to 90%, which is why the rewind exists. The dodge rule is covered by `cargo test` (`slam_*` tests).

How it stays cheap (details in the design doc):

- **Skillshots are sent once.** The server sends one `ProjectileSpawn`; every client flies it itself. The server sweeps it against nearby mobs and sends one `ProjectileEnd`. No per-tick projectile traffic.
- **Shooting at what you see.** You see mobs about 100 ms in the past plus network delay, so the server tests aimed hits against where each mob was that long ago (a short position trail per mob, capped at 300 ms), and judges hits 0.15 m wider than your screen draws them. In PvE the shooter gets the benefit of the doubt.
- **Dodging what you see.** A telegraph reaches you one way late, and your dodge reaches the server one way late, so the server waits that long (up to 100 ms) before resolving the slam against you.
- **Movement stays client-owned.** A dash just lets the server's speed check allow the extra distance for a moment.

## Automated checks

```sh
cargo test                                            # rules + zone AI (aggro, leash, pawns, death) + aimed hits, slams, dashes
godot --path godot -- --autotest                      # cast at the dummy for 8 s
godot --path godot -- --autotest pawns --latency 150 --loss 2   # walk to a ghoul camp, fight it with pawns
godot --path godot -- --autotest idle                 # start a second copy alongside: passes if it sees the other player
godot --path godot -- --autotest aimed --latency 150 --loss 2   # skillshots, runes and dodges at the walking dummy
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
- Aimed abilities on PC only: quick cast at the cursor, no aim preview yet. Mobile drag-to-aim is the next pass once the PC feel is approved.
- Pawns don't dodge telegraphs yet, and don't use aimed abilities. Only the Ghoul Brute has a telegraph.
- Server hit tests scan every mob in the zone; a spatial grid replaces that when zones hold hundreds of mobs.
- Mobs and the dummy are round 0.5 m bodies scaled by their size; no per-model hitboxes.

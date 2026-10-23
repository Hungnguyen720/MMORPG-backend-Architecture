# Phase 0: combat-feel prototype

The first build from the [architecture doc](https://claude.ai/artifact/1CNGE2VbeBfsMDY6Lt7TCr). The point is to answer one question with your hands: does tab-target combat feel crisp at 150 ms and 2% packet loss?

What's in it: one caster class with five abilities (hard cast, instant, channel, DoT, off-GCD cooldown), a training dummy, cast bar, GCD swipe, a 400 ms spell queue, WASD movement, and a debug panel that adds 0 to 250 ms of latency and 0 to 5% packet loss.

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
  project.godot  Godot 4.5+ project. main.gd builds the arena and UI in code.
  content/abilities.yaml   The class, abilities, auras and dummy. Data, not code.
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

To join a server on another machine: `godot --path godot -- --server 192.168.1.20:5000`. Run the project twice to see another player (their cast bar shows on their nameplate).

### 5. Play

| Key | Action |
| --- | --- |
| W A S D | Move |
| Tab, or click the dummy | Target |
| 1 | Frost Lance, 2.0 s cast |
| 2 | Ice Shard, instant |
| 3 | Arcane Torrent, 3 s channel (breaks if you move) |
| 4 | Searing Brand, damage over time |
| 5 | Arcane Surge, off the GCD, 15 s cooldown |
| Esc | Clear target |

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

In the cloud build of this repo it reported a median gap of about 0.3 ms and a maximum under 1 ms.

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
- The arena is a flat square; the server checks speed and bounds but has no navmesh yet.
- No facing requirement for casts, no NPC AI, the dummy never dies.

# Character models

Drop a glTF here named after what it replaces and it is used instead of the
greybox shape, with no code changes:

| File | Replaces |
| --- | --- |
| `player.glb` | every player character |
| `sentinel.glb`, `mystic.glb`, `ranger.glb` | pawns (ids from `content/abilities.yaml`) |
| `ghoul.glb`, `ghoul_brute.glb`, `bone_archer.glb`, `dire_wolf.glb` | mobs |
| `dummy.glb` | the training dummy |

Rules (see `scripts/visuals.gd`):

- Forward is +Z (the glTF default). Origin at the feet.
- The model is scaled so it stands 1.8 m tall (times the mob's `scale`), so
  any export scale works and gameplay reach never changes.
- One shared humanoid skeleton and clip set for everything: `Idle`, `Walk`,
  `Attack`, `Cast`, `Channel`, `Hit`, `Death`. Missing clips fall back to
  `Idle`. Different clip names go in `Visuals.OVERRIDES`.
- Open the project in the Godot editor once after adding files so it imports
  them.

class_name Visuals
extends RefCounted
## Where real art plugs in. Every unit starts as a greybox shape at human
## scale; a model replaces it when one is found here.
##
## Keys: "player", "dummy", or a mob / pawn id from content/abilities.yaml
## ("ghoul", "sentinel", ...).
##
## Drop-in convention: put a glTF at res://art/characters/<key>.glb (for
## example art/characters/player.glb) and it is used automatically. Or add an
## entry to OVERRIDES for a different path, height or animation names.
##
## Every character is expected to share one skeleton and one animation set
## (Mixamo-style humanoid), so the same tags drive every model:
##   idle, walk, attack, cast, channel, hit, death
## A model whose clips use other names maps them in "anims".

const ART_DIR := "res://art/characters/"

## Height in metres a model is scaled to (before the content pack's `scale`).
## Matches the greybox capsule, so swapping art never changes gameplay reach.
const HUMAN_HEIGHT := 1.8

const DEFAULT_ANIMS := {
	"idle": "Idle",
	"walk": "Walk",
	"attack": "Attack",
	"cast": "Cast",
	"channel": "Channel",
	"hit": "Hit",
	"death": "Death",
}

const OVERRIDES := {
	# "sentinel": {
	# 	"scene": "res://art/characters/knight_v2.glb",
	# 	"height": 1.85,
	# 	"anims": {"attack": "Sword_Slash"},
	# },
}


## {scene: PackedScene, height: float, anims: Dictionary}, or {} for greybox.
static func lookup(key: String) -> Dictionary:
	var entry: Dictionary = OVERRIDES.get(key, {})
	var path: String = entry.get("scene", ART_DIR + key + ".glb")
	if key == "" or not ResourceLoader.exists(path):
		return {}
	var scene := load(path) as PackedScene
	if scene == null:
		return {}
	var anims := DEFAULT_ANIMS.duplicate()
	anims.merge(entry.get("anims", {}), true)
	return {"scene": scene, "height": entry.get("height", HUMAN_HEIGHT), "anims": anims}

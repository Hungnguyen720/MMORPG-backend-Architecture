class_name UnitView
extends Node3D
## One character on screen: a greybox body at human scale, or a real model
## from Visuals in the ModelSlot. Gameplay code only talks to this API
## (play_action, set_casting, set_dead), so swapping art changes nothing else.
##
## Convention: +Z is forward (glTF), and rotation.y is the server's facing.

const WALK_SPEED_THRESHOLD := 0.4

var key := ""
var tint := Color.WHITE
var unit_scale := 1.0

var _rig: Node3D           # greybox body parts, animated procedurally
var _model_slot: Node3D    # real model goes here
var _anim: AnimationPlayer
var _anims := {}
var _body_mats: Array[StandardMaterial3D] = []
var _hands: MeshInstance3D
var _hands_mat: StandardMaterial3D
var _last_pos := Vector3.ZERO
var _speed := 0.0
var _walk_phase := 0.0
var _dead := false
var _casting := false
var _one_shot := 0.0       # seconds left on an attack / hit clip
var _flash := 0.0
var _lunge: Tween


## `shape` picks the greybox silhouette: "humanoid", "quadruped" or "dummy".
func setup(p_key: String, shape: String, color: Color, p_scale: float) -> void:
	key = p_key
	tint = color
	unit_scale = p_scale
	_rig = Node3D.new()
	_rig.name = "Greybox"
	add_child(_rig)
	_model_slot = Node3D.new()
	_model_slot.name = "ModelSlot"
	add_child(_model_slot)
	match shape:
		"quadruped": _build_quadruped()
		"dummy": _build_dummy()
		_: _build_humanoid()
	_rig.scale = Vector3.ONE * unit_scale
	_try_model()


## Top of the head in metres, for nameplates and floating text.
func head_height() -> float:
	return (1.15 if _is_quadruped() else 1.95) * unit_scale


func _is_quadruped() -> bool:
	return _rig.has_meta("quadruped")


func _physics_process(delta: float) -> void:
	# Estimated from how far the node moved; works for interpolated remotes
	# and the locally predicted player alike.
	var moved := Vector2(position.x - _last_pos.x, position.z - _last_pos.z).length()
	_last_pos = position
	if delta > 0:
		_speed = lerp(_speed, moved / delta, 0.35)


func _process(delta: float) -> void:
	_one_shot = max(0.0, _one_shot - delta)
	_flash = max(0.0, _flash - delta)
	if _anim != null:
		_update_model_anim()
	else:
		_update_greybox(delta)


# --------------------------------------------------------------------------
# Actions
# --------------------------------------------------------------------------

## "attack", "cast" or "hit". Casts and channels use set_casting instead.
func play_action(tag: String) -> void:
	if _dead:
		return
	match tag:
		"attack":
			_one_shot = 0.45
			if _anim == null:
				_lunge_forward()
			else:
				_play(tag)
		"hit":
			_flash = 0.12
			if _anim != null and _one_shot <= 0.0:
				_one_shot = 0.25
				_play(tag)


func set_casting(active: bool, color: Color = Color.WHITE, channel := false) -> void:
	_casting = active
	if _hands != null:
		_hands.visible = active and _anim == null
		_hands_mat.albedo_color = color
		_hands_mat.emission = color
	if active and _anim != null:
		_play("channel" if channel else "cast")


func set_dead(dead: bool) -> void:
	if dead == _dead:
		return
	_dead = dead
	set_casting(false)
	if _anim != null:
		_play("death" if dead else "idle")
		return
	if dead:
		var tw := create_tween()
		var axis := "rotation:z" if _is_quadruped() else "rotation:x"
		tw.tween_property(_rig, axis, -PI / 2.0, 0.35).set_trans(Tween.TRANS_QUAD).set_ease(Tween.EASE_IN)
		tw.parallel().tween_property(_rig, "position:y", 0.3 * unit_scale, 0.35)
		for m in _body_mats:
			m.albedo_color = m.albedo_color.darkened(0.5)
	else:
		_rig.rotation = Vector3.ZERO
		_rig.position = Vector3.ZERO
		for m in _body_mats:
			m.albedo_color = m.get_meta("base")


# --------------------------------------------------------------------------
# Real model
# --------------------------------------------------------------------------

func _try_model() -> void:
	var v := Visuals.lookup(key)
	if v.is_empty():
		return
	var model: Node3D = v.scene.instantiate()
	_model_slot.add_child(model)
	# Normalise height so every model stands as tall as its greybox.
	var aabb := _mesh_aabb(model)
	if aabb.size.y > 0.01:
		var k: float = v.height * unit_scale / aabb.size.y
		model.scale = Vector3.ONE * k
		model.position.y = -aabb.position.y * k
	_anim = _find_anim_player(model)
	_anims = v.anims
	_rig.visible = false
	_play("idle")


func _update_model_anim() -> void:
	if _dead or _one_shot > 0.0 or _casting:
		return
	_play("walk" if _speed > WALK_SPEED_THRESHOLD else "idle")


func _play(tag: String) -> void:
	if _anim == null:
		return
	var clip: String = _anims.get(tag, "")
	if clip == "" or not _anim.has_animation(clip):
		# Missing clips fall back to idle rather than freezing.
		clip = _anims.get("idle", "")
		if not _anim.has_animation(clip):
			return
	if _anim.current_animation != clip:
		_anim.play(clip, 0.15)


static func _find_anim_player(n: Node) -> AnimationPlayer:
	if n is AnimationPlayer:
		return n
	for c in n.get_children():
		var a := _find_anim_player(c)
		if a != null:
			return a
	return null


static func _mesh_aabb(root: Node3D) -> AABB:
	var out := AABB()
	var first := true
	var stack: Array[Node] = [root]
	while not stack.is_empty():
		var n: Node = stack.pop_back()
		stack.append_array(n.get_children())
		if n is MeshInstance3D and n.mesh != null:
			var b: AABB = _relative(root, n) * n.mesh.get_aabb()
			out = b if first else out.merge(b)
			first = false
	return out


static func _relative(root: Node3D, n: Node3D) -> Transform3D:
	# Transform of `n` in `root`'s space, excluding root's own transform.
	var t := Transform3D.IDENTITY
	var cur: Node = n
	while cur != null and cur != root:
		if cur is Node3D:
			t = (cur as Node3D).transform * t
		cur = cur.get_parent()
	return t


# --------------------------------------------------------------------------
# Greybox
# --------------------------------------------------------------------------

func _update_greybox(delta: float) -> void:
	if _dead:
		return
	if _speed > WALK_SPEED_THRESHOLD:
		_walk_phase += delta * (6.0 + _speed) / unit_scale
		_rig.position.y = abs(sin(_walk_phase)) * 0.07 * unit_scale
		_rig.rotation.x = 0.08
	else:
		_rig.position.y = lerp(_rig.position.y, 0.0, 0.3)
		_rig.rotation.x = lerp(_rig.rotation.x, 0.0, 0.3)
	if _casting and _hands != null:
		_hands.scale = Vector3.ONE * (1.0 + 0.15 * sin(Time.get_ticks_msec() / 90.0))
	var f := _flash / 0.12
	for m in _body_mats:
		m.emission_enabled = f > 0.0
		m.emission = Color.WHITE
		m.emission_energy_multiplier = f * 1.5


func _lunge_forward() -> void:
	if _lunge != null:
		_lunge.kill()
	_lunge = create_tween()
	_lunge.tween_property(_rig, "position:z", 0.35, 0.09).set_trans(Tween.TRANS_QUAD)
	_lunge.tween_property(_rig, "position:z", 0.0, 0.25).set_trans(Tween.TRANS_QUAD)


func _mat(c: Color) -> StandardMaterial3D:
	var m := StandardMaterial3D.new()
	m.albedo_color = c
	m.roughness = 0.9
	m.set_meta("base", c)
	_body_mats.append(m)
	return m


func _part(mesh: Mesh, pos: Vector3, c: Color, rot := Vector3.ZERO) -> MeshInstance3D:
	var mi := MeshInstance3D.new()
	mi.mesh = mesh
	mi.position = pos
	mi.rotation = rot
	mi.material_override = _mat(c)
	_rig.add_child(mi)
	return mi


func _box(size: Vector3) -> BoxMesh:
	var b := BoxMesh.new()
	b.size = size
	return b


func _capsule(r: float, h: float) -> CapsuleMesh:
	var c := CapsuleMesh.new()
	c.radius = r
	c.height = h
	return c


func _sphere(r: float) -> SphereMesh:
	var s := SphereMesh.new()
	s.radius = r
	s.height = r * 2
	return s


## 1.8 m tall, shoulders ~0.5 m: the size real characters will be.
func _build_humanoid() -> void:
	var dark := tint.darkened(0.35)
	_part(_capsule(0.17, 0.85), Vector3(0, 0.43, 0), dark)                  # legs
	_part(_capsule(0.25, 0.75), Vector3(0, 1.12, 0), tint)                  # torso
	_part(_sphere(0.14), Vector3(0, 1.66, 0), tint.lightened(0.25))         # head
	_part(_box(Vector3(0.2, 0.08, 0.12)), Vector3(0, 1.68, 0.12), Color(0.1, 0.1, 0.12))  # visor = facing
	_part(_capsule(0.07, 0.62), Vector3(-0.31, 1.1, 0), dark)               # arms
	_part(_capsule(0.07, 0.62), Vector3(0.31, 1.1, 0), dark)
	_add_prop()
	_add_hands(Vector3(0, 1.15, 0.38))


## Role props so pawns and mobs read at a glance with no art.
func _add_prop() -> void:
	match key:
		"sentinel", "ghoul_brute":
			_part(_box(Vector3(0.08, 0.7, 0.5)), Vector3(-0.42, 1.05, 0.12), Color(0.55, 0.55, 0.6))  # shield
			_part(_box(Vector3(0.06, 0.06, 0.9)), Vector3(0.36, 0.95, 0.35), Color(0.75, 0.75, 0.8))   # blade
		"mystic":
			_part(_capsule(0.025, 1.7), Vector3(0.36, 0.9, 0.1), Color(0.45, 0.32, 0.2))               # staff
			_part(_sphere(0.09), Vector3(0.36, 1.78, 0.1), Color(0.5, 1, 0.75))
		"ranger", "bone_archer":
			_part(_capsule(0.02, 1.2), Vector3(-0.36, 1.1, 0.15), Color(0.45, 0.32, 0.2), Vector3(0.25, 0, 0))  # bow
		"ghoul":
			_part(_box(Vector3(0.05, 0.05, 0.35)), Vector3(0.33, 0.8, 0.25), Color(0.85, 0.8, 0.7))     # claws
			_part(_box(Vector3(0.05, 0.05, 0.35)), Vector3(-0.33, 0.8, 0.25), Color(0.85, 0.8, 0.7))
		"player":
			_part(_box(Vector3(0.34, 0.5, 0.06)), Vector3(0, 1.2, -0.27), Color(0.25, 0.2, 0.5))        # spellbook pack


func _add_hands(pos: Vector3) -> void:
	_hands = MeshInstance3D.new()
	_hands.mesh = _sphere(0.11)
	_hands.position = pos
	_hands_mat = StandardMaterial3D.new()
	_hands_mat.shading_mode = BaseMaterial3D.SHADING_MODE_UNSHADED
	_hands_mat.emission_enabled = true
	_hands.material_override = _hands_mat
	_hands.visible = false
	_rig.add_child(_hands)


func _build_quadruped() -> void:
	_rig.set_meta("quadruped", true)
	var dark := tint.darkened(0.3)
	_part(_capsule(0.28, 1.5), Vector3(0, 0.75, 0), tint, Vector3(PI / 2, 0, 0))   # body
	_part(_box(Vector3(0.32, 0.3, 0.45)), Vector3(0, 0.95, 0.85), tint.lightened(0.15))  # head
	_part(_box(Vector3(0.12, 0.1, 0.25)), Vector3(0, 0.88, 1.15), Color(0.1, 0.1, 0.1))  # snout
	for x in [-0.18, 0.18]:
		for z in [-0.45, 0.45]:
			_part(_capsule(0.07, 0.6), Vector3(x, 0.3, z), dark)
	_add_hands(Vector3(0, 0.95, 1.0))


func _build_dummy() -> void:
	var wood := Color(0.55, 0.4, 0.25)
	_part(_box(Vector3(0.3, 1.8, 0.3)), Vector3(0, 0.9, 0), wood)
	_part(_box(Vector3(1.6, 0.2, 0.2)), Vector3(0, 1.4, 0), wood)
	_part(_sphere(0.3), Vector3(0, 2.05, 0), Color(0.85, 0.75, 0.5))

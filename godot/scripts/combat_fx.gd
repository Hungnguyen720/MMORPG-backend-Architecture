class_name CombatFx
extends Node3D
## Draws aimed combat on the ground: skillshots in flight and the circles of
## ground abilities and mob telegraphs. Everything here is cosmetic; the
## server decides hits. Positions come from CombatClient each frame, already
## predicted for our own casts, so a press shows on the same frame.
##
## Readability rule from the combat design: enemy telegraphs draw on top and
## brightest, our own effects next, other players' faded.

const SHOT_HEIGHT := 0.9
const DANGER := Color(1.0, 0.18, 0.12)
const MINE := Color(1.0, 0.55, 0.2)
const ALLY := Color(0.9, 0.75, 0.5)

var _shots: Array[MeshInstance3D] = []
var _markers: Array[Dictionary] = []


func update(client: CombatClient) -> void:
	_draw_shots(client.shots())
	_draw_markers(client.markers())


func _draw_shots(shots: Array) -> void:
	while _shots.size() < shots.size():
		var m := MeshInstance3D.new()
		var cap := CapsuleMesh.new()
		cap.radius = 0.24
		cap.height = 1.6
		m.mesh = cap
		m.material_override = _glow(Color.WHITE)
		m.cast_shadow = GeometryInstance3D.SHADOW_CASTING_SETTING_OFF
		add_child(m)
		_shots.append(m)
	for i in _shots.size():
		var m := _shots[i]
		m.visible = i < shots.size()
		if not m.visible:
			continue
		var s: Dictionary = shots[i]
		var dir: Vector3 = s.dir
		m.position = (s.pos as Vector3) + Vector3(0, SHOT_HEIGHT, 0)
		# Lay the capsule (long along +Y) along the flight direction.
		m.basis = Basis(Vector3.UP.cross(dir).normalized(), PI / 2.0) if dir.length() > 0.01 else Basis()
		var c := Color(s.color)
		if not s.mine:
			c.a = 0.55
		(m.material_override as StandardMaterial3D).albedo_color = c


## Each marker is an outer ring showing the area plus an inner disc that
## grows to the edge as the hit approaches: when they meet, it lands.
func _draw_markers(markers: Array) -> void:
	while _markers.size() < markers.size():
		var area := _disc(0.02)
		var fill := _disc(0.03)
		_markers.append({"area": area, "fill": fill})
	for i in _markers.size():
		var mk: Dictionary = _markers[i]
		var show := i < markers.size()
		mk.area.visible = show
		mk.fill.visible = show
		if not show:
			continue
		var m: Dictionary = markers[i]
		var base: Color = DANGER if m.kind == "danger" else (MINE if m.kind == "mine" else ALLY)
		var r: float = m.radius
		var p: Vector3 = m.pos
		mk.area.position = Vector3(p.x, mk.area.position.y, p.z)
		mk.area.scale = Vector3(r, 1, r)
		mk.fill.position = Vector3(p.x, mk.fill.position.y, p.z)
		var grow: float = max(m.progress, 0.05)
		mk.fill.scale = Vector3(r * grow, 1, r * grow)
		var faint := 0.5 if m.kind == "ally" else 1.0
		var area_c := base
		area_c.a = 0.22 * faint
		var fill_c := base
		fill_c.a = (0.75 if m.landed else 0.35) * faint
		(mk.area.material_override as StandardMaterial3D).albedo_color = area_c
		(mk.fill.material_override as StandardMaterial3D).albedo_color = fill_c
		# Telegraphs sort above everything else on the ground.
		var prio := 2 if m.kind == "danger" else (1 if m.kind == "mine" else 0)
		(mk.area.material_override as StandardMaterial3D).render_priority = prio
		(mk.fill.material_override as StandardMaterial3D).render_priority = prio


func _disc(height: float) -> MeshInstance3D:
	var m := MeshInstance3D.new()
	var cyl := CylinderMesh.new()
	cyl.top_radius = 1.0
	cyl.bottom_radius = 1.0
	cyl.height = 0.01
	cyl.radial_segments = 48
	m.mesh = cyl
	m.position.y = height
	var mat := StandardMaterial3D.new()
	mat.shading_mode = BaseMaterial3D.SHADING_MODE_UNSHADED
	mat.transparency = BaseMaterial3D.TRANSPARENCY_ALPHA
	mat.no_depth_test = false
	m.material_override = mat
	m.cast_shadow = GeometryInstance3D.SHADOW_CASTING_SETTING_OFF
	add_child(m)
	return m


func _glow(c: Color) -> StandardMaterial3D:
	var m := StandardMaterial3D.new()
	m.shading_mode = BaseMaterial3D.SHADING_MODE_UNSHADED
	m.transparency = BaseMaterial3D.TRANSPARENCY_ALPHA
	m.albedo_color = c
	return m

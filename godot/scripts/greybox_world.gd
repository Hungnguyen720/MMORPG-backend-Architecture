class_name GreyboxWorld
extends Node3D
## The greybox level: flat ground with a metre grid, perimeter walls, ruins
## that block the player, and marked areas for spawn and the mob camps.
## Built in code from the content pack's layout so moving a camp in the YAML
## moves its marker too. Real environment art replaces this node later.
##
## Scale rules (keep them when art arrives): 1 grid square = 1 m, characters
## are 1.8 m, doorways 2.2 m clear, walls 3 m, one storey 4 m.

const WALL := Color(0.42, 0.43, 0.46)
const WALL_DARK := Color(0.32, 0.33, 0.36)
const ORANGE := Color(0.85, 0.5, 0.2)   # greybox "interactive / scale reference" colour

var half_extent := 60.0


func build(layout: Dictionary, p_half_extent: float) -> void:
	half_extent = p_half_extent
	_environment()
	_ground()
	_perimeter()
	var spawn: Vector3 = layout.get("spawn", Vector3.ZERO)
	_disc(spawn, 7.0, Color(0.24, 0.25, 0.28), 0.01)
	_label(spawn + Vector3(0, 0.05, 6), "SPAWN", 0.02)
	for camp in layout.get("camps", []):
		_camp(camp.pos, camp.mob)
	_ruins()
	_scale_gauge(spawn + Vector3(9, 0, 3))


func _environment() -> void:
	var env := WorldEnvironment.new()
	var e := Environment.new()
	e.background_mode = Environment.BG_COLOR
	e.background_color = Color(0.11, 0.12, 0.14)
	e.ambient_light_source = Environment.AMBIENT_SOURCE_COLOR
	e.ambient_light_color = Color(0.6, 0.63, 0.7)
	e.ambient_light_energy = 0.35
	e.tonemap_mode = Environment.TONE_MAPPER_FILMIC
	e.fog_enabled = true
	e.fog_light_color = Color(0.11, 0.12, 0.14)
	e.fog_density = 0.004
	env.environment = e
	add_child(env)

	var sun := DirectionalLight3D.new()
	sun.rotation_degrees = Vector3(-58, 30, 0)
	sun.light_energy = 0.85
	sun.shadow_enabled = true
	sun.directional_shadow_max_distance = 80.0
	add_child(sun)


func _ground() -> void:
	var ground := MeshInstance3D.new()
	var pm := PlaneMesh.new()
	pm.size = Vector2(half_extent * 2 + 20, half_extent * 2 + 20)
	ground.mesh = pm
	var sh := Shader.new()
	sh.code = """
shader_type spatial;
varying vec3 world;
void vertex() { world = (MODEL_MATRIX * vec4(VERTEX, 1.0)).xyz; }
float grid(vec2 p, float size) {
	vec2 g = abs(fract(p / size - 0.5) - 0.5) / fwidth(p / size);
	return 1.0 - min(min(g.x, g.y), 1.0);
}
void fragment() {
	vec3 base = vec3(0.30, 0.31, 0.32);
	float minor = grid(world.xz, 1.0) * 0.35;
	float major = grid(world.xz, 10.0);
	vec3 c = mix(base, vec3(0.38, 0.39, 0.41), minor);
	c = mix(c, vec3(0.46, 0.47, 0.5), major);
	ALBEDO = c;
	ROUGHNESS = 0.95;
}
"""
	var m := ShaderMaterial.new()
	m.shader = sh
	ground.material_override = m
	add_child(ground)


func _perimeter() -> void:
	var h := half_extent + 0.5
	var len := half_extent * 2 + 2
	block(Vector3(0, 1.5, -h), Vector3(len, 3, 1), WALL_DARK)
	block(Vector3(0, 1.5, h), Vector3(len, 3, 1), WALL_DARK)
	block(Vector3(-h, 1.5, 0), Vector3(1, 3, len), WALL_DARK)
	block(Vector3(h, 1.5, 0), Vector3(1, 3, len), WALL_DARK)


func _camp(pos: Vector3, mob: String) -> void:
	_disc(pos, 7.0, Color(0.33, 0.27, 0.23), 0.012)
	_label(pos + Vector3(0, 0.05, 7.6), mob.to_upper().replace("_", " "), 0.02)
	# A few low rocks around the edge (no collision: NPCs have no navmesh yet).
	for i in 5:
		var a := float(i) / 5.0 * TAU + pos.x
		var p := pos + Vector3(cos(a), 0, sin(a)) * 7.5
		_deco(p + Vector3(0, 0.3, 0), Vector3(1.2, 0.6, 0.9), Color(0.36, 0.33, 0.3))


## Ruins away from the camp lanes. They block the player; NPCs walk through
## them until the server has a navmesh.
func _ruins() -> void:
	# Broken L-shaped walls, west of spawn.
	block(Vector3(-16, 1.5, 10), Vector3(10, 3, 0.8), WALL)
	block(Vector3(-20.6, 1.5, 15), Vector3(0.8, 3, 10), WALL)
	block(Vector3(-12, 0.9, 15), Vector3(3, 1.8, 0.8), WALL)
	# Doorway: two posts and a lintel, 2.2 m clear.
	block(Vector3(-1.6, 1.4, 18), Vector3(0.8, 2.8, 0.8), WALL)
	block(Vector3(1.6, 1.4, 18), Vector3(0.8, 2.8, 0.8), WALL)
	_deco(Vector3(0, 2.6, 18), Vector3(4, 0.4, 0.8), WALL)
	# Pillar row to the east, one storey tall.
	for i in 5:
		block(Vector3(14 + i * 3.5, 2, 10), Vector3(1, 4, 1), WALL)
	# A collapsed tower near the north road.
	block(Vector3(-10, 2, -32), Vector3(5, 4, 5), WALL_DARK)
	block(Vector3(12, 1, -36), Vector3(3, 2, 6), WALL)
	# Trees (trunk collides, canopy is decoration).
	var trees := [Vector3(-40, 0, 0), Vector3(-44, 0, 10), Vector3(42, 0, 4), Vector3(46, 0, -8),
		Vector3(-30, 0, 46), Vector3(22, 0, 44), Vector3(-48, 0, -40), Vector3(48, 0, 46)]
	for t in trees:
		block(t + Vector3(0, 1.5, 0), Vector3(0.6, 3, 0.6), Color(0.35, 0.28, 0.22))
		var canopy := MeshInstance3D.new()
		var cyl := CylinderMesh.new()
		cyl.top_radius = 0.0
		cyl.bottom_radius = 2.2
		cyl.height = 4.0
		canopy.mesh = cyl
		canopy.position = t + Vector3(0, 4.5, 0)
		canopy.material_override = _lit(Color(0.27, 0.36, 0.28))
		add_child(canopy)


## Reference objects next to spawn: 1 m cube, 2 m and 3 m walls, a doorway
## and a 1.8 m figure, so imported art can be checked against them.
func _scale_gauge(at: Vector3) -> void:
	block(at, Vector3(1, 1, 1), ORANGE, true)
	block(at + Vector3(2, 0, 0), Vector3(1, 2, 0.3), ORANGE, true)
	block(at + Vector3(4, 0, 0), Vector3(1, 3, 0.3), ORANGE, true)
	var post := _capsule_marker(at + Vector3(6, 0, 0))
	add_child(post)
	_label(at + Vector3(3, 0.05, 1.6), "1 m   2 m   3 m   1.8 m", 0.025)


func _capsule_marker(at: Vector3) -> MeshInstance3D:
	var mi := MeshInstance3D.new()
	var c := CapsuleMesh.new()
	c.radius = 0.3
	c.height = 1.8
	mi.mesh = c
	mi.position = at + Vector3(0, 0.9, 0)
	mi.material_override = _lit(ORANGE.lightened(0.2))
	return mi


## A solid block the player collides with. `grounded` puts its base at y = 0.
func block(pos: Vector3, size: Vector3, c: Color, grounded := false) -> StaticBody3D:
	if grounded:
		pos.y += size.y / 2.0
	var body := StaticBody3D.new()
	body.position = pos
	var shape := CollisionShape3D.new()
	var box := BoxShape3D.new()
	box.size = size
	shape.shape = box
	body.add_child(shape)
	var mi := MeshInstance3D.new()
	var bm := BoxMesh.new()
	bm.size = size
	mi.mesh = bm
	mi.material_override = _lit(c)
	body.add_child(mi)
	add_child(body)
	return body


func _deco(pos: Vector3, size: Vector3, c: Color) -> void:
	var mi := MeshInstance3D.new()
	var bm := BoxMesh.new()
	bm.size = size
	mi.mesh = bm
	mi.position = pos
	mi.material_override = _lit(c)
	add_child(mi)


func _disc(pos: Vector3, r: float, c: Color, lift: float) -> void:
	var mi := MeshInstance3D.new()
	var cyl := CylinderMesh.new()
	cyl.top_radius = r
	cyl.bottom_radius = r
	cyl.height = 0.02
	cyl.radial_segments = 48
	mi.mesh = cyl
	mi.position = Vector3(pos.x, lift, pos.z)
	mi.material_override = _lit(c)
	mi.cast_shadow = GeometryInstance3D.SHADOW_CASTING_SETTING_OFF
	add_child(mi)


func _label(pos: Vector3, text: String, pixel: float) -> void:
	var l := Label3D.new()
	l.text = text
	l.position = pos
	l.rotation_degrees = Vector3(-90, IsoCamera.YAW_DEG, 0)  # readable from the camera
	l.pixel_size = pixel
	l.font_size = 64
	l.modulate = Color(1, 1, 1, 0.16)
	l.outline_size = 0
	add_child(l)


func _lit(c: Color) -> StandardMaterial3D:
	var m := StandardMaterial3D.new()
	m.albedo_color = c
	m.roughness = 0.9
	return m

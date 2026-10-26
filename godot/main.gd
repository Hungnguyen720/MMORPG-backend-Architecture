extends Node3D
## Greybox client. Grey blocks and capsules at real human scale, an
## isometric camera, tab-target combat, pawns and other players, all driven
## by the zone server. Art drops in later through Visuals / UnitView without
## touching this file.
##
## Networking, prediction and the combat rules live in Rust (CombatClient,
## from the GDExtension). Scripts here only draw and read input:
##   scripts/iso_camera.gd     fixed isometric follow camera, cursor-to-ground ray
##   scripts/greybox_world.gd  the level (ground grid, ruins, camp markers)
##   scripts/unit_view.gd      one character: greybox body or real model
##   scripts/visuals.gd        which model replaces which greybox
##   scripts/hud.gd            frames, bars, nameplates, log, debug panel
##   scripts/combat_fx.gd      skillshots, ground runes and mob telegraphs
##
## Aimed abilities (skillshots, ground runes, the dodge) quick-cast at the
## mouse cursor: press the key and it fires where you point. With nothing
## under the cursor they fall back to your target. Space dodges toward where
## you are walking, or toward the cursor when standing still.
##
## Command-line options (after `--`):
##   --server HOST:PORT   default 127.0.0.1:5000
##   --latency MS         initial fake latency (RTT), default 0
##   --loss PERCENT       initial fake loss, default 0
##   --autotest [MODE]    play by itself, print a summary, quit. MODE is
##                        "dummy" (default: cast at the dummy for 8 s),
##                        "pawns" (walk to a ghoul camp, fight it with pawns) or
##                        "aimed" (skillshots, runes and dodges at the dummy)
##   --screenshot PATH    with --autotest, save a PNG of the window
##   --shot-at SECONDS    when to take it (default 1 for dummy, 14 for pawns)

const CONTENT_PATH := "res://content/abilities.yaml"
const TAB_RANGE := 45.0
const CLICK_PICK_PX := 48.0
const ARRIVE_DIST := 0.4
const DASH_SECONDS := 0.18
## After an aimed press, keep facing the aim point this long.
const AIM_FACE_SECONDS := 0.5

var client: CombatClient
var server_host := "127.0.0.1"
var server_port := 5000
var move_speed := 7.0
var facing := 0.0
var half_extent := 60.0

var world: GreyboxWorld
var camera: IsoCamera
var hud: Hud
var player: CharacterBody3D
var player_view: UnitView
var target_ring: MeshInstance3D
var target_ring_mat: StandardMaterial3D
var move_marker: MeshInstance3D
var fx: CombatFx
var abilities: Array = []   # ability_info per slot
var dodge_slot := -1
var dash_vel := Vector3.ZERO
var dash_left := 0.0
var aim_face = null         # Vector3 we turned to for an aimed cast, or null
var aim_face_left := 0.0
var views := {}          # entity id -> UnitView (everyone but us)
var entities: Array = [] # this frame's snapshot from CombatClient
var by_id := {}
var was_dead := false
var world_built := false

## Click-to-move destination, or null. WASD clears it.
var move_goal = null

# Autotest
var autotest := ""
var screenshot_path := ""
var shot_at := -1.0
var auto_elapsed := 0.0
var auto_next_press := 0.0
var auto_slot := 0
var auto_stage := 0
var auto_mob := -1
var stats := {"my_hits": 0, "pawn_swings": 0, "pawn_heals": 0, "mob_swings_at_party": 0,
	"kills": 0, "players_seen": 0, "queued": 0, "max_gap": 0.0,
	"shots_judged": 0, "shots_agreed": 0, "dashes": 0}
var auto_errors: Array[String] = []


func _ready() -> void:
	var initial_latency := 0
	var initial_loss := 0.0
	var args := OS.get_cmdline_user_args()
	var i := 0
	while i < args.size():
		match args[i]:
			"--server":
				var parts: PackedStringArray = args[i + 1].split(":")
				server_host = parts[0]
				if parts.size() > 1:
					server_port = int(parts[1])
				i += 1
			"--latency":
				initial_latency = int(args[i + 1])
				i += 1
			"--loss":
				initial_loss = float(args[i + 1])
				i += 1
			"--autotest":
				autotest = "dummy"
				if i + 1 < args.size() and not args[i + 1].begins_with("--"):
					autotest = args[i + 1]
					i += 1
			"--screenshot":
				screenshot_path = args[i + 1]
				i += 1
			"--shot-at":
				shot_at = float(args[i + 1])
				i += 1
		i += 1
	if shot_at < 0:
		shot_at = 14.0 if autotest == "pawns" else 1.0

	_setup_input_map()
	camera = IsoCamera.new()
	add_child(camera)
	hud = Hud.new()
	add_child(hud)
	hud.fake_lag_changed.connect(_on_fake_lag)

	var yaml := FileAccess.get_file_as_string(CONTENT_PATH)
	client = CombatClient.new()
	add_child(client)
	var err := client.connect_to_server(server_host, server_port, yaml)
	if err != "":
		hud.show_error("Could not start client: " + err)
		hud.log_line("[color=#ff6666]" + err + "[/color]")
		client = null
		return
	half_extent = client.world_half_extent()
	for n in client.ability_count():
		abilities.append(client.ability_info(n))
		if abilities[n].delivery == "dash":
			dodge_slot = n
	_build_world()
	hud.build_action_bar(client)
	hud.set_fake_lag(initial_latency, initial_loss)
	hud.log_line("Connecting to %s:%d ..." % [server_host, server_port])


func _on_fake_lag(latency_ms: int, loss_percent: float) -> void:
	if client != null:
		client.set_fake_lag(latency_ms, loss_percent)


func _setup_input_map() -> void:
	var keys := {
		"move_up": [KEY_W, KEY_UP], "move_down": [KEY_S, KEY_DOWN],
		"move_left": [KEY_A, KEY_LEFT], "move_right": [KEY_D, KEY_RIGHT],
		"target_next": [KEY_TAB], "clear_target": [KEY_ESCAPE],
		"pawn_attack": [KEY_F], "pawn_regroup": [KEY_G],
		"dodge": [KEY_SPACE],
	}
	for n in 9:
		keys["slot_%d" % (n + 1)] = [KEY_1 + n]
	for action in keys:
		if InputMap.has_action(action):
			continue
		InputMap.add_action(action)
		for k in keys[action]:
			var ev := InputEventKey.new()
			ev.physical_keycode = k
			InputMap.action_add_event(action, ev)


func _build_world() -> void:
	world = GreyboxWorld.new()
	add_child(world)
	world.build(client.world_layout(), half_extent)

	player = CharacterBody3D.new()
	player.motion_mode = CharacterBody3D.MOTION_MODE_FLOATING
	var shape := CollisionShape3D.new()
	var cap := CapsuleShape3D.new()
	cap.radius = 0.35
	cap.height = 1.8
	shape.shape = cap
	shape.position.y = 0.9
	player.add_child(shape)
	add_child(player)
	player_view = UnitView.new()
	player.add_child(player_view)
	player_view.setup("player", "humanoid", Color(0.3, 0.55, 1.0), 1.0)
	player.position = client.world_layout().get("spawn", Vector3.ZERO)
	camera.snap_to(player.position)

	target_ring = MeshInstance3D.new()
	var torus := TorusMesh.new()
	torus.inner_radius = 0.75
	torus.outer_radius = 0.9
	target_ring.mesh = torus
	target_ring.scale = Vector3(1, 0.04, 1)
	target_ring_mat = _unshaded(Hud.HOSTILE)
	target_ring.material_override = target_ring_mat
	target_ring.visible = false
	add_child(target_ring)

	move_marker = MeshInstance3D.new()
	var mt := TorusMesh.new()
	mt.inner_radius = 0.2
	mt.outer_radius = 0.3
	move_marker.mesh = mt
	move_marker.scale = Vector3(1, 0.05, 1)
	move_marker.material_override = _unshaded(Color(0.6, 1, 0.6))
	move_marker.visible = false
	add_child(move_marker)
	fx = CombatFx.new()
	add_child(fx)
	world_built = true


# --------------------------------------------------------------------------
# Frame loop
# --------------------------------------------------------------------------

func _process(delta: float) -> void:
	if client == null:
		return
	client.poll()
	entities = client.entities()
	by_id.clear()
	for e in entities:
		by_id[e.id] = e

	var correction = client.take_correction()
	if correction != null:
		player.position = correction
		move_goal = null
		camera.snap_to(player.position)

	var dead := client.is_dead()
	if dead != was_dead:
		player_view.set_dead(dead)
		was_dead = dead
	_move_player(delta, dead)
	_sync_entities()
	_handle_events()
	fx.update(client)
	hud.update(client, camera, player.position, views, entities, "%s:%d" % [server_host, server_port])

	if autotest != "":
		_run_autotest(delta)


func _move_player(delta: float, dead: bool) -> void:
	var dir := Vector3.ZERO
	if not dead:
		dir = _move_input()
	var dashing := dash_left > 0.0 and not dead
	var moving := dir != Vector3.ZERO or dashing
	aim_face_left -= delta
	if dashing:
		dash_left -= delta
		dir = dash_vel.normalized()
		player.velocity = dash_vel
		player.move_and_slide()
		player.position.y = 0
		player.position.x = clamp(player.position.x, -half_extent + 1, half_extent - 1)
		player.position.z = clamp(player.position.z, -half_extent + 1, half_extent - 1)
		facing = atan2(dir.x, dir.z)
	elif moving:
		player.velocity = dir * move_speed
		player.move_and_slide()
		player.position.y = 0
		player.position.x = clamp(player.position.x, -half_extent + 1, half_extent - 1)
		player.position.z = clamp(player.position.z, -half_extent + 1, half_extent - 1)
		facing = atan2(dir.x, dir.z)
	elif not dead:
		# Standing still: face what we just aimed at, else the target, like
		# a tab-target caster does.
		var t = aim_face if aim_face != null and aim_face_left > 0.0 else _entity_pos(client.get_target())
		if t != null:
			var to: Vector3 = t - player.position
			if to.length() > 0.1:
				facing = atan2(to.x, to.z)
	player_view.rotation.y = facing
	move_marker.visible = move_goal != null
	if move_goal != null:
		move_marker.position = move_goal + Vector3(0, 0.03, 0)
	client.set_local_move(player.position, facing, moving)
	var cb := client.cast_bar()
	player_view.set_casting(cb.active, Color(cb.get("color", "#ffffff")), cb.get("channel", false))
	camera.follow(player.position, delta)


## Camera-relative WASD, else hold right mouse to walk toward the cursor,
## else continue to the last click-to-move point.
func _move_input() -> Vector3:
	var v := Input.get_vector("move_left", "move_right", "move_down", "move_up")
	if v != Vector2.ZERO:
		move_goal = null
		return (camera.ground_right() * v.x + camera.ground_forward() * v.y).normalized()
	if Input.is_mouse_button_pressed(MOUSE_BUTTON_RIGHT) and autotest == "":
		var p = camera.ground_point(get_viewport().get_mouse_position())
		if p != null:
			move_goal = p
	if move_goal != null:
		var to: Vector3 = move_goal - player.position
		to.y = 0
		if to.length() <= ARRIVE_DIST:
			move_goal = null
			return Vector3.ZERO
		return to.normalized()
	return Vector3.ZERO


func _unhandled_input(event: InputEvent) -> void:
	if client == null or not world_built:
		return
	for n in 9:
		if event.is_action_pressed("slot_%d" % (n + 1)) and n < client.ability_count():
			_press(n)
			return
	if event.is_action_pressed("dodge") and dodge_slot >= 0:
		_press(dodge_slot)
		return
	if event.is_action_pressed("target_next"):
		_cycle_target()
	elif event.is_action_pressed("clear_target"):
		client.set_target(-1)
	elif event.is_action_pressed("pawn_attack"):
		_pawn_command("attack")
	elif event.is_action_pressed("pawn_regroup"):
		_pawn_command("regroup")
	elif event is InputEventMouseButton and event.pressed:
		match event.button_index:
			MOUSE_BUTTON_LEFT:
				_click_target(event.position)
			MOUSE_BUTTON_WHEEL_UP:
				camera.zoom(-1)
			MOUSE_BUTTON_WHEEL_DOWN:
				camera.zoom(1)


func _press(slot: int) -> void:
	var was_ready := client.swipe(slot) == 0.0
	var aim = _aim_for(slot)
	if aim == null:
		client.press(slot)
	else:
		client.press_at(slot, aim)
		if abilities[slot].delivery != "dash":
			aim_face = aim
			aim_face_left = AIM_FACE_SECONDS
	hud.flash_slot(slot)
	# Instants have no bar: play a release so the press reads on the body.
	if was_ready and client.swipe(slot) > 0.0 and not client.cast_bar().active:
		player_view.play_action("attack")


## Ground point for an aimed ability, or null to let it fire at the target.
## Skillshots and runes go to the cursor (quick cast). The dodge goes where
## we are walking, else toward the cursor.
func _aim_for(slot: int):
	var delivery: String = abilities[slot].delivery
	if delivery == "target":
		return null
	var cursor = null
	if autotest == "":
		cursor = camera.ground_point(get_viewport().get_mouse_position())
	if delivery == "dash":
		var walk := _move_input()
		if walk == Vector3.ZERO and cursor != null:
			walk = (cursor - player.position)
			walk.y = 0
		if walk.length() < 0.01:
			return null
		return player.position + walk.normalized() * float(abilities[slot].dash_m)
	return cursor


func _start_dash(dir: Vector3, distance: float) -> void:
	dash_vel = dir.normalized() * distance / DASH_SECONDS
	dash_left = DASH_SECONDS
	move_goal = null
	player_view.play_action("attack")
	stats.dashes += 1


func _pawn_command(cmd: String) -> void:
	client.pawn_command(cmd)
	var lines: Array = ["On it!", "Attacking!", "For the Arisen!"] if cmd == "attack" else ["Coming!", "Falling back!", "Right behind you."]
	for e in entities:
		if e.kind == "pawn" and e.mine and not e.dead and views.has(e.id):
			_bark(views[e.id], lines.pick_random())


## Tab: nearest living enemy first, then the next nearest, wrapping around.
func _cycle_target() -> void:
	var candidates := []
	for e in entities:
		if e.hostile and not e.dead:
			var d: float = (e.pos as Vector3).distance_to(player.position)
			if d <= TAB_RANGE:
				candidates.append([d, e.id])
	if candidates.is_empty():
		return
	candidates.sort_custom(func(a, b): return a[0] < b[0])
	var current := client.get_target()
	var idx := -1
	for i in candidates.size():
		if candidates[i][1] == current:
			idx = i
	client.set_target(candidates[(idx + 1) % candidates.size()][1])


func _click_target(screen_pos: Vector2) -> void:
	var best := -1
	var best_d := CLICK_PICK_PX
	for id in views:
		var v: UnitView = views[id]
		var p: Vector3 = v.position + Vector3(0, v.head_height() * 0.6, 0)
		if camera.is_position_behind(p):
			continue
		var d := camera.unproject_position(p).distance_to(screen_pos)
		if d < best_d:
			best_d = d
			best = id
	if best != -1:
		client.set_target(best)


func _entity_pos(id: int):
	if id < 0 or not by_id.has(id):
		return null
	return by_id[id].pos


# --------------------------------------------------------------------------
# Entities
# --------------------------------------------------------------------------

func _sync_entities() -> void:
	var seen := {}
	var players := 0
	for e in entities:
		if e.kind == "player" and not e.is_me:
			players += 1
		if e.is_me:
			continue
		seen[e.id] = true
		if not views.has(e.id):
			views[e.id] = _make_view(e)
		var v: UnitView = views[e.id]
		v.position = e.pos
		v.rotation.y = e.facing
		v.set_dead(e.dead)
		v.set_casting(e.cast_name != "", Color(0.6, 0.8, 1.0))
	stats.players_seen = max(stats.players_seen, players)
	for id in views.keys():
		if not seen.has(id):
			views[id].queue_free()
			views.erase(id)

	var t = by_id.get(client.get_target())
	target_ring.visible = t != null
	if t != null:
		target_ring.position = (t.pos as Vector3) + Vector3(0, 0.03, 0)
		target_ring.scale = Vector3(t.scale, 0.04, t.scale)
		target_ring_mat.albedo_color = hud.unit_color(t)


func _make_view(e: Dictionary) -> UnitView:
	var v := UnitView.new()
	add_child(v)
	var shape := "humanoid"
	if e.kind == "dummy":
		shape = "dummy"
	elif "wolf" in e.def:
		shape = "quadruped"
	var key: String = e.def if e.kind != "player" else "player"
	var c := Color(e.color)
	if e.kind == "player":
		c = Color(0.85, 0.7, 0.3)  # other players: gold, you are blue
	v.setup(key, shape, c, e.scale)
	return v


func _handle_events() -> void:
	for ev in client.take_events():
		match ev.type:
			"hit":
				_floating_text(ev.target, Hud.num(ev.amount) + ("!" if ev.crit else ""), ev.crit,
					Color(1, 0.6, 0.3) if ev.periodic else Color.WHITE, 60 if ev.periodic else 80)
				_hit_reaction(ev.target)
				if not ev.periodic and not ev.aimed:
					_bolt(ev.source, ev.target, Color(0.7, 0.85, 1.0))
				if ev.mine:
					var verb := "ticks" if ev.periodic else "hits"
					var crit := " [color=#ffd84a](crit)[/color]" if ev.crit else ""
					hud.log_line("Your %s %s %s for [b]%s[/b]%s" % [ev.name, verb, _name(ev.target), Hud.num(ev.amount), crit])
					stats.my_hits += 1
			"swing":
				_on_swing(ev)
			"died":
				var e = by_id.get(ev.entity)
				if ev.entity == client.my_id():
					hud.log_line("[color=#ff6666]You died.[/color]")
				elif e != null:
					hud.log_line("[color=#c0c0c0]%s dies.[/color]" % e.name)
					if e.kind == "mob":
						stats.kills += 1
			"error":
				hud.show_error(ev.text)
				auto_errors.append(ev.text)
			"log":
				hud.log_line("[color=#9aa4b2]" + ev.text + "[/color]")
			"dash":
				_start_dash(ev.dir, ev.distance)
			"shot_resolved":
				stats.shots_judged += 1
				if ev.agree:
					stats.shots_agreed += 1
			"confirmed":
				if ev.queue_gap_ms >= 0:
					stats.queued += 1
					stats.max_gap = max(stats.max_gap, ev.queue_gap_ms)


func _on_swing(ev: Dictionary) -> void:
	var src = by_id.get(ev.source)
	var dst = by_id.get(ev.target)
	if views.has(ev.source):
		views[ev.source].play_action("attack")
	var mine_party: bool = ev.mine
	var at_party: bool = ev.target == client.my_id() or (dst != null and dst.kind == "pawn" and dst.mine)
	if src != null and src.reach > 5.0:
		_bolt(ev.source, ev.target, Color(0.5, 1, 0.7) if ev.heal else Color(src.color).lightened(0.3))
	if ev.heal:
		_floating_text(ev.target, "+" + Hud.num(ev.amount), ev.crit, Color(0.4, 1, 0.5), 60)
		if mine_party:
			stats.pawn_heals += 1
		return
	_hit_reaction(ev.target)
	# Only show numbers that involve us, so a busy zone stays readable.
	if mine_party or at_party:
		var col := Color(1, 0.4, 0.35) if at_party else Color(0.85, 0.9, 1.0)
		_floating_text(ev.target, Hud.num(ev.amount) + ("!" if ev.crit else ""), ev.crit, col, 56)
	if mine_party:
		stats.pawn_swings += 1
	if at_party and src != null and src.kind == "mob":
		stats.mob_swings_at_party += 1
		if ev.target == client.my_id():
			hud.log_line("[color=#ff8080]%s hits you for %s[/color]" % [src.name, Hud.num(ev.amount)])


func _name(id: int) -> String:
	if id == client.my_id():
		return "you"
	var e = by_id.get(id)
	return e.name if e != null else "?"


func _view_or_me(id: int) -> Node3D:
	if id == client.my_id():
		return player
	return views.get(id)


func _hit_reaction(id: int) -> void:
	if id == client.my_id():
		player_view.play_action("hit")
	elif views.has(id):
		views[id].play_action("hit")


func _head(id: int):
	var n := _view_or_me(id)
	if n == null:
		return null
	var v: UnitView = player_view if n == player else n
	return n.position + Vector3(0, v.head_height(), 0)


func _floating_text(target: int, text: String, crit: bool, color: Color, size: int) -> void:
	var pos = _head(target)
	if pos == null:
		return
	var l := Label3D.new()
	l.text = text
	l.billboard = BaseMaterial3D.BILLBOARD_ENABLED
	l.no_depth_test = true
	l.font_size = int(size * 1.35) if crit else size
	l.pixel_size = 0.012
	l.outline_size = 16
	l.modulate = Color(1, 0.85, 0.2) if crit else color
	l.position = pos + Vector3(randf_range(-0.6, 0.6), 0.6, 0)
	add_child(l)
	var tw := create_tween()
	tw.set_parallel(true)
	tw.tween_property(l, "position:y", l.position.y + 1.6, 1.1).set_ease(Tween.EASE_OUT).set_trans(Tween.TRANS_CUBIC)
	tw.tween_property(l, "modulate:a", 0.0, 1.1).set_delay(0.4)
	tw.chain().tween_callback(l.queue_free)


## A quick projectile from source to target. Cosmetic only: damage already
## happened on the server.
func _bolt(source: int, target: int, color: Color) -> void:
	var a = _head(source)
	var b = _head(target)
	if a == null or b == null or source == target:
		return
	var m := MeshInstance3D.new()
	var s := SphereMesh.new()
	s.radius = 0.12
	s.height = 0.24
	m.mesh = s
	m.material_override = _unshaded(color)
	m.position = a - Vector3(0, 0.5, 0)
	add_child(m)
	var tw := create_tween()
	tw.tween_property(m, "position", b - Vector3(0, 0.6, 0), 0.18)
	tw.tween_callback(m.queue_free)


func _bark(v: UnitView, text: String) -> void:
	var l := Label3D.new()
	l.text = text
	l.billboard = BaseMaterial3D.BILLBOARD_ENABLED
	l.no_depth_test = true
	l.font_size = 36
	l.pixel_size = 0.01
	l.outline_size = 10
	l.modulate = Color(0.85, 1, 0.85)
	l.position = Vector3(0, v.head_height() + 1.5, 0)
	v.add_child(l)
	var tw := create_tween()
	tw.tween_interval(1.2)
	tw.tween_property(l, "modulate:a", 0.0, 0.4)
	tw.tween_callback(l.queue_free)


func _unshaded(c: Color) -> StandardMaterial3D:
	var m := StandardMaterial3D.new()
	m.shading_mode = BaseMaterial3D.SHADING_MODE_UNSHADED
	m.albedo_color = c
	return m


# --------------------------------------------------------------------------
# Autotest (headless smoke test)
# --------------------------------------------------------------------------

func _run_autotest(delta: float) -> void:
	auto_elapsed += delta
	if not client.is_online():
		if auto_elapsed > 10.0:
			print("AUTOTEST FAIL: could not connect")
			get_tree().quit(1)
		return
	if screenshot_path != "" and auto_elapsed > shot_at:
		get_viewport().get_texture().get_image().save_png(screenshot_path)
		print("AUTOTEST screenshot saved to " + screenshot_path)
		screenshot_path = ""
	match autotest:
		"pawns":
			_autotest_pawns()
		"idle":
			if auto_elapsed > 10.0:
				_finish(stats.players_seen > 0)
		"aimed":
			_autotest_aimed()
		_:
			_autotest_dummy()


func _autotest_dummy() -> void:
	if client.get_target() < 0:
		for e in entities:
			if e.kind == "dummy":
				client.set_target(e.id)
		auto_next_press = auto_elapsed + 0.2
		return
	_auto_rotation()
	if auto_elapsed > 9.0:
		_finish(stats.my_hits > 0)


## Skillshots, runes and dodges at the walking dummy through the soft-target
## fallback (there is no mouse in a headless run).
func _autotest_aimed() -> void:
	camera.distance = IsoCamera.MAX_DIST  # keep the dummy's circle in the screenshot
	if client.get_target() < 0:
		for e in entities:
			if e.kind == "dummy":
				client.set_target(e.id)
		auto_next_press = auto_elapsed + 0.2
		return
	if auto_elapsed >= auto_next_press:
		var order := ["glacial_spike", "flame_rune", "glacial_spike", "blink_step"]
		var want: String = order[auto_slot % order.size()]
		for n in abilities.size():
			if abilities[n].id == want:
				_press(n)
		auto_slot += 1
		auto_next_press = auto_elapsed + 0.8
	if auto_elapsed > 12.0:
		_finish(stats.my_hits > 0 and stats.shots_judged > 0 and stats.dashes > 0)


## Walk to the nearest ghoul camp, stop at casting range, pull with the
## rotation, send the pawns in, fight for a while.
func _autotest_pawns() -> void:
	match auto_stage:
		0:
			var best := -1
			var best_d := INF
			for e in entities:
				if e.kind == "mob" and e.def == "ghoul" and not e.dead:
					var d: float = (e.pos as Vector3).distance_to(player.position)
					if d < best_d:
						best_d = d
						best = e.id
			if best == -1:
				return
			auto_mob = best
			var mp: Vector3 = by_id[best].pos
			move_goal = mp + (player.position - mp).normalized() * 14.0
			auto_stage = 1
		1:
			if move_goal == null:
				client.set_target(auto_mob)
				_pawn_command("attack")
				auto_next_press = auto_elapsed
				auto_stage = 2
		2:
			var t = by_id.get(client.get_target())
			if t == null or t.dead:
				_cycle_target()
				_pawn_command("attack")
			_auto_rotation()
			if auto_elapsed > 32.0:
				_finish(stats.my_hits > 0 and stats.pawn_swings > 0 and stats.mob_swings_at_party > 0)


func _auto_rotation() -> void:
	if auto_elapsed >= auto_next_press:
		_press(auto_slot % client.ability_count())
		auto_slot += 1
		auto_next_press = auto_elapsed + 0.6


func _finish(pass_: bool) -> void:
	print("AUTOTEST mode=%s %s errors=%s" % [autotest, JSON.stringify(stats), str(auto_errors)])
	print("AUTOTEST " + ("PASS" if pass_ else "FAIL"))
	client.disconnect_from_server()
	get_tree().quit(0 if pass_ else 1)

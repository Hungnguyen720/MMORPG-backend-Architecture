extends Node3D
## Phase 0 combat-feel prototype. Builds the arena and UI in code so the
## whole client is readable in one file. Networking, prediction and the
## combat rules live in Rust (CombatClient, from the GDExtension).
##
## Command-line options (after `--`):
##   --server HOST:PORT   default 127.0.0.1:5000
##   --latency MS         initial fake latency (RTT), default 0
##   --loss PERCENT       initial fake loss, default 0
##   --autotest           target the dummy, press 1-5 for 8 s, print a summary, quit
##   --screenshot PATH    with --autotest, save a PNG of the window halfway through

const CONTENT_PATH := "res://content/abilities.yaml"
const ARENA := 40.0
const CAMERA_OFFSET := Vector3(0, 17, 15)
const SLOT_SIZE := 64

var client: CombatClient
var server_host := "127.0.0.1"
var server_port := 5000
var autotest := false
var screenshot_path := ""
var move_speed := 7.0
var facing := 0.0

var player: Node3D
var camera: Camera3D
var target_ring: MeshInstance3D
var others := {}  # entity id -> Node3D

# UI
var slots := []
var white_tex: ImageTexture
var cast_panel: Control
var cast_bar: ProgressBar
var cast_fill: StyleBoxFlat
var cast_label: Label
var cast_time: Label
var target_frame: Control
var target_name: Label
var target_hp: ProgressBar
var target_info: Label
var mana_bar: ProgressBar
var mana_label: Label
var error_label: Label
var error_time := 0.0
var log_box: RichTextLabel
var stats_label: Label
var status_label: Label
var latency_slider: HSlider
var loss_slider: HSlider
var latency_value: Label
var loss_value: Label

# Autotest bookkeeping
var auto_elapsed := 0.0
var auto_next_press := 0.0
var auto_slot := 0
var auto_hits := 0
var auto_errors: Array[String] = []
var auto_gaps: Array[float] = []


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
				autotest = true
			"--screenshot":
				screenshot_path = args[i + 1]
				i += 1
		i += 1

	_build_world()
	_build_ui()

	var yaml := FileAccess.get_file_as_string(CONTENT_PATH)
	client = CombatClient.new()
	add_child(client)
	var err := client.connect_to_server(server_host, server_port, yaml)
	if err != "":
		_show_error("Could not start client: " + err)
		_log("[color=#ff6666]" + err + "[/color]")
		return
	latency_slider.value = initial_latency
	loss_slider.value = initial_loss
	_apply_fake_lag()
	_build_action_bar()
	_log("Connecting to %s:%d ..." % [server_host, server_port])


# --------------------------------------------------------------------------
# Frame loop
# --------------------------------------------------------------------------

func _process(delta: float) -> void:
	if client == null:
		return
	client.poll()

	var correction = client.take_correction()
	if correction != null:
		player.position = correction

	_move_player(delta)
	_sync_entities()
	_handle_events()
	_update_ui()

	if autotest:
		_run_autotest(delta)


func _move_player(delta: float) -> void:
	var dir := Vector3.ZERO
	if Input.is_physical_key_pressed(KEY_W): dir.z -= 1
	if Input.is_physical_key_pressed(KEY_S): dir.z += 1
	if Input.is_physical_key_pressed(KEY_A): dir.x -= 1
	if Input.is_physical_key_pressed(KEY_D): dir.x += 1
	var moving := dir != Vector3.ZERO
	if moving:
		dir = dir.normalized()
		player.position += dir * move_speed * delta
		player.position.x = clamp(player.position.x, -ARENA + 1, ARENA - 1)
		player.position.z = clamp(player.position.z, -ARENA + 1, ARENA - 1)
		facing = atan2(dir.x, dir.z)
	else:
		# Standing still: face the target, like a tab-target caster does.
		var t = _entity_pos(client.get_target())
		if t != null:
			var to: Vector3 = t - player.position
			if to.length() > 0.1:
				facing = atan2(to.x, to.z)
	player.rotation.y = facing
	client.set_local_move(player.position, facing, moving)
	camera.position = player.position + CAMERA_OFFSET
	camera.look_at(player.position + Vector3(0, 0, -3))


func _unhandled_input(event: InputEvent) -> void:
	if client == null:
		return
	if event is InputEventKey and event.pressed and not event.echo:
		var k: Key = event.physical_keycode
		if k >= KEY_1 and k <= KEY_9:
			var slot := int(k - KEY_1)
			if slot < slots.size():
				_press(slot)
		elif k == KEY_TAB:
			_cycle_target()
		elif k == KEY_ESCAPE:
			client.set_target(-1)
	elif event is InputEventMouseButton and event.pressed and event.button_index == MOUSE_BUTTON_LEFT:
		_click_target(event.position)


func _press(slot: int) -> void:
	client.press(slot)
	var s: Dictionary = slots[slot]
	s.flash = 0.15


func _cycle_target() -> void:
	# No PvP: the only hostile things are dummies. Tab picks the nearest.
	var best := -1
	var best_d := INF
	for e in client.entities():
		if e.kind == "dummy":
			var d: float = (e.pos as Vector3).distance_to(player.position)
			if d < best_d and e.id != client.get_target():
				best_d = d
				best = e.id
	if best == -1:
		best = client.dummy_id()
	client.set_target(best)


func _click_target(screen_pos: Vector2) -> void:
	for e in client.entities():
		if e.is_me:
			continue
		var p: Vector3 = e.pos + Vector3(0, 1, 0)
		if camera.is_position_behind(p):
			continue
		if camera.unproject_position(p).distance_to(screen_pos) < 50:
			client.set_target(e.id)
			return


func _entity_pos(id: int):
	if id < 0:
		return null
	for e in client.entities():
		if e.id == id:
			return e.pos
	return null


# --------------------------------------------------------------------------
# Entities
# --------------------------------------------------------------------------

func _sync_entities() -> void:
	var seen := {}
	for e in client.entities():
		if e.is_me:
			continue
		seen[e.id] = true
		if not others.has(e.id):
			others[e.id] = _make_dummy() if e.kind == "dummy" else _make_character(Color(0.9, 0.75, 0.3))
			add_child(others[e.id])
		var n: Node3D = others[e.id]
		n.position = e.pos
		if e.kind == "player":
			n.rotation.y = e.facing
		var plate: Label3D = n.get_node("Plate")
		plate.text = e.name if e.cast_name == "" else "%s\n%s %d%%" % [e.name, e.cast_name, int(e.cast_progress * 100)]
	for id in others.keys():
		if not seen.has(id):
			others[id].queue_free()
			others.erase(id)

	var t = _entity_pos(client.get_target())
	target_ring.visible = t != null
	if t != null:
		target_ring.position = t + Vector3(0, 0.03, 0)


func _handle_events() -> void:
	for ev in client.take_events():
		match ev.type:
			"hit":
				_floating_text(ev)
				var verb := "ticks" if ev.periodic else "hits"
				var crit := " [color=#ffd84a](crit)[/color]" if ev.crit else ""
				var who := "Your" if ev.mine else "Their"
				_log("%s %s %s for [b]%s[/b]%s" % [who, ev.name, verb, _num(ev.amount), crit])
				auto_hits += 1
			"error":
				_show_error(ev.text)
				auto_errors.append(ev.text)
			"log":
				_log("[color=#9aa4b2]" + ev.text + "[/color]")
			"confirmed":
				if ev.queue_gap_ms >= 0:
					auto_gaps.append(ev.queue_gap_ms)
					_log("[color=#6f7a88]%s fired from queue, %.2f ms after ready[/color]" % [ev.name, ev.queue_gap_ms])


func _floating_text(ev: Dictionary) -> void:
	var pos = _entity_pos(ev.target)
	if pos == null:
		return
	var l := Label3D.new()
	l.text = _num(ev.amount) + ("!" if ev.crit else "")
	l.billboard = BaseMaterial3D.BILLBOARD_ENABLED
	l.no_depth_test = true
	l.font_size = 110 if ev.crit else (60 if ev.periodic else 80)
	l.pixel_size = 0.012
	l.outline_size = 16
	l.modulate = Color(1, 0.85, 0.2) if ev.crit else (Color(1, 0.6, 0.3) if ev.periodic else Color.WHITE)
	l.position = pos + Vector3(randf_range(-0.8, 0.8), 2.6, 0)
	add_child(l)
	var tw := create_tween()
	tw.set_parallel(true)
	tw.tween_property(l, "position:y", l.position.y + 1.6, 1.1).set_ease(Tween.EASE_OUT).set_trans(Tween.TRANS_CUBIC)
	tw.tween_property(l, "modulate:a", 0.0, 1.1).set_delay(0.4)
	tw.chain().tween_callback(l.queue_free)


func _num(v: float) -> String:
	var s := str(int(round(v)))
	var out := ""
	while s.length() > 3:
		out = "," + s.substr(s.length() - 3) + out
		s = s.substr(0, s.length() - 3)
	return s + out


# --------------------------------------------------------------------------
# UI updates
# --------------------------------------------------------------------------

func _update_ui() -> void:
	var online := client.is_online()
	status_label.text = "Connected" if online else "Connecting to %s:%d ..." % [server_host, server_port]
	status_label.modulate = Color(0.5, 1, 0.6) if online else Color(1, 0.8, 0.4)

	# Action bar: GCD / cooldown swipe, queued highlight, press flash.
	var queued := client.queued_slot()
	for i in slots.size():
		var s: Dictionary = slots[i]
		s.swipe.value = client.swipe(i)
		s.flash = max(0.0, s.flash - get_process_delta_time())
		var border: StyleBoxFlat = s.style
		if i == queued:
			border.border_color = Color(1, 0.85, 0.2)
		elif s.flash > 0:
			border.border_color = Color.WHITE
		else:
			border.border_color = Color(0.15, 0.15, 0.18)

	# Our own cast bar, predicted locally.
	var cb := client.cast_bar()
	cast_panel.visible = cb.active
	if cb.active:
		cast_fill.bg_color = Color(cb.color)
		cast_bar.value = (1.0 - cb.progress) if cb.channel else cb.progress
		cast_label.text = cb.name
		cast_time.text = "%.1f" % (cb.remaining_ms / 1000.0)

	var m := client.mana()
	mana_bar.max_value = max(m.y, 1.0)
	mana_bar.value = m.x
	mana_label.text = "%d / %d" % [m.x, m.y]

	# Target frame.
	var tid := client.get_target()
	var tdata = null
	for e in client.entities():
		if e.id == tid:
			tdata = e
	target_frame.visible = tdata != null
	if tdata != null:
		target_name.text = tdata.name
		target_hp.max_value = max(tdata.max_hp, 1.0)
		target_hp.value = tdata.hp
		var dist: float = (tdata.pos as Vector3).distance_to(player.position)
		var parts := PackedStringArray(["%s / %s" % [_num(tdata.hp), _num(tdata.max_hp)], "%.1f m" % dist])
		for a in tdata.auras:
			parts.append("%s %.0fs" % [a.name, a.remaining_ms / 1000.0])
		target_info.text = "   ".join(parts)
		target_info.modulate = Color(1, 0.5, 0.5) if dist > 30 else Color.WHITE

	var st := client.net_stats()
	if not st.is_empty():
		var gap := "-" if st.last_queue_gap_ms < 0 else "%.2f ms" % st.last_queue_gap_ms
		stats_label.text = "RTT %d ms   loss %.1f%%\nlast spell-queue gap %s" % [st.rtt_ms, st.loss * 100.0, gap]

	error_time -= get_process_delta_time()
	error_label.modulate.a = clamp(error_time, 0.0, 1.0)


func _show_error(text: String) -> void:
	error_label.text = text
	error_time = 2.0


func _log(bb: String) -> void:
	log_box.append_text(bb + "\n")


func _apply_fake_lag() -> void:
	latency_value.text = "%d ms" % latency_slider.value
	loss_value.text = "%.1f %%" % loss_slider.value
	if client != null:
		client.set_fake_lag(int(latency_slider.value), loss_slider.value)


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
	if client.get_target() < 0:
		_cycle_target()
		auto_next_press = auto_elapsed + 0.2
		auto_elapsed = 0.0
		return
	if auto_elapsed >= auto_next_press:
		_press(auto_slot % slots.size())
		auto_slot += 1
		auto_next_press = auto_elapsed + 0.6
	if screenshot_path != "" and auto_elapsed > 1.0:
		get_viewport().get_texture().get_image().save_png(screenshot_path)
		print("AUTOTEST screenshot saved to " + screenshot_path)
		screenshot_path = ""
	if auto_elapsed > 8.0:
		var max_gap := 0.0
		for g in auto_gaps:
			max_gap = max(max_gap, g)
		print("AUTOTEST hits=%d queued=%d max_queue_gap_ms=%.2f errors=%s" % [auto_hits, auto_gaps.size(), max_gap, str(auto_errors)])
		print("AUTOTEST " + ("PASS" if auto_hits > 0 else "FAIL"))
		client.disconnect_from_server()
		get_tree().quit(0 if auto_hits > 0 else 1)


# --------------------------------------------------------------------------
# World construction
# --------------------------------------------------------------------------

func _build_world() -> void:
	var env := WorldEnvironment.new()
	var e := Environment.new()
	e.background_mode = Environment.BG_COLOR
	e.background_color = Color(0.08, 0.09, 0.12)
	e.ambient_light_source = Environment.AMBIENT_SOURCE_COLOR
	e.ambient_light_color = Color(0.55, 0.6, 0.7)
	e.ambient_light_energy = 0.6
	e.tonemap_mode = Environment.TONE_MAPPER_FILMIC
	env.environment = e
	add_child(env)

	var sun := DirectionalLight3D.new()
	sun.rotation_degrees = Vector3(-55, 35, 0)
	sun.shadow_enabled = true
	add_child(sun)

	var ground := MeshInstance3D.new()
	var pm := PlaneMesh.new()
	pm.size = Vector2(ARENA * 2, ARENA * 2)
	ground.mesh = pm
	ground.material_override = _grid_material()
	add_child(ground)

	# Pillars at the edges give a sense of movement speed and scale.
	for x in [-30, -15, 15, 30]:
		for z in [-30, 30]:
			_add_box(Vector3(x, 1.5, z), Vector3(1.2, 3, 1.2), Color(0.35, 0.37, 0.42))

	player = _make_character(Color(0.3, 0.6, 1.0))
	(player.get_node("Plate") as Label3D).text = ""
	add_child(player)

	target_ring = MeshInstance3D.new()
	var torus := TorusMesh.new()
	torus.inner_radius = 1.0
	torus.outer_radius = 1.2
	target_ring.mesh = torus
	target_ring.scale = Vector3(1, 0.05, 1)
	target_ring.material_override = _unshaded(Color(1, 0.25, 0.2))
	target_ring.visible = false
	add_child(target_ring)

	camera = Camera3D.new()
	camera.fov = 50
	add_child(camera)


func _grid_material() -> ShaderMaterial:
	var sh := Shader.new()
	sh.code = """
shader_type spatial;
void fragment() {
	vec3 w = (INV_VIEW_MATRIX * vec4(VERTEX, 1.0)).xyz;
	vec2 g = abs(fract(w.xz / 2.0 - 0.5) - 0.5) / fwidth(w.xz / 2.0);
	float line = 1.0 - min(min(g.x, g.y), 1.0);
	ALBEDO = mix(vec3(0.16, 0.19, 0.17), vec3(0.28, 0.32, 0.29), line);
	ROUGHNESS = 0.95;
}
"""
	var m := ShaderMaterial.new()
	m.shader = sh
	return m


func _unshaded(c: Color) -> StandardMaterial3D:
	var m := StandardMaterial3D.new()
	m.shading_mode = BaseMaterial3D.SHADING_MODE_UNSHADED
	m.albedo_color = c
	return m


func _lit(c: Color) -> StandardMaterial3D:
	var m := StandardMaterial3D.new()
	m.albedo_color = c
	return m


func _add_box(pos: Vector3, size: Vector3, c: Color, parent: Node3D = null) -> MeshInstance3D:
	var mi := MeshInstance3D.new()
	var bm := BoxMesh.new()
	bm.size = size
	mi.mesh = bm
	mi.material_override = _lit(c)
	mi.position = pos
	(parent if parent != null else self).add_child(mi)
	return mi


func _plate(height: float) -> Label3D:
	var l := Label3D.new()
	l.name = "Plate"
	l.billboard = BaseMaterial3D.BILLBOARD_ENABLED
	l.position = Vector3(0, height, 0)
	l.font_size = 48
	l.pixel_size = 0.01
	l.outline_size = 12
	return l


func _make_character(c: Color) -> Node3D:
	var root := Node3D.new()
	var body := MeshInstance3D.new()
	var cap := CapsuleMesh.new()
	cap.radius = 0.4
	cap.height = 1.8
	body.mesh = cap
	body.position = Vector3(0, 0.9, 0)
	body.material_override = _lit(c)
	root.add_child(body)
	_add_box(Vector3(0, 1.4, 0.4), Vector3(0.25, 0.15, 0.3), Color(0.95, 0.95, 0.95), root)
	root.add_child(_plate(2.4))
	return root


func _make_dummy() -> Node3D:
	var root := Node3D.new()
	var wood := Color(0.55, 0.4, 0.25)
	_add_box(Vector3(0, 0.9, 0), Vector3(0.3, 1.8, 0.3), wood, root)
	_add_box(Vector3(0, 1.4, 0), Vector3(1.6, 0.2, 0.2), wood, root)
	var head := MeshInstance3D.new()
	var sm := SphereMesh.new()
	sm.radius = 0.3
	sm.height = 0.6
	head.mesh = sm
	head.position = Vector3(0, 2.05, 0)
	head.material_override = _lit(Color(0.85, 0.75, 0.5))
	root.add_child(head)
	root.add_child(_plate(2.8))
	return root


# --------------------------------------------------------------------------
# UI construction
# --------------------------------------------------------------------------

func _build_ui() -> void:
	var img := Image.create(8, 8, false, Image.FORMAT_RGBA8)
	img.fill(Color.WHITE)
	white_tex = ImageTexture.create_from_image(img)

	var ui := CanvasLayer.new()
	add_child(ui)
	var root := Control.new()
	root.anchor_right = 1.0
	root.anchor_bottom = 1.0
	root.mouse_filter = Control.MOUSE_FILTER_IGNORE
	ui.add_child(root)

	# Player frame, top left.
	var pf := _panel(root, Vector2(0, 0), Vector2(16, 16), Vector2(260, 70))
	var pv := VBoxContainer.new()
	pf.add_child(pv)
	pv.add_child(_label("You", 18))
	var mb := _bar(Color(0.25, 0.45, 1.0))
	mana_bar = mb[0]
	pv.add_child(mana_bar)
	mana_label = _label("", 12)
	mana_label.horizontal_alignment = HORIZONTAL_ALIGNMENT_CENTER
	_place(mana_label, Vector2(0, 0), Vector2(0, -2), Vector2(240, 18))
	mana_bar.add_child(mana_label)

	# Target frame, top centre.
	target_frame = _panel(root, Vector2(0.5, 0), Vector2(-190, 16), Vector2(380, 84))
	var tv := VBoxContainer.new()
	target_frame.add_child(tv)
	target_name = _label("", 18)
	tv.add_child(target_name)
	target_hp = _bar(Color(0.8, 0.2, 0.2))[0]
	tv.add_child(target_hp)
	target_info = _label("", 13)
	tv.add_child(target_info)
	target_frame.visible = false

	# Cast bar, above the action bar.
	cast_panel = Control.new()
	_place(cast_panel, Vector2(0.5, 1), Vector2(-170, -SLOT_SIZE - 90), Vector2(340, 26))
	cast_panel.mouse_filter = Control.MOUSE_FILTER_IGNORE
	root.add_child(cast_panel)
	var cbx := _bar(Color.WHITE)
	cast_bar = cbx[0]
	cast_fill = cbx[1]
	cast_bar.custom_minimum_size = Vector2(340, 26)
	cast_bar.max_value = 1.0
	cast_panel.add_child(cast_bar)
	cast_label = _label("", 15)
	cast_label.position = Vector2(8, 2)
	cast_panel.add_child(cast_label)
	cast_time = _label("", 15)
	cast_time.position = Vector2(296, 2)
	cast_panel.add_child(cast_time)
	cast_panel.visible = false

	# Error text, centre.
	error_label = _label("", 22)
	_place(error_label, Vector2(0.5, 0), Vector2(-300, 110), Vector2(600, 30))
	error_label.horizontal_alignment = HORIZONTAL_ALIGNMENT_CENTER
	error_label.modulate = Color(1, 0.3, 0.3, 0)
	error_label.add_theme_color_override("font_color", Color(1, 0.35, 0.3))
	root.add_child(error_label)

	# Combat log, bottom left.
	var lp := _panel(root, Vector2(0, 1), Vector2(16, -236), Vector2(440, 220))
	log_box = RichTextLabel.new()
	log_box.bbcode_enabled = true
	log_box.scroll_following = true
	log_box.add_theme_font_size_override("normal_font_size", 13)
	log_box.add_theme_font_size_override("bold_font_size", 13)
	log_box.custom_minimum_size = Vector2(420, 200)
	log_box.focus_mode = Control.FOCUS_NONE
	lp.add_child(log_box)

	# Debug panel, top right: the fake-lag controls.
	var dp := _panel(root, Vector2(1, 0), Vector2(-356, 16), Vector2(340, 170))
	var dv := VBoxContainer.new()
	dp.add_child(dv)
	status_label = _label("", 14)
	dv.add_child(status_label)
	var lat := _slider_row(dv, "Fake latency (RTT)", 0, 250, 10)
	latency_slider = lat[0]
	latency_value = lat[1]
	var loss := _slider_row(dv, "Fake packet loss", 0, 5, 0.5)
	loss_slider = loss[0]
	loss_value = loss[1]
	latency_slider.value_changed.connect(func(_v): _apply_fake_lag())
	loss_slider.value_changed.connect(func(_v): _apply_fake_lag())
	stats_label = _label("", 13)
	dv.add_child(stats_label)
	var help := _label("WASD move   Tab / click target   1-5 cast   Esc clear", 12)
	help.modulate = Color(0.7, 0.75, 0.8)
	dv.add_child(help)


func _build_action_bar() -> void:
	var bar := HBoxContainer.new()
	bar.add_theme_constant_override("separation", 8)
	var n := client.ability_count()
	var width := n * SLOT_SIZE + (n - 1) * 8
	_place(bar, Vector2(0.5, 1), Vector2(-width / 2.0, -SLOT_SIZE - 40), Vector2(width, SLOT_SIZE))
	cast_panel.get_parent().add_child(bar)
	for i in n:
		var info := client.ability_info(i)
		var style := StyleBoxFlat.new()
		style.bg_color = Color(info.color).darkened(0.35)
		style.set_border_width_all(3)
		style.border_color = Color(0.15, 0.15, 0.18)
		style.set_corner_radius_all(6)
		var p := Panel.new()
		p.custom_minimum_size = Vector2(SLOT_SIZE, SLOT_SIZE)
		p.add_theme_stylebox_override("panel", style)
		p.tooltip_text = _describe(info)
		bar.add_child(p)
		var title := _label(info.name.replace(" ", "\n"), 11)
		title.horizontal_alignment = HORIZONTAL_ALIGNMENT_CENTER
		title.vertical_alignment = VERTICAL_ALIGNMENT_CENTER
		title.size = Vector2(SLOT_SIZE, SLOT_SIZE)
		p.add_child(title)
		var swipe := TextureProgressBar.new()
		swipe.texture_progress = white_tex
		swipe.nine_patch_stretch = true
		swipe.fill_mode = TextureProgressBar.FILL_CLOCKWISE
		swipe.tint_progress = Color(0, 0, 0, 0.65)
		swipe.max_value = 1.0
		swipe.step = 0.001
		swipe.size = Vector2(SLOT_SIZE, SLOT_SIZE)
		swipe.mouse_filter = Control.MOUSE_FILTER_IGNORE
		p.add_child(swipe)
		var key := _label(str(i + 1), 14)
		key.position = Vector2(5, 2)
		p.add_child(key)
		slots.append({"style": style, "swipe": swipe, "flash": 0.0})


func _describe(info: Dictionary) -> String:
	var t := "instant"
	if info.cast_ms > 0:
		t = "%.1fs cast" % (info.cast_ms / 1000.0)
	elif info.channel_ms > 0:
		t = "%.1fs channel" % (info.channel_ms / 1000.0)
	var gcd := "off GCD" if info.gcd_ms == 0 else "GCD"
	var cd := "" if info.cooldown_ms == 0 else ", %ds cooldown" % (info.cooldown_ms / 1000)
	return "%s\n%s, %s%s, %d mana, %d m" % [info.name, t, gcd, cd, info.mana_cost, info.range]


## Pin a control to an anchor point of its parent (0..1 on each axis) with
## a pixel offset from that point.
func _place(c: Control, anchor: Vector2, offset: Vector2, size: Vector2) -> void:
	c.anchor_left = anchor.x
	c.anchor_right = anchor.x
	c.anchor_top = anchor.y
	c.anchor_bottom = anchor.y
	c.offset_left = offset.x
	c.offset_top = offset.y
	c.offset_right = offset.x + size.x
	c.offset_bottom = offset.y + size.y


func _panel(parent: Control, anchor: Vector2, offset: Vector2, size: Vector2) -> PanelContainer:
	var p := PanelContainer.new()
	var sb := StyleBoxFlat.new()
	sb.bg_color = Color(0.05, 0.06, 0.08, 0.8)
	sb.set_corner_radius_all(6)
	sb.set_content_margin_all(10)
	p.add_theme_stylebox_override("panel", sb)
	p.custom_minimum_size = size
	_place(p, anchor, offset, size)
	parent.add_child(p)
	return p


func _label(text: String, size: int) -> Label:
	var l := Label.new()
	l.text = text
	l.add_theme_font_size_override("font_size", size)
	l.add_theme_color_override("font_outline_color", Color.BLACK)
	l.add_theme_constant_override("outline_size", 4)
	l.mouse_filter = Control.MOUSE_FILTER_IGNORE
	return l


func _bar(c: Color) -> Array:
	var b := ProgressBar.new()
	b.show_percentage = false
	b.custom_minimum_size = Vector2(0, 18)
	var bg := StyleBoxFlat.new()
	bg.bg_color = Color(0.12, 0.12, 0.14)
	bg.set_corner_radius_all(3)
	var fill := StyleBoxFlat.new()
	fill.bg_color = c
	fill.set_corner_radius_all(3)
	b.add_theme_stylebox_override("background", bg)
	b.add_theme_stylebox_override("fill", fill)
	b.mouse_filter = Control.MOUSE_FILTER_IGNORE
	return [b, fill]


func _slider_row(parent: Control, title: String, lo: float, hi: float, step: float) -> Array:
	var row := HBoxContainer.new()
	var t := _label(title, 13)
	t.custom_minimum_size = Vector2(130, 0)
	row.add_child(t)
	var s := HSlider.new()
	s.min_value = lo
	s.max_value = hi
	s.step = step
	s.custom_minimum_size = Vector2(110, 0)
	s.size_flags_horizontal = Control.SIZE_EXPAND_FILL
	s.focus_mode = Control.FOCUS_NONE  # keep number keys for the action bar
	row.add_child(s)
	var v := _label("", 13)
	v.custom_minimum_size = Vector2(50, 0)
	row.add_child(v)
	parent.add_child(row)
	return [s, v]

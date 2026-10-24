extends Node3D
## Engine client. Builds the world and UI in code so the whole client is
## readable in one file. Networking, prediction and the combat rules live in
## Rust (CombatClient, from the GDExtension). Everything game-specific comes
## from the tenant: names and numbers from the data pack, models, colours and
## fonts from the asset pack.
##
## Command-line options (after `--`):
##   --login URL          log in through the login service (shows the login
##                        and character screens), e.g. http://127.0.0.1:7000
##   --email E --password P --character NAME
##                        fill in (and with --autotest, submit) those screens
##   --server HOST:PORT   zone server for a dev connection, default 127.0.0.1:5000
##   --assets PATH        tenant asset pack (.zip or .pck) for a dev connection;
##                        with --login it is downloaded from the login service
##   --content PATH       tenant directory or .pack, default ../tenants/default
##   --name NAME          character name for a dev connection
##   --latency MS         initial fake latency (RTT), default 0
##   --loss PERCENT       initial fake loss, default 0
##   --autotest           fight the nearest mob for 10 s, print a summary, quit
##   --screenshot PATH    with --autotest, save a PNG of the window halfway through

const CAMERA_OFFSET := Vector3(0, 17, 15)
const SLOT_SIZE := 64
const TAB_RANGE := 40.0

var client: CombatClient
var server_host := "127.0.0.1"
var server_port := 5000
var content_path := ""
var char_name := ""
var autotest := false
var screenshot_path := ""
var facing := 0.0
var zone_id := ""
var zone_layer := -1
var login_url := ""
var login_email := ""
var login_password := ""
var auto_character := ""

var world: Node3D
var player: Node3D
var camera: Camera3D
var target_ring: MeshInstance3D
var others := {}  # entity id -> Node3D
var theme_colors := {}

# UI
var slots := []
var action_bar: HBoxContainer
var white_tex: ImageTexture
var cast_panel: Control
var cast_bar: ProgressBar
var cast_fill: StyleBoxFlat
var cast_label: Label
var cast_time: Label
var player_name: Label
var hp_bar: ProgressBar
var hp_label: Label
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
var zone_label: Label
var death_panel: Control
var latency_slider: HSlider
var loss_slider: HSlider
var latency_value: Label
var loss_value: Label
var xp_bar: ProgressBar
var xp_label: Label
var party_box: VBoxContainer
var chat_input: LineEdit
var inventory_panel: PanelContainer
var bag_grid: GridContainer
var equip_box: VBoxContainer
var login_panel: PanelContainer
var login_status: Label
var login_fields := {}
var char_list: VBoxContainer
var class_pick: OptionButton
var classes := []
var last_whisper_from := ""
var ui_root: Control
var world_env: Environment
var banner: Label

# Autotest bookkeeping
var auto_elapsed := 0.0
var auto_next_press := 0.0
var auto_slot := 0
var auto_hits := 0
var auto_errors: Array[String] = []
var auto_gaps: Array[float] = []
var auto_walked := 0.0


func _ready() -> void:
	var initial_latency := 0
	var initial_loss := 0.0
	content_path = ProjectSettings.globalize_path("res://").path_join("../tenants/default").simplify_path()
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
			"--content":
				content_path = args[i + 1]
				i += 1
			"--name":
				char_name = args[i + 1]
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
			"--login":
				login_url = args[i + 1]
				i += 1
			"--email":
				login_email = args[i + 1]
				i += 1
			"--password":
				login_password = args[i + 1]
				i += 1
			"--assets":
				_mount_assets(args[i + 1])
				i += 1
			"--character":
				auto_character = args[i + 1]
				i += 1
		i += 1
	if char_name == "":
		char_name = "Hero%d" % (randi() % 1000)

	_build_base()
	_build_ui()

	client = CombatClient.new()
	add_child(client)
	latency_slider.value = initial_latency
	loss_slider.value = initial_loss
	if login_url != "":
		_build_login_screen()
		if autotest:
			_auto_login.call_deferred()
		return
	var err := client.connect_dev(server_host, server_port, content_path, char_name)
	if err != "":
		_show_error("Could not start client: " + err)
		_log("[color=#ff6666]" + err + "[/color]")
		return
	_apply_fake_lag()
	_log("Connecting to %s:%d ..." % [server_host, server_port])


# --------------------------------------------------------------------------
# Frame loop
# --------------------------------------------------------------------------

func _process(delta: float) -> void:
	if client == null:
		return
	client.poll()
	if client.is_online():
		var z := client.zone_info()
		if z.id != zone_id or z.layer != zone_layer:
			_enter_zone(z)

	var correction = client.take_correction()
	if correction != null:
		player.position = correction

	if zone_id != "":
		if player_model_tag == "player":
			_swap_player_model()
		_move_player(delta)
		_sync_entities()
	_handle_events()
	_update_ui()

	if autotest:
		_run_autotest(delta)


func _enter_zone(z: Dictionary) -> void:
	zone_id = z.id
	zone_layer = z.layer
	_apply_fake_lag()
	var t := client.tenant_info()
	theme_colors = t.theme
	_apply_asset_theme()
	_build_zone(z)
	_build_action_bar()
	_swap_player_model()
	zone_label.text = "%s  ·  %s  (layer %d)" % [t.name, z.name, z.layer]
	player.position = client.my_pos()


## The player's own model comes from its class (asset pack tag).
var player_model_tag := "player"
func _swap_player_model() -> void:
	var tag: String = client.my_state().get("model", "")
	if tag == "" or tag == player_model_tag:
		return
	player_model_tag = tag
	var pos := player.position
	player.queue_free()
	player = _make_model(tag, Color(0.3, 0.6, 1.0), 1.0)
	(player.get_node("Plate") as Label3D).text = ""
	player.position = pos
	add_child(player)


func _move_player(delta: float) -> void:
	var st := client.my_state()
	var dir := Vector3.ZERO
	var typing := chat_input != null and chat_input.has_focus()
	if not typing:
		if Input.is_physical_key_pressed(KEY_W): dir.z -= 1
		if Input.is_physical_key_pressed(KEY_S): dir.z += 1
		if Input.is_physical_key_pressed(KEY_A): dir.x -= 1
		if Input.is_physical_key_pressed(KEY_D): dir.x += 1
	if autotest and auto_walked < 1.0 and auto_elapsed > 0.5:
		dir = Vector3(1, 0, 0)  # walk briefly so the test covers movement
		auto_walked += delta
	var dead: bool = st.get("dead", false)
	if dir != Vector3.ZERO and not dead:
		dir = dir.normalized()
		var step := dir * client.move_speed() * delta
		player.position = client.walk(step.x, step.z)
		facing = atan2(dir.x, dir.z)
	else:
		# Standing still: face the target, like a tab-target caster does.
		var t = _entity_pos(client.get_target())
		if t != null:
			var to: Vector3 = t - player.position
			if to.length() > 0.1:
				facing = atan2(to.x, to.z)
		client.set_local_move(player.position, facing, false)
	player.rotation.y = facing
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
		elif k == KEY_R:
			client.release()
		elif k == KEY_B or k == KEY_I:
			inventory_panel.visible = not inventory_panel.visible
			if inventory_panel.visible:
				_refresh_inventory()
		elif k == KEY_ENTER or k == KEY_KP_ENTER:
			chat_input.grab_focus()
			get_viewport().set_input_as_handled()
		elif k == KEY_SLASH:
			chat_input.grab_focus()
			chat_input.text = "/"
			chat_input.caret_column = 1
			get_viewport().set_input_as_handled()
	elif event is InputEventMouseButton and event.pressed and event.button_index == MOUSE_BUTTON_LEFT:
		_click_target(event.position)


func _press(slot: int) -> void:
	client.press(slot)
	var s: Dictionary = slots[slot]
	s.flash = 0.15


func _cycle_target() -> void:
	var t := client.next_target(TAB_RANGE)
	if t >= 0:
		client.set_target(t)


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
	if id == client.my_id():
		return player.position
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
			var col := Color(e.color) if e.kind == "mob" else Color(0.9, 0.75, 0.3)
			others[e.id] = _make_model(e.model, col, e.scale)
			world.add_child(others[e.id])
		var n: Node3D = others[e.id]
		n.position = e.pos
		n.rotation.y = e.facing
		n.rotation.z = PI / 2 if e.dead else 0.0  # corpses lie down
		var plate: Label3D = n.get_node("Plate")
		var line: String = "%s" % e.name if e.passive else "%s  %d" % [e.name, e.level]
		if e.evading:
			line += "  (evading)"
		var hp_pct: float = 0.0 if e.max_hp <= 0 else 100.0 * e.hp / e.max_hp
		if not e.dead and hp_pct < 99.5:
			line += "\n%d%%" % int(round(hp_pct))
		if e.cast_name != "":
			line += "\n%s %d%%" % [e.cast_name, int(e.cast_progress * 100)]
		plate.text = line
		plate.modulate = Color(1, 0.55, 0.5) if e.kind == "mob" and e.in_combat else Color.WHITE
		if e.dead:
			plate.modulate = Color(0.6, 0.6, 0.6)
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
				var verb := "heals" if ev.heal else ("ticks" if ev.periodic else "hits")
				var crit := " [color=#ffd84a](crit)[/color]" if ev.crit else ""
				if ev.mine:
					_log("Your %s %s for [b]%s[/b]%s" % [ev.name, verb, _num(ev.amount), crit])
				elif ev.on_me:
					_log("[color=#ff8080]%s %s you for [b]%s[/b][/color]%s" % [ev.name, verb, _num(ev.amount), crit])
				auto_hits += 1
			"died":
				if ev.me:
					_log("[color=#ff6666]You died. Press R to release.[/color]")
				else:
					_log("%s dies." % ev.name)
			"error":
				_show_error(ev.text)
				auto_errors.append(ev.text)
			"log":
				_log("[color=#9aa4b2]" + ev.text + "[/color]")
			"confirmed":
				if ev.queue_gap_ms >= 0:
					auto_gaps.append(ev.queue_gap_ms)
			"chat":
				_log_chat(ev)
			"xp":
				_log("[color=#c58cf0]You gain %d experience.[/color]" % ev.gained)
				if ev.leveled:
					_show_banner("Level %d!" % ev.level)
			"looted":
				_log("[color=#7fd67f]You receive: %s[/color]" % ev.text)
			"inventory":
				if inventory_panel.visible:
					_refresh_inventory()
			"party":
				_refresh_party()
			"party_invite":
				_log("[color=#6fb6ff]%s invites you to a party. Type /accept to join.[/color]" % ev.from)
			"zone_change":
				_log("[color=#9aa4b2]Travelling to %s ...[/color]" % ev.zone)
				zone_id = ""
				zone_layer = -1


func _floating_text(ev: Dictionary) -> void:
	var pos = _entity_pos(ev.target)
	if pos == null:
		return
	var l := Label3D.new()
	l.text = ("+" if ev.heal else "") + _num(ev.amount) + ("!" if ev.crit else "")
	l.billboard = BaseMaterial3D.BILLBOARD_ENABLED
	l.no_depth_test = true
	l.font_size = 110 if ev.crit else (60 if ev.periodic else 80)
	l.pixel_size = 0.012
	l.outline_size = 16
	if ev.heal:
		l.modulate = Color(0.4, 1, 0.5)
	elif ev.on_me:
		l.modulate = Color(1, 0.3, 0.3)
	else:
		l.modulate = Color(1, 0.85, 0.2) if ev.crit else (Color(1, 0.6, 0.3) if ev.periodic else Color.WHITE)
	l.position = pos + Vector3(randf_range(-0.8, 0.8), 2.6, 0)
	world.add_child(l)
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
	if online:
		status_label.text = "Connected"
	elif client.changing_zone():
		status_label.text = "Travelling ..."
	elif client.disconnect_reason() != "":
		status_label.text = "Disconnected: " + client.disconnect_reason()
	else:
		status_label.text = "Connecting ..."
	status_label.modulate = Color(0.5, 1, 0.6) if online else Color(1, 0.8, 0.4)

	var xp := client.xp_info()
	if not xp.is_empty():
		xp_bar.visible = online
		if xp.next > 0:
			xp_bar.max_value = xp.next
			xp_bar.value = xp.xp
			xp_label.text = "Level %d   %s / %s XP" % [xp.level, _num(xp.xp), _num(xp.next)]
		else:
			xp_bar.max_value = 1
			xp_bar.value = 1
			xp_label.text = "Level %d (max)" % xp.level

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

	var st := client.my_state()
	if not st.is_empty():
		player_name.text = "%s   level %d%s" % [st.name, st.level, "   (in combat)" if st.in_combat else ""]
		hp_bar.max_value = max(st.max_hp, 1.0)
		hp_bar.value = st.hp
		hp_label.text = "%s / %s" % [_num(st.hp), _num(st.max_hp)]
		mana_bar.max_value = max(st.max_mana, 1.0)
		mana_bar.value = st.mana
		mana_label.text = "%d / %d" % [st.mana, st.max_mana]
		death_panel.visible = st.dead

	# Target frame.
	var tid := client.get_target()
	var tdata = null
	for e in client.entities():
		if e.id == tid:
			tdata = e
	target_frame.visible = tdata != null
	if tdata != null:
		target_name.text = tdata.name if tdata.passive else "%s   level %d" % [tdata.name, tdata.level]
		target_hp.max_value = max(tdata.max_hp, 1.0)
		target_hp.value = tdata.hp
		var dist: float = (tdata.pos as Vector3).distance_to(player.position)
		var parts := PackedStringArray(["%s / %s" % [_num(tdata.hp), _num(tdata.max_hp)], "%.1f m" % dist])
		if tdata.dead:
			parts.append("dead")
		for a in tdata.auras:
			parts.append("%s %.0fs" % [a.name, a.remaining_ms / 1000.0])
		target_info.text = "   ".join(parts)
		target_info.modulate = Color(1, 0.5, 0.5) if dist > 30 else Color.WHITE

	var ns := client.net_stats()
	if not ns.is_empty():
		var gap := "-" if ns.last_queue_gap_ms < 0 else "%.2f ms" % ns.last_queue_gap_ms
		stats_label.text = "RTT %d ms   loss %.1f%%   %d in view\nlast spell-queue gap %s" % [ns.rtt_ms, ns.loss * 100.0, ns.entities, gap]

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
	if zone_id == "":
		if auto_elapsed > 10.0:
			print("AUTOTEST FAIL: could not connect")
			get_tree().quit(1)
		return
	if client.get_target() < 0:
		_cycle_target()
		auto_next_press = auto_elapsed + 0.2
		return
	if auto_elapsed >= auto_next_press and auto_walked >= 1.0:
		_press(auto_slot % slots.size())
		auto_slot += 1
		auto_next_press = auto_elapsed + 0.6
	if screenshot_path != "" and auto_elapsed > 3.0:
		get_viewport().get_texture().get_image().save_png(screenshot_path)
		print("AUTOTEST screenshot saved to " + screenshot_path)
		screenshot_path = ""
	if auto_elapsed > 2.5 and login_url != "" and not inventory_panel.visible:
		inventory_panel.visible = true  # show it in the screenshot
		_refresh_inventory()
	if auto_elapsed > 10.0:
		var max_gap := 0.0
		for g in auto_gaps:
			max_gap = max(max_gap, g)
		print("AUTOTEST zone=%s entities=%d hits=%d queued=%d max_queue_gap_ms=%.2f errors=%s" % [zone_id, others.size(), auto_hits, auto_gaps.size(), max_gap, str(auto_errors)])
		var ok := auto_hits > 0 and others.size() > 0
		if login_url != "":
			var inv := client.inventory()
			var items := 0
			for it in inv.bag:
				if it != null:
					items += 1
			var xpi := client.xp_info()
			print("AUTOTEST login mode: level=%d xp=%d/%d bag_items=%d" % [xpi.level, xpi.xp, xpi.next, items])
			ok = ok and items > 0
		print("AUTOTEST " + ("PASS" if ok else "FAIL"))
		client.disconnect_from_server()
		get_tree().quit(0 if ok else 1)


# --------------------------------------------------------------------------
# World construction
# --------------------------------------------------------------------------

func _build_base() -> void:
	var env := WorldEnvironment.new()
	var e := Environment.new()
	world_env = e
	e.background_mode = Environment.BG_COLOR
	e.background_color = Color(0.08, 0.09, 0.12)
	e.ambient_light_source = Environment.AMBIENT_SOURCE_COLOR
	e.ambient_light_color = Color(0.55, 0.6, 0.7)
	e.ambient_light_energy = 0.6
	e.tonemap_mode = Environment.TONE_MAPPER_FILMIC
	env.environment = e
	env.name = "Env"
	add_child(env)

	var sun := DirectionalLight3D.new()
	sun.rotation_degrees = Vector3(-55, 35, 0)
	sun.shadow_enabled = true
	add_child(sun)

	world = Node3D.new()
	add_child(world)

	player = _make_model("player", Color(0.3, 0.6, 1.0), 1.0)
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
	camera.far = 400
	add_child(camera)


## Rebuild ground, walls and portals for the zone we just entered.
func _build_zone(z: Dictionary) -> void:
	for c in world.get_children():
		c.queue_free()
	others.clear()
	var half: Vector2 = z.half_extent
	var ground := MeshInstance3D.new()
	var pm := PlaneMesh.new()
	pm.size = half * 2.0
	ground.mesh = pm
	ground.material_override = _grid_material(Color(z.ground_color))
	world.add_child(ground)
	for b in z.blockers:
		var h: float = b.height
		_add_box(Vector3(b.center.x, h / 2.0, b.center.y), Vector3(b.size.x, h, b.size.y), _theme("wall", Color(0.35, 0.37, 0.42)), world)
	for p in z.portals:
		var ring := MeshInstance3D.new()
		var cyl := CylinderMesh.new()
		cyl.top_radius = p.radius
		cyl.bottom_radius = p.radius
		cyl.height = 0.1
		ring.mesh = cyl
		ring.material_override = _unshaded(_theme("portal", Color(0.4, 0.6, 1.0, 0.6)))
		ring.position = p.pos + Vector3(0, 0.05, 0)
		world.add_child(ring)
		var l := _plate(2.5)
		l.text = "To " + p.name
		l.position = p.pos + Vector3(0, 2.5, 0)
		world.add_child(l)


## Mount a tenant asset pack at res:// so its models, icons and theme
## replace the primitive stand-ins.
func _mount_assets(path: String) -> void:
	if ProjectSettings.load_resource_pack(path, true):
		print("asset pack mounted: " + path)
	else:
		push_warning("could not mount asset pack " + path)


## Colours from the asset pack's theme.json win over the data pack's.
func _apply_asset_theme() -> void:
	var path := "res://tenant/theme.json"
	if FileAccess.file_exists(path):
		var parsed = JSON.parse_string(FileAccess.get_file_as_string(path))
		if parsed is Dictionary:
			for k in parsed:
				theme_colors[k] = parsed[k]
	if theme_colors.has("sky"):
		world_env.background_color = Color(theme_colors.sky)
	if theme_colors.has("ambient"):
		world_env.ambient_light_color = Color(theme_colors.ambient)
	if theme_colors.has("fog"):
		world_env.fog_enabled = true
		world_env.fog_light_color = Color(theme_colors.fog)
		world_env.fog_density = 0.004


## An icon from the asset pack: imported textures, or raw PNGs inside a
## zip pack (which have no import data).
var _icon_cache := {}
func _icon(name: String) -> Texture2D:
	if name == "":
		return null
	if _icon_cache.has(name):
		return _icon_cache[name]
	var path := "res://tenant/icons/%s.png" % name
	var tex: Texture2D = null
	if ResourceLoader.exists(path):
		tex = load(path)
	elif FileAccess.file_exists(path):
		var img := Image.new()
		if img.load_png_from_buffer(FileAccess.get_file_as_bytes(path)) == OK:
			tex = ImageTexture.create_from_image(img)
	_icon_cache[name] = tex
	return tex


func _theme(key: String, fallback: Color) -> Color:
	return Color(theme_colors[key]) if theme_colors.has(key) else fallback


func _grid_material(base: Color) -> ShaderMaterial:
	var sh := Shader.new()
	sh.code = """
shader_type spatial;
uniform vec3 base_color;
void fragment() {
	vec3 w = (INV_VIEW_MATRIX * vec4(VERTEX, 1.0)).xyz;
	vec2 g = abs(fract(w.xz / 4.0 - 0.5) - 0.5) / fwidth(w.xz / 4.0);
	float line = 1.0 - min(min(g.x, g.y), 1.0);
	ALBEDO = mix(base_color, base_color * 1.35, line * 0.6);
	ROUGHNESS = 0.95;
}
"""
	var m := ShaderMaterial.new()
	m.shader = sh
	m.set_shader_parameter("base_color", Vector3(base.r, base.g, base.b))
	return m


func _unshaded(c: Color) -> StandardMaterial3D:
	var m := StandardMaterial3D.new()
	m.shading_mode = BaseMaterial3D.SHADING_MODE_UNSHADED
	m.albedo_color = c
	if c.a < 1.0:
		m.transparency = BaseMaterial3D.TRANSPARENCY_ALPHA
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
	l.font_size = 44
	l.pixel_size = 0.01
	l.outline_size = 12
	return l


## A model by tag. The tenant asset pack provides `res://tenant/models/<tag>.tscn`;
## without one, a primitive stand-in is built from the tag's family.
func _make_model(tag: String, c: Color, s: float) -> Node3D:
	var path := "res://tenant/models/%s.tscn" % tag
	var root: Node3D
	if tag != "" and ResourceLoader.exists(path):
		root = (load(path) as PackedScene).instantiate()
		root.scale = Vector3.ONE * s
		_tint(root, c)
	else:
		root = _primitive_model(tag, c, s)
	root.add_child(_plate(2.4 * s + 0.3))
	return root


func _tint(n: Node, c: Color) -> void:
	if n is MeshInstance3D and n.has_meta("tint"):
		n.material_override = _lit(c)
	for ch in n.get_children():
		_tint(ch, c)


func _primitive_model(tag: String, c: Color, s: float) -> Node3D:
	var root := Node3D.new()
	var body := Node3D.new()
	body.scale = Vector3.ONE * s
	root.add_child(body)
	if tag == "dummy":
		var wood := c
		_add_box(Vector3(0, 0.9, 0), Vector3(0.3, 1.8, 0.3), wood, body)
		_add_box(Vector3(0, 1.4, 0), Vector3(1.6, 0.2, 0.2), wood, body)
		var head := MeshInstance3D.new()
		var sm := SphereMesh.new()
		sm.radius = 0.3
		sm.height = 0.6
		head.mesh = sm
		head.position = Vector3(0, 2.05, 0)
		head.material_override = _lit(wood.lightened(0.3))
		body.add_child(head)
	elif tag.begins_with("quadruped"):
		_add_box(Vector3(0, 0.75, 0), Vector3(0.7, 0.6, 1.6), c, body)
		_add_box(Vector3(0, 1.0, 0.95), Vector3(0.5, 0.45, 0.5), c.lightened(0.15), body)
		for x in [-0.25, 0.25]:
			for z in [-0.6, 0.6]:
				_add_box(Vector3(x, 0.25, z), Vector3(0.18, 0.5, 0.18), c.darkened(0.3), body)
	elif tag == "treant":
		_add_box(Vector3(0, 1.4, 0), Vector3(0.9, 2.8, 0.9), c.darkened(0.3), body)
		var crown := MeshInstance3D.new()
		var sm := SphereMesh.new()
		sm.radius = 1.1
		sm.height = 1.8
		crown.mesh = sm
		crown.position = Vector3(0, 3.0, 0)
		crown.material_override = _lit(c)
		body.add_child(crown)
	else:
		var b := MeshInstance3D.new()
		var cap := CapsuleMesh.new()
		cap.radius = 0.4
		cap.height = 1.8
		b.mesh = cap
		b.position = Vector3(0, 0.9, 0)
		b.material_override = _lit(c)
		body.add_child(b)
		_add_box(Vector3(0, 1.4, 0.4), Vector3(0.25, 0.15, 0.3), Color(0.95, 0.95, 0.95), body)
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
	root.name = "UIRoot"
	root.anchor_right = 1.0
	root.anchor_bottom = 1.0
	root.mouse_filter = Control.MOUSE_FILTER_IGNORE
	ui.add_child(root)
	ui_root = root

	# Player frame, top left: name, health, mana.
	var pf := _panel(root, Vector2(0, 0), Vector2(16, 16), Vector2(280, 96))
	var pv := VBoxContainer.new()
	pf.add_child(pv)
	player_name = _label("", 16)
	pv.add_child(player_name)
	hp_bar = _bar(Color(0.8, 0.2, 0.2))[0]
	pv.add_child(hp_bar)
	hp_label = _label("", 12)
	hp_label.horizontal_alignment = HORIZONTAL_ALIGNMENT_CENTER
	_place(hp_label, Vector2(0, 0), Vector2(0, -1), Vector2(260, 18))
	hp_bar.add_child(hp_label)
	mana_bar = _bar(Color(0.25, 0.45, 1.0))[0]
	pv.add_child(mana_bar)
	mana_label = _label("", 12)
	mana_label.horizontal_alignment = HORIZONTAL_ALIGNMENT_CENTER
	_place(mana_label, Vector2(0, 0), Vector2(0, -1), Vector2(260, 18))
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

	zone_label = _label("", 14)
	_place(zone_label, Vector2(0, 0), Vector2(18, 118), Vector2(400, 20))
	zone_label.modulate = Color(0.8, 0.85, 0.9)
	root.add_child(zone_label)

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

	# Death notice, centre.
	death_panel = _panel(root, Vector2(0.5, 0.5), Vector2(-160, -60), Vector2(320, 80))
	var dl := _label("You died.\nPress R to release to the graveyard.", 18)
	dl.horizontal_alignment = HORIZONTAL_ALIGNMENT_CENTER
	death_panel.add_child(dl)
	death_panel.visible = false

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
	var dp := _panel(root, Vector2(1, 0), Vector2(-356, 16), Vector2(340, 186))
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
	var help := _label("WASD move  Tab/click target  1-5 cast  R release\nB bags  Enter chat (/w /p /world /invite /accept /leave)", 12)
	help.modulate = Color(0.7, 0.75, 0.8)
	dv.add_child(help)

	# Experience bar, bottom centre under the action bar.
	var xb := _bar(_theme("xp", Color(0.69, 0.31, 0.84)))
	xp_bar = xb[0]
	xp_bar.custom_minimum_size = Vector2(520, 14)
	_place(xp_bar, Vector2(0.5, 1), Vector2(-260, -26), Vector2(520, 14))
	root.add_child(xp_bar)
	xp_label = _label("", 11)
	xp_label.horizontal_alignment = HORIZONTAL_ALIGNMENT_CENTER
	_place(xp_label, Vector2(0, 0), Vector2(0, -2), Vector2(520, 14))
	xp_bar.add_child(xp_label)
	xp_bar.visible = false

	# Party members, under the player frame.
	party_box = VBoxContainer.new()
	_place(party_box, Vector2(0, 0), Vector2(18, 144), Vector2(240, 120))
	party_box.mouse_filter = Control.MOUSE_FILTER_IGNORE
	root.add_child(party_box)

	# Chat line under the log.
	chat_input = LineEdit.new()
	chat_input.placeholder_text = "Enter to chat:  /w Name hi   /p party   /world all"
	_place(chat_input, Vector2(0, 1), Vector2(16, -12 - 4), Vector2(440, 0))
	chat_input.offset_top = -14 - 30
	chat_input.offset_bottom = -14
	chat_input.text_submitted.connect(_on_chat_submitted)
	root.add_child(chat_input)
	lp.offset_top -= 34
	lp.offset_bottom -= 34

	# Bags and equipment, right side.
	inventory_panel = _panel(root, Vector2(1, 1), Vector2(-436, -420), Vector2(420, 340))
	var iv := VBoxContainer.new()
	inventory_panel.add_child(iv)
	var ititle := _label("Bags  (left-click: use or equip,  right-click: destroy)", 13)
	iv.add_child(ititle)
	var ih := HBoxContainer.new()
	ih.add_theme_constant_override("separation", 12)
	iv.add_child(ih)
	equip_box = VBoxContainer.new()
	equip_box.custom_minimum_size = Vector2(120, 0)
	ih.add_child(equip_box)
	bag_grid = GridContainer.new()
	bag_grid.columns = 5
	bag_grid.add_theme_constant_override("h_separation", 4)
	bag_grid.add_theme_constant_override("v_separation", 4)
	ih.add_child(bag_grid)
	inventory_panel.visible = false

	# Level-up banner.
	banner = _label("", 40)
	banner.horizontal_alignment = HORIZONTAL_ALIGNMENT_CENTER
	_place(banner, Vector2(0.5, 0.3), Vector2(-300, 0), Vector2(600, 60))
	banner.modulate = Color(1, 0.9, 0.5, 0)
	root.add_child(banner)


func _show_banner(text: String) -> void:
	var b := banner
	b.text = text
	b.modulate.a = 1.0
	var tw := create_tween()
	tw.tween_interval(1.5)
	tw.tween_property(b, "modulate:a", 0.0, 1.0)


const RARITY_COLORS := {
	"poor": Color(0.62, 0.62, 0.62),
	"common": Color(0.95, 0.95, 0.95),
	"uncommon": Color(0.12, 0.9, 0.2),
	"rare": Color(0.2, 0.55, 1.0),
	"epic": Color(0.75, 0.35, 1.0),
}


func _item_tooltip(it: Dictionary) -> String:
	var lines := PackedStringArray([it.name])
	if it.equip != "":
		lines.append(it.equip.capitalize())
	if it.stats != "":
		lines.append(it.stats)
	if it.description != "":
		lines.append(it.description)
	if it.count > 1:
		lines.append("Stack of %d" % it.count)
	return "\n".join(lines)


func _item_button(it, on_left: Callable, on_right: Callable) -> Button:
	var b := Button.new()
	b.custom_minimum_size = Vector2(52, 52)
	b.focus_mode = Control.FOCUS_NONE
	b.clip_text = true
	b.add_theme_font_size_override("font_size", 10)
	var sb := StyleBoxFlat.new()
	sb.bg_color = Color(0.1, 0.11, 0.13)
	sb.set_border_width_all(2)
	sb.set_corner_radius_all(4)
	sb.border_color = Color(0.22, 0.23, 0.27) if it == null else RARITY_COLORS.get(it.rarity, Color.WHITE).darkened(0.2)
	for state in ["normal", "hover", "pressed"]:
		b.add_theme_stylebox_override(state, sb)
	if it == null:
		return b
	var tex := _icon(it.icon)
	if tex != null:
		b.icon = tex
		b.expand_icon = true
	else:
		b.text = it.name.substr(0, 9)
	if it.count > 1:
		b.text += "\n%d" % it.count
	b.add_theme_color_override("font_color", RARITY_COLORS.get(it.rarity, Color.WHITE))
	b.tooltip_text = _item_tooltip(it)
	b.gui_input.connect(func(e):
		if e is InputEventMouseButton and e.pressed:
			if e.button_index == MOUSE_BUTTON_LEFT:
				on_left.call()
			elif e.button_index == MOUSE_BUTTON_RIGHT:
				on_right.call())
	return b


func _refresh_inventory() -> void:
	var inv := client.inventory()
	if inv.is_empty():
		return
	for c in bag_grid.get_children():
		c.queue_free()
	for c in equip_box.get_children():
		c.queue_free()
	for i in inv.bag.size():
		var slot: int = i
		bag_grid.add_child(_item_button(inv.bag[i], func(): client.use_item(slot), func(): client.destroy_item(slot)))
	for i in inv.equip.size():
		var slot: int = i
		var row := HBoxContainer.new()
		var name_l := _label(str(inv.equip_names[i]).capitalize(), 11)
		name_l.custom_minimum_size = Vector2(56, 0)
		row.add_child(name_l)
		row.add_child(_item_button(inv.equip[i], func(): client.unequip(slot), func(): pass))
		equip_box.add_child(row)


func _refresh_party() -> void:
	for c in party_box.get_children():
		c.queue_free()
	var members := client.party()
	if members.is_empty():
		return
	party_box.add_child(_label("Party", 13))
	for m in members:
		var l := _label(("★ " if m.leader else "· ") + m.name, 13)
		l.modulate = Color(0.55, 0.75, 1.0)
		party_box.add_child(l)


func _log_chat(ev: Dictionary) -> void:
	match ev.channel:
		"system":
			_log("[color=#ffd84a]%s[/color]" % ev.text)
		"say":
			_log("[color=#ffffff][%s] says: %s[/color]" % [ev.from, ev.text])
		"world":
			_log("[color=#ffc58c][World] [%s]: %s[/color]" % [ev.from, ev.text])
		"party":
			_log("[color=#8cc8ff][Party] [%s]: %s[/color]" % [ev.from, ev.text])
		"whisper":
			last_whisper_from = ev.from
			_log("[color=#ff8cf0][%s] whispers: %s[/color]" % [ev.from, ev.text])


func _on_chat_submitted(text: String) -> void:
	chat_input.text = ""
	chat_input.release_focus()
	text = text.strip_edges()
	if text == "":
		return
	if not text.begins_with("/"):
		client.chat("say", text, "")
		return
	var parts := text.split(" ", false, 2)
	var cmd := parts[0].to_lower()
	var rest := text.substr(parts[0].length()).strip_edges()
	match cmd:
		"/s", "/say":
			client.chat("say", rest, "")
		"/p", "/party":
			client.chat("party", rest, "")
		"/1", "/world":
			client.chat("world", rest, "")
		"/w", "/whisper", "/tell":
			if parts.size() >= 3:
				client.chat("whisper", parts[2], parts[1])
				_log("[color=#ff8cf0]To [%s]: %s[/color]" % [parts[1], parts[2]])
		"/r":
			if last_whisper_from != "":
				client.chat("whisper", rest, last_whisper_from)
				_log("[color=#ff8cf0]To [%s]: %s[/color]" % [last_whisper_from, rest])
		"/invite", "/inv":
			client.party_invite(rest)
		"/accept":
			client.party_accept()
		"/leave":
			client.party_leave()
		_:
			_show_error("Unknown command " + cmd)


# --------------------------------------------------------------------------
# Login and character screens
# --------------------------------------------------------------------------

func _build_login_screen() -> void:
	login_panel = _panel(ui_root, Vector2(0.5, 0.5), Vector2(-220, -230), Vector2(440, 460))
	var v := VBoxContainer.new()
	v.add_theme_constant_override("separation", 8)
	login_panel.add_child(v)
	var title := _label("Log in", 24)
	v.add_child(title)
	for f in [["url", "Server", login_url], ["email", "Email", login_email], ["password", "Password", login_password]]:
		var row := HBoxContainer.new()
		var l := _label(f[1], 14)
		l.custom_minimum_size = Vector2(90, 0)
		row.add_child(l)
		var e := LineEdit.new()
		e.text = f[2]
		e.size_flags_horizontal = Control.SIZE_EXPAND_FILL
		e.secret = f[0] == "password"
		row.add_child(e)
		v.add_child(row)
		login_fields[f[0]] = e
	var buttons := HBoxContainer.new()
	var lb := Button.new()
	lb.text = "Log in"
	lb.pressed.connect(_do_login)
	buttons.add_child(lb)
	var rb := Button.new()
	rb.text = "Create account"
	rb.pressed.connect(_do_register)
	buttons.add_child(rb)
	v.add_child(buttons)
	login_status = _label("", 13)
	login_status.autowrap_mode = TextServer.AUTOWRAP_WORD
	login_status.custom_minimum_size = Vector2(400, 0)
	v.add_child(login_status)
	v.add_child(HSeparator.new())
	v.add_child(_label("Characters", 18))
	char_list = VBoxContainer.new()
	v.add_child(char_list)
	var create := HBoxContainer.new()
	var name_e := LineEdit.new()
	name_e.placeholder_text = "New character name"
	name_e.size_flags_horizontal = Control.SIZE_EXPAND_FILL
	create.add_child(name_e)
	login_fields["new_name"] = name_e
	class_pick = OptionButton.new()
	create.add_child(class_pick)
	var cb := Button.new()
	cb.text = "Create"
	cb.pressed.connect(_do_create)
	create.add_child(cb)
	v.add_child(create)


func _do_register() -> void:
	var err := client.register(login_fields.url.text, login_fields.email.text, login_fields.password.text)
	login_status.text = err if err != "" else "Account created. Log in."
	if err == "":
		_do_login()


func _do_login() -> void:
	var err := client.login(login_fields.url.text, login_fields.email.text, login_fields.password.text)
	if err != "":
		login_status.text = err
		return
	login_status.text = "Logged in."
	_refresh_characters()


func _refresh_characters() -> void:
	var r := client.characters()
	if r.error != "":
		login_status.text = r.error
		return
	for c in char_list.get_children():
		c.queue_free()
	classes = r.classes
	class_pick.clear()
	for c in classes:
		class_pick.add_item(c.name)
	if r.characters.is_empty():
		char_list.add_child(_label("No characters yet: create one below.", 13))
	for ch in r.characters:
		var b := Button.new()
		b.text = "%s   level %d %s   (%s)" % [ch.name, ch.level, str(ch["class"]).capitalize(), str(ch.zone).capitalize()]
		var id: int = ch.id
		b.pressed.connect(func(): _do_play(id))
		char_list.add_child(b)


func _do_create() -> void:
	if classes.is_empty():
		return
	var cls: String = classes[class_pick.selected].id
	var err := client.create_character(login_fields.new_name.text, cls)
	login_status.text = err if err != "" else "Created " + login_fields.new_name.text
	_refresh_characters()


func _sync_assets() -> String:
	var ap := client.sync_assets(ProjectSettings.globalize_path("user://assets"))
	if ap.begins_with("error"):
		return ap
	if ap != "":
		_mount_assets(ap)
	return ""


func _do_play(id: int) -> void:
	login_status.text = "Entering the world ..."
	var err := _sync_assets()
	if err == "":
		err = client.play(id, content_path)
	if err != "":
		login_status.text = err
		return
	login_panel.visible = false
	_log("Entering the world ...")


## --autotest with --login: register if needed, create the character, play.
func _auto_login() -> void:
	if client.login(login_url, login_email, login_password) != "":
		var err := client.register(login_url, login_email, login_password)
		if err != "":
			print("AUTOTEST FAIL: register: " + err)
			get_tree().quit(1)
			return
		client.login(login_url, login_email, login_password)
	var r := client.characters()
	var pick := -1
	for ch in r.characters:
		if ch.name == auto_character or auto_character == "":
			pick = ch.id
	if pick < 0:
		var err := client.create_character(auto_character if auto_character != "" else "Autotester", r.classes[0].id)
		if err != "":
			print("AUTOTEST FAIL: create: " + err)
			get_tree().quit(1)
			return
		r = client.characters()
		pick = r.characters[0].id
	var err := _sync_assets()
	if err == "":
		err = client.play(pick, content_path)
	if err != "":
		print("AUTOTEST FAIL: play: " + err)
		get_tree().quit(1)
		return
	login_panel.visible = false


func _build_action_bar() -> void:
	if action_bar != null:
		action_bar.queue_free()
	slots.clear()
	action_bar = HBoxContainer.new()
	action_bar.add_theme_constant_override("separation", 8)
	var n := client.bar_count()
	var width := n * SLOT_SIZE + (n - 1) * 8
	_place(action_bar, Vector2(0.5, 1), Vector2(-width / 2.0, -SLOT_SIZE - 40), Vector2(width, SLOT_SIZE))
	cast_panel.get_parent().add_child(action_bar)
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
		action_bar.add_child(p)
		var tex := _icon(info.icon)
		if tex != null:
			var tr := TextureRect.new()
			tr.texture = tex
			tr.size = Vector2(SLOT_SIZE, SLOT_SIZE)
			tr.expand_mode = TextureRect.EXPAND_IGNORE_SIZE
			p.add_child(tr)
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

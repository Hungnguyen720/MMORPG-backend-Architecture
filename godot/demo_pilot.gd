extends Node
## Scripted player for recording gameplay videos (--demo). It plays through
## the real input path: WASD, Tab, 1-5 and B are injected as key events, so
## the client predicts, queues and sends exactly as it does for a person.

const SCALE_3D := 0.75
var main: Node
var caption: Label
var phase := "connect"
var t := 0.0          # seconds in the current phase
var total := 0.0
var held := {}        # Key -> true
var kills := 0
var rotation := [3, 0, 1, 0, 2, 0, 1]
var rot_i := 0
var route := []
var dummy_time := 22.0
var max_kills := 3
var lag_on := false
var reskin := "--reskin" in OS.get_cmdline_user_args()


func _ready() -> void:
	caption = Label.new()
	caption.add_theme_font_size_override("font_size", 26)
	caption.add_theme_color_override("font_outline_color", Color.BLACK)
	caption.add_theme_constant_override("outline_size", 8)
	caption.horizontal_alignment = HORIZONTAL_ALIGNMENT_CENTER
	caption.set_anchors_preset(Control.PRESET_CENTER_TOP)
	caption.position = Vector2(-500, 175)
	caption.size = Vector2(1000, 40)
	main.ui_root.add_child(caption)
	get_viewport().scaling_3d_scale = SCALE_3D


func _process(delta: float) -> void:
	t += delta
	total += delta
	var c: CombatClient = main.client
	if c == null:
		return
	match phase:
		"connect":
			_say("Claude is playing: connecting to the zone server")
			if main.zone_id != "" and t > 2.0:
				_go("walk_dummy")
				route = [main.player.position + Vector3(0, 0, -4)]
				if reskin:
					dummy_time = 16.0
		"walk_dummy":
			_say("Walking to the training dummies (WASD, client-predicted)")
			if _follow_route():
				_go("dummy")
				_tap(KEY_TAB)
		"dummy":
			if reskin:
				_say("Same engine build, reskinned: the Ashfall tenant is only data + an asset pack")
			elif t < dummy_time * 0.5:
				_say("Rotation on a dummy: casts, GCD, spell queue (yellow border), off-GCD Arcane Surge")
			else:
				if not lag_on:
					lag_on = true
					main.latency_slider.value = 150
					main.loss_slider.value = 2.0
				_say("Same rotation with 150 ms fake latency and 2% packet loss")
			_fight(c)
			if t > dummy_time and c.cast_bar().active == false:
				if reskin:
					print("DEMO done reskin")
					get_tree().quit(0)
					return
				_tap(KEY_ESCAPE)
				main.latency_slider.value = 0
				main.loss_slider.value = 0.0
				_go("walk_wolves")
				route = [Vector3(26, 0, -6), Vector3(28, 0, -50), Vector3(44, 0, -55)]
		"walk_wolves":
			_say("Heading east past the ruined wall to the wolf woods")
			var w = _nearest_alive("Forest Wolf")
			if w != null and main.player.position.distance_to(w.pos) < 27.0:
				_release_moves()
				main.client.set_target(w.id)
				_go("wolves")
			elif _follow_route():
				_go("wolves")
		"wolves":
			_say("Pulling Forest Wolves  (kills: %d)" % kills)
			var target := c.get_target()
			var te = _entity(target)
			if te == null or te.dead:
				if te != null and te.dead:
					kills += 1
					rot_i = 0
				var w = _nearest_alive("Forest Wolf", true)
				if w == null:
					var st := c.my_state()
					if kills >= max_kills:
						_go("loot")
						return
					if st.hp < 0.8 * st.max_hp:
						_say("Waiting for health to regenerate before the next pull")
						return
					w = _nearest_alive("Forest Wolf")
					if w == null:
						_go("loot")
						return
				c.set_target(w.id)
				return
			if c.my_state().get("dead", false):
				_go("dead")
				return
			var d: float = main.player.position.distance_to(te.pos)
			if d > 26.0 and not c.cast_bar().active:
				_hold_towards(te.pos)
			else:
				_release_moves()
				_fight(c)
		"dead":
			_say("Claude died. Releasing to the graveyard (R)")
			_release_moves()
			if t > 2.0 and t < 2.1:
				_tap(KEY_R)
			if t > 6.0:
				_go("loot")
		"loot":
			_say("Done: %d wolves down. Opening bags (B) to check the loot" % kills)
			_release_moves()
			if t > 0.5 and not main.inventory_panel.visible:
				_tap(KEY_B)
			if t > 6.0:
				print("DEMO done kills=%d time=%.1f" % [kills, total])
				get_tree().quit(0)
	if total > 170.0:
		print("DEMO timeout")
		get_tree().quit(1)


func _go(p: String) -> void:
	phase = p
	t = 0.0
	print("DEMO phase %s at %.1f" % [p, total])


func _say(s: String) -> void:
	caption.text = s


## Press the next rotation key when the current cast or GCD has under 350 ms
## left, so the spell queue catches it. Arcane Surge is woven off the GCD.
func _fight(c: CombatClient) -> void:
	if c.get_target() < 0:
		if t > 0.5 and fmod(t, 1.0) < 0.05:
			_tap(KEY_TAB)
		return
	if c.swipe(4) <= 0.0 and t > 1.0 and not c.cast_bar().active:
		_tap(KEY_5)
		return
	if c.queued_slot() >= 0:
		return
	var slot: int = rotation[rot_i % rotation.size()]
	var cb := c.cast_bar()
	var cast_left: int = cb.remaining_ms if cb.active else 0
	if cb.active and cb.channel:
		return  # don't clip the channel
	if cast_left < 350 and c.swipe(slot) < 0.23:
		_tap(KEY_1 + slot)
		rot_i += 1


func _follow_route() -> bool:
	while route.size() > 0 and Vector2(main.player.position.x - route[0].x, main.player.position.z - route[0].z).length() < 1.2:
		route.pop_front()
	if route.is_empty():
		_release_moves()
		return true
	_hold_towards(route[0])
	return false


func _hold_towards(p: Vector3) -> void:
	var d := Vector2(p.x - main.player.position.x, p.z - main.player.position.z).normalized()
	var want := {}
	if d.y < -0.38: want[KEY_W] = true
	if d.y > 0.38: want[KEY_S] = true
	if d.x < -0.38: want[KEY_A] = true
	if d.x > 0.38: want[KEY_D] = true
	for k in [KEY_W, KEY_A, KEY_S, KEY_D]:
		_set_key(k, want.has(k))


func _release_moves() -> void:
	for k in [KEY_W, KEY_A, KEY_S, KEY_D]:
		_set_key(k, false)


func _set_key(k: Key, down: bool) -> void:
	if held.get(k, false) == down:
		return
	held[k] = down
	_send(k, down)


func _tap(k: Key) -> void:
	_send(k, true)
	_send(k, false)


func _send(k: Key, down: bool) -> void:
	var ev := InputEventKey.new()
	ev.physical_keycode = k
	ev.keycode = k
	ev.pressed = down
	Input.parse_input_event(ev)


func _entity(id: int):
	if id < 0:
		return null
	for e in main.client.entities():
		if e.id == id:
			return e
	return null


## With attackers_only, just the ones already fighting close by.
func _nearest_alive(name: String, attackers_only := false):
	var best = null
	var best_d := INF
	for e in main.client.entities():
		if e.is_me or e.dead or e.name != name:
			continue
		if attackers_only and (not e.in_combat or main.player.position.distance_to(e.pos) > 15.0):
			continue
		var d: float = main.player.position.distance_to(e.pos)
		if d < best_d:
			best_d = d
			best = e
	return best

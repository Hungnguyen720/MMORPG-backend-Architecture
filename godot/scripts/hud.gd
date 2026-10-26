class_name Hud
extends CanvasLayer
## All 2D UI: unit frames, party (pawn) frames, target frame, cast bar,
## action bar, nameplates, combat log and the fake-lag debug panel.
## Reads state from CombatClient each frame; owns no game rules.

signal fake_lag_changed(latency_ms: int, loss_percent: float)

const SLOT_SIZE := 64
const HOSTILE := Color(1.0, 0.35, 0.3)
const FRIENDLY := Color(0.45, 0.95, 0.55)
const PLAYER := Color(0.45, 0.7, 1.0)
const NEUTRAL := Color(1.0, 0.9, 0.4)

var root: Control
var white_tex: ImageTexture
var slots := []

var cast_panel: Control
var cast_bar: ProgressBar
var cast_fill: StyleBoxFlat
var cast_label: Label
var cast_time: Label

var hp_bar: ProgressBar
var hp_label: Label
var mana_bar: ProgressBar
var mana_label: Label
var party_box: VBoxContainer
var party_rows := {}   # pawn id -> {name, bar, label}

var target_frame: Control
var target_name: Label
var target_hp: ProgressBar
var target_fill: StyleBoxFlat
var target_info: Label
var target_of_target: Label

var error_label: Label
var error_time := 0.0
var log_box: RichTextLabel
var stats_label: Label
var status_label: Label
var latency_slider: HSlider
var loss_slider: HSlider
var latency_value: Label
var loss_value: Label
var death_overlay: Label

var plates_layer: Control
var plates := {}       # entity id -> {root, name, bar, fill}


func _ready() -> void:
	var img := Image.create(8, 8, false, Image.FORMAT_RGBA8)
	img.fill(Color.WHITE)
	white_tex = ImageTexture.create_from_image(img)

	root = Control.new()
	root.anchor_right = 1.0
	root.anchor_bottom = 1.0
	root.mouse_filter = Control.MOUSE_FILTER_IGNORE
	add_child(root)

	plates_layer = Control.new()
	plates_layer.anchor_right = 1.0
	plates_layer.anchor_bottom = 1.0
	plates_layer.mouse_filter = Control.MOUSE_FILTER_IGNORE
	root.add_child(plates_layer)

	_build_player_frame()
	_build_target_frame()
	_build_cast_bar()
	_build_misc()


# --------------------------------------------------------------------------
# Per-frame updates
# --------------------------------------------------------------------------

func update(client: CombatClient, camera: Camera3D, me_pos: Vector3, views: Dictionary,
		entities: Array, server_label: String) -> void:
	var delta := get_process_delta_time()
	var online := client.is_online()
	status_label.text = "Connected" if online else "Connecting to %s ..." % server_label
	status_label.modulate = Color(0.5, 1, 0.6) if online else Color(1, 0.8, 0.4)

	var by_id := {}
	for e in entities:
		by_id[e.id] = e

	# Player frame.
	var hp := client.hp()
	hp_bar.max_value = max(hp.y, 1.0)
	hp_bar.value = hp.x
	hp_label.text = "%s / %s" % [num(hp.x), num(hp.y)]
	var m := client.mana()
	mana_bar.max_value = max(m.y, 1.0)
	mana_bar.value = m.x
	mana_label.text = "%d / %d" % [m.x, m.y]
	death_overlay.visible = client.is_dead()

	_update_party(entities)
	_update_action_bar(client, delta)
	_update_cast_bar(client)
	_update_target(client, by_id, me_pos)
	_update_plates(client, camera, views, entities)

	var st := client.net_stats()
	if not st.is_empty():
		var gap := "-" if st.last_queue_gap_ms < 0 else "%.2f ms" % st.last_queue_gap_ms
		var shots := "-" if st.shots_judged == 0 else "%d / %d" % [st.shots_agreed, st.shots_judged]
		stats_label.text = "RTT %d ms   loss %.1f%%\nlast spell-queue gap %s\nskillshots matching server %s" % [st.rtt_ms, st.loss * 100.0, gap, shots]

	error_time -= delta
	error_label.modulate.a = clamp(error_time, 0.0, 1.0)


func _update_party(entities: Array) -> void:
	var seen := {}
	for e in entities:
		if e.kind != "pawn" or not e.mine:
			continue
		seen[e.id] = true
		if not party_rows.has(e.id):
			var row := VBoxContainer.new()
			row.add_theme_constant_override("separation", 1)
			var name := label(e.name, 13)
			name.add_theme_color_override("font_color", Color(e.color).lightened(0.3))
			row.add_child(name)
			var bx := bar(FRIENDLY.darkened(0.25))
			var b: ProgressBar = bx[0]
			b.custom_minimum_size = Vector2(0, 12)
			row.add_child(b)
			party_box.add_child(row)
			party_rows[e.id] = {"row": row, "name": name, "bar": b}
		var r: Dictionary = party_rows[e.id]
		r.bar.max_value = max(e.max_hp, 1.0)
		r.bar.value = e.hp
		r.name.text = e.name + ("  (down)" if e.dead else "")
	for id in party_rows.keys():
		if not seen.has(id):
			party_rows[id].row.queue_free()
			party_rows.erase(id)


func _update_action_bar(client: CombatClient, delta: float) -> void:
	var queued := client.queued_slot()
	for i in slots.size():
		var s: Dictionary = slots[i]
		s.swipe.value = client.swipe(i)
		s.flash = max(0.0, s.flash - delta)
		var border: StyleBoxFlat = s.style
		if i == queued:
			border.border_color = Color(1, 0.85, 0.2)
		elif s.flash > 0:
			border.border_color = Color.WHITE
		else:
			border.border_color = Color(0.15, 0.15, 0.18)


func _update_cast_bar(client: CombatClient) -> void:
	var cb := client.cast_bar()
	cast_panel.visible = cb.active
	if cb.active:
		cast_fill.bg_color = Color(cb.color)
		cast_bar.value = (1.0 - cb.progress) if cb.channel else cb.progress
		cast_label.text = cb.name
		cast_time.text = "%.1f" % (cb.remaining_ms / 1000.0)


func _update_target(client: CombatClient, by_id: Dictionary, me_pos: Vector3) -> void:
	var tdata = by_id.get(client.get_target())
	target_frame.visible = tdata != null
	if tdata == null:
		return
	target_name.text = tdata.name + ("  (dead)" if tdata.dead else "")
	target_name.add_theme_color_override("font_color", unit_color(tdata))
	target_fill.bg_color = HOSTILE.darkened(0.2) if tdata.hostile else FRIENDLY.darkened(0.25)
	target_hp.max_value = max(tdata.max_hp, 1.0)
	target_hp.value = tdata.hp
	var dist: float = (tdata.pos as Vector3).distance_to(me_pos)
	var parts := PackedStringArray(["%s / %s" % [num(tdata.hp), num(tdata.max_hp)], "%.1f m" % dist])
	for a in tdata.auras:
		parts.append("%s %.0fs" % [a.name, a.remaining_ms / 1000.0])
	target_info.text = "   ".join(parts)
	target_info.modulate = Color(1, 0.5, 0.5) if dist > 30 else Color.WHITE
	# Target of target: who the mob is hitting. Tells you if your tank has it.
	var tot = by_id.get(tdata.target)
	if tot == null or tdata.target == tdata.id:
		target_of_target.text = ""
	else:
		var who: String = "you" if tot.is_me else tot.name
		target_of_target.text = "targeting: " + who
		target_of_target.add_theme_color_override("font_color", unit_color(tot))


func _update_plates(client: CombatClient, camera: Camera3D, views: Dictionary, entities: Array) -> void:
	var seen := {}
	var tid := client.get_target()
	for e in entities:
		if e.is_me or not views.has(e.id):
			continue
		var view: UnitView = views[e.id]
		var head: Vector3 = view.position + Vector3(0, view.head_height() + 0.35, 0)
		if camera.is_position_behind(head):
			continue
		seen[e.id] = true
		if not plates.has(e.id):
			plates[e.id] = _make_plate()
		var p: Dictionary = plates[e.id]
		var sp := camera.unproject_position(head)
		p.root.position = sp - Vector2(60, 26)
		var owner_tag := ""
		if e.kind == "pawn" and not e.mine:
			owner_tag = " (pawn)"
		var cast := ""
		if e.cast_name != "":
			cast = "  %s %d%%" % [e.cast_name, int(e.cast_progress * 100)]
		p.name.text = e.name + owner_tag + cast
		p.name.add_theme_color_override("font_color", unit_color(e))
		p.bar.max_value = max(e.max_hp, 1.0)
		p.bar.value = e.hp
		p.fill.bg_color = HOSTILE.darkened(0.15) if e.hostile else FRIENDLY.darkened(0.2)
		p.bar.visible = not e.dead
		p.root.modulate.a = 0.45 if e.dead else 1.0
		p.root.scale = Vector2.ONE * (1.15 if e.id == tid else 1.0)
	for id in plates.keys():
		plates[id].root.visible = seen.has(id)
		if not views.has(id):
			plates[id].root.queue_free()
			plates.erase(id)


func _make_plate() -> Dictionary:
	var box := VBoxContainer.new()
	box.size = Vector2(120, 26)
	box.add_theme_constant_override("separation", 1)
	box.mouse_filter = Control.MOUSE_FILTER_IGNORE
	box.pivot_offset = Vector2(60, 26)
	var n := label("", 12)
	n.horizontal_alignment = HORIZONTAL_ALIGNMENT_CENTER
	n.custom_minimum_size = Vector2(120, 0)
	box.add_child(n)
	var bx := bar(HOSTILE)
	var b: ProgressBar = bx[0]
	b.custom_minimum_size = Vector2(120, 6)
	box.add_child(b)
	plates_layer.add_child(box)
	return {"root": box, "name": n, "bar": b, "fill": bx[1]}


func unit_color(e: Dictionary) -> Color:
	if e.hostile:
		return NEUTRAL if e.kind == "dummy" else HOSTILE
	if e.kind == "pawn":
		return FRIENDLY if e.mine else FRIENDLY.darkened(0.3)
	return PLAYER


func show_error(text: String) -> void:
	error_label.text = text
	error_time = 2.0


func log_line(bb: String) -> void:
	log_box.append_text(bb + "\n")


func flash_slot(i: int) -> void:
	if i < slots.size():
		slots[i].flash = 0.15


func set_fake_lag(latency_ms: int, loss_percent: float) -> void:
	latency_slider.value = latency_ms
	loss_slider.value = loss_percent
	_on_lag_slider()


# --------------------------------------------------------------------------
# Construction
# --------------------------------------------------------------------------

func _build_player_frame() -> void:
	var pf := panel(root, Vector2(0, 0), Vector2(16, 16), Vector2(260, 0))
	var pv := VBoxContainer.new()
	pv.add_theme_constant_override("separation", 3)
	pf.add_child(pv)
	pv.add_child(label("You", 18))
	hp_bar = bar(Color(0.25, 0.75, 0.3))[0]
	pv.add_child(hp_bar)
	hp_label = _bar_text(hp_bar)
	mana_bar = bar(Color(0.25, 0.45, 1.0))[0]
	pv.add_child(mana_bar)
	mana_label = _bar_text(mana_bar)
	var ph := label("Pawns", 12)
	ph.modulate = Color(0.7, 0.75, 0.8)
	pv.add_child(ph)
	party_box = VBoxContainer.new()
	party_box.add_theme_constant_override("separation", 4)
	pv.add_child(party_box)


func _bar_text(b: ProgressBar) -> Label:
	var l := label("", 12)
	l.horizontal_alignment = HORIZONTAL_ALIGNMENT_CENTER
	place(l, Vector2(0, 0), Vector2(0, -1), Vector2(240, 18))
	b.add_child(l)
	return l


func _build_target_frame() -> void:
	target_frame = panel(root, Vector2(0.5, 0), Vector2(-190, 16), Vector2(380, 0))
	var tv := VBoxContainer.new()
	target_frame.add_child(tv)
	target_name = label("", 18)
	tv.add_child(target_name)
	var tb := bar(HOSTILE)
	target_hp = tb[0]
	target_fill = tb[1]
	tv.add_child(target_hp)
	target_info = label("", 13)
	tv.add_child(target_info)
	target_of_target = label("", 13)
	tv.add_child(target_of_target)
	target_frame.visible = false


func _build_cast_bar() -> void:
	cast_panel = Control.new()
	place(cast_panel, Vector2(0.5, 1), Vector2(-170, -SLOT_SIZE - 90), Vector2(340, 26))
	cast_panel.mouse_filter = Control.MOUSE_FILTER_IGNORE
	root.add_child(cast_panel)
	var cbx := bar(Color.WHITE)
	cast_bar = cbx[0]
	cast_fill = cbx[1]
	cast_bar.custom_minimum_size = Vector2(340, 26)
	cast_bar.max_value = 1.0
	cast_panel.add_child(cast_bar)
	cast_label = label("", 15)
	cast_label.position = Vector2(8, 2)
	cast_panel.add_child(cast_label)
	cast_time = label("", 15)
	cast_time.position = Vector2(296, 2)
	cast_panel.add_child(cast_time)
	cast_panel.visible = false


func _build_misc() -> void:
	error_label = label("", 22)
	place(error_label, Vector2(0.5, 0), Vector2(-300, 140), Vector2(600, 30))
	error_label.horizontal_alignment = HORIZONTAL_ALIGNMENT_CENTER
	error_label.modulate = Color(1, 0.3, 0.3, 0)
	error_label.add_theme_color_override("font_color", Color(1, 0.35, 0.3))
	root.add_child(error_label)

	death_overlay = label("You died. Respawning at the plaza...", 30)
	place(death_overlay, Vector2(0.5, 0.5), Vector2(-300, -60), Vector2(600, 40))
	death_overlay.horizontal_alignment = HORIZONTAL_ALIGNMENT_CENTER
	death_overlay.add_theme_color_override("font_color", Color(0.95, 0.3, 0.25))
	death_overlay.visible = false
	root.add_child(death_overlay)

	var lp := panel(root, Vector2(0, 1), Vector2(16, -236), Vector2(440, 220))
	log_box = RichTextLabel.new()
	log_box.bbcode_enabled = true
	log_box.scroll_following = true
	log_box.add_theme_font_size_override("normal_font_size", 13)
	log_box.add_theme_font_size_override("bold_font_size", 13)
	log_box.custom_minimum_size = Vector2(420, 200)
	log_box.focus_mode = Control.FOCUS_NONE
	log_box.mouse_filter = Control.MOUSE_FILTER_IGNORE
	lp.add_child(log_box)

	var dp := panel(root, Vector2(1, 0), Vector2(-356, 16), Vector2(340, 0))
	var dv := VBoxContainer.new()
	dp.add_child(dv)
	status_label = label("", 14)
	dv.add_child(status_label)
	var lat := _slider_row(dv, "Fake latency (RTT)", 0, 250, 10)
	latency_slider = lat[0]
	latency_value = lat[1]
	var loss := _slider_row(dv, "Fake packet loss", 0, 5, 0.5)
	loss_slider = loss[0]
	loss_value = loss[1]
	latency_slider.value_changed.connect(func(_v): _on_lag_slider())
	loss_slider.value_changed.connect(func(_v): _on_lag_slider())
	stats_label = label("", 13)
	dv.add_child(stats_label)
	var help := label(
		"WASD / hold right-click: move    Tab / left-click: target\n" +
		"1-5: cast at target    6-7: aim at cursor    Space: dodge\n" +
		"F: pawns attack    G: pawns regroup\n" +
		"Esc: clear target    Wheel: zoom", 12)
	help.modulate = Color(0.7, 0.75, 0.8)
	dv.add_child(help)


func _on_lag_slider() -> void:
	latency_value.text = "%d ms" % latency_slider.value
	loss_value.text = "%.1f %%" % loss_slider.value
	fake_lag_changed.emit(int(latency_slider.value), loss_slider.value)


## Action bar plus the two pawn command buttons.
func build_action_bar(client: CombatClient) -> void:
	var bar_box := HBoxContainer.new()
	bar_box.add_theme_constant_override("separation", 8)
	var n := client.ability_count()
	var extra := 2
	var width := (n + extra) * SLOT_SIZE + (n + extra) * 8 + 16
	place(bar_box, Vector2(0.5, 1), Vector2(-width / 2.0, -SLOT_SIZE - 40), Vector2(width, SLOT_SIZE))
	root.add_child(bar_box)
	for i in n:
		var info := client.ability_info(i)
		var key := "Space" if info.delivery == "dash" else str(i + 1)
		var s := _slot(bar_box, info.name, key, Color(info.color), _describe(info))
		slots.append(s)
	var gap := Control.new()
	gap.custom_minimum_size = Vector2(16, 0)
	bar_box.add_child(gap)
	_slot(bar_box, "Pawns\nattack", "F", FRIENDLY.darkened(0.2), "Pawns attack your target")
	_slot(bar_box, "Pawns\nregroup", "G", FRIENDLY.darkened(0.45), "Pawns break off and return to you")


func _slot(parent: Control, title_text: String, key_text: String, c: Color, tip: String) -> Dictionary:
	var style := StyleBoxFlat.new()
	style.bg_color = c.darkened(0.35)
	style.set_border_width_all(3)
	style.border_color = Color(0.15, 0.15, 0.18)
	style.set_corner_radius_all(6)
	var p := Panel.new()
	p.custom_minimum_size = Vector2(SLOT_SIZE, SLOT_SIZE)
	p.add_theme_stylebox_override("panel", style)
	p.tooltip_text = tip
	parent.add_child(p)
	var title := label(title_text.replace(" ", "\n"), 11)
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
	var key := label(key_text, 14)
	key.position = Vector2(5, 2)
	p.add_child(key)
	return {"style": style, "swipe": swipe, "flash": 0.0}


func _describe(info: Dictionary) -> String:
	var t := "instant"
	if info.cast_ms > 0:
		t = "%.1fs cast" % (info.cast_ms / 1000.0)
	elif info.channel_ms > 0:
		t = "%.1fs channel" % (info.channel_ms / 1000.0)
	var gcd := "off GCD" if info.gcd_ms == 0 else "GCD"
	var cd := "" if info.cooldown_ms == 0 else ", %ds cooldown" % (info.cooldown_ms / 1000)
	var aim: String = {"skillshot": "\nSkillshot: fires at the cursor", "ground": "\nGround: lands at the cursor",
		"dash": "\nDodge: toward where you walk"}.get(info.delivery, "")
	return "%s\n%s, %s%s, %d mana, %d m%s" % [info.name, t, gcd, cd, info.mana_cost, info.range, aim]


# --------------------------------------------------------------------------
# Widget helpers
# --------------------------------------------------------------------------

## Pin a control to an anchor point of its parent (0..1 on each axis) with
## a pixel offset from that point.
static func place(c: Control, anchor: Vector2, offset: Vector2, size: Vector2) -> void:
	c.anchor_left = anchor.x
	c.anchor_right = anchor.x
	c.anchor_top = anchor.y
	c.anchor_bottom = anchor.y
	c.offset_left = offset.x
	c.offset_top = offset.y
	c.offset_right = offset.x + size.x
	c.offset_bottom = offset.y + size.y


static func panel(parent: Control, anchor: Vector2, offset: Vector2, size: Vector2) -> PanelContainer:
	var p := PanelContainer.new()
	var sb := StyleBoxFlat.new()
	sb.bg_color = Color(0.05, 0.06, 0.08, 0.8)
	sb.set_corner_radius_all(6)
	sb.set_content_margin_all(10)
	p.add_theme_stylebox_override("panel", sb)
	p.custom_minimum_size = size
	p.mouse_filter = Control.MOUSE_FILTER_IGNORE
	place(p, anchor, offset, size)
	parent.add_child(p)
	return p


static func label(text: String, size: int) -> Label:
	var l := Label.new()
	l.text = text
	l.add_theme_font_size_override("font_size", size)
	l.add_theme_color_override("font_outline_color", Color.BLACK)
	l.add_theme_constant_override("outline_size", 4)
	l.mouse_filter = Control.MOUSE_FILTER_IGNORE
	return l


static func bar(c: Color) -> Array:
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


static func _slider_row(parent: Control, title: String, lo: float, hi: float, step: float) -> Array:
	var row := HBoxContainer.new()
	var t := label(title, 13)
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
	var v := label("", 13)
	v.custom_minimum_size = Vector2(50, 0)
	row.add_child(v)
	parent.add_child(row)
	return [s, v]


static func num(v: float) -> String:
	var s := str(int(round(v)))
	var out := ""
	while s.length() > 3:
		out = "," + s.substr(s.length() - 3) + out
		s = s.substr(0, s.length() - 3)
	return s + out

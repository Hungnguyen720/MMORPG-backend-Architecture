class_name IsoCamera
extends Camera3D
## Fixed isometric-style follow camera (Diablo / Albion framing). The yaw
## never changes in play, so screen-up is always the same world direction
## and WASD, click-to-move and any future aimed abilities share one basis.

const YAW_DEG := 45.0
const PITCH_DEG := 52.0
const MIN_DIST := 12.0
const MAX_DIST := 36.0
const FOLLOW_SHARPNESS := 12.0

var distance := 26.0
var _focus := Vector3.ZERO


func _ready() -> void:
	fov = 40.0
	near = 0.5
	far = 300.0


## Snap to `pos` with no smoothing (spawn, respawn, teleport).
func snap_to(pos: Vector3) -> void:
	_focus = pos
	_apply()


## Smoothly follow `pos`. Call once per frame.
func follow(pos: Vector3, delta: float) -> void:
	_focus = _focus.lerp(pos, 1.0 - exp(-FOLLOW_SHARPNESS * delta))
	_apply()


func zoom(steps: float) -> void:
	distance = clamp(distance + steps * 2.0, MIN_DIST, MAX_DIST)


func _apply() -> void:
	var yaw := deg_to_rad(YAW_DEG)
	var pitch := deg_to_rad(PITCH_DEG)
	var back := Vector3(sin(yaw) * cos(pitch), sin(pitch), cos(yaw) * cos(pitch))
	var look := _focus + Vector3(0, 1.0, 0)
	position = look + back * distance
	look_at(look, Vector3.UP)


## World direction that screen-up / screen-right point at, on the ground.
func ground_forward() -> Vector3:
	var f := -global_transform.basis.z
	f.y = 0
	return f.normalized()


func ground_right() -> Vector3:
	var r := global_transform.basis.x
	r.y = 0
	return r.normalized()


## Where the cursor ray meets the ground plane (y = 0), or null.
func ground_point(screen_pos: Vector2):
	var from := project_ray_origin(screen_pos)
	var dir := project_ray_normal(screen_pos)
	if abs(dir.y) < 1e-4:
		return null
	var t := -from.y / dir.y
	if t < 0:
		return null
	return from + dir * t

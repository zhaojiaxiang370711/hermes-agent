extends RefCounted
## Optional semantic-action adapter, with no networking or automatic game hookup.
## The host explicitly enables it, binds one existing moment() arbiter, and opens
## each authoritative conversation/turn scope before submitting director events.
## invalidate() cancels admission, not animation: hand/photo state is untouched.

const EVENT_TYPE := "character.action"
const EVENT_SCHEMA := "huahuo.character.action.v1"
const ACTION_PRIORITY := 60
const MAX_ID_LENGTH := 128
const MAX_JSON_INTEGER := 9007199254740991
const ACTION_CLIPS := {
	"greet": "introduction",
	"explain": "guiding",
	"celebrate": "excited",
	"approve": "thumbs_up",
	"think": "thinking",
	"shrug": "shrug",
	"apologize": "facepalm",
	"surprise": "surprised_lean",
}
const EVENT_FIELDS := [
	"type", "schema", "request_id", "character_id", "conversation_id", "turn_id", "action",
]

var enabled := false:
	set(value):
		if enabled and not value:
			invalidate()
		enabled = value

var _character_id := ""
var _actions: Object
var _conversation_id := ""
var _turn_id := -1
var _seen: Dictionary = {}


func bind(character_id: String, actions: Object) -> bool:
	## One binding only; a successful rebind invalidates the former turn scope.
	## The supplied arbiter must implement moment(clip, now, priority) -> bool.
	if not _valid_id(character_id) or not is_instance_valid(actions):
		return false
	if not actions.has_method("moment"):
		return false
	_character_id = character_id
	_actions = actions
	invalidate()
	return true


func begin_scope(conversation_id: String, turn_id: int) -> bool:
	## Call only with host/director-owned scope, never fields copied from an event.
	## Reopening the current scope preserves deduplication; a new scope replaces it.
	if not _valid_id(conversation_id) or turn_id < 0 or turn_id > MAX_JSON_INTEGER:
		invalidate()
		return false
	if conversation_id == _conversation_id and turn_id == _turn_id:
		return true
	_conversation_id = conversation_id
	_turn_id = turn_id
	_seen.clear()
	return true


func invalidate() -> void:
	## Use on interrupt, stop, disconnect, actor switch, or host availability loss.
	## Already-started gestures remain owned by the original action arbiter.
	_conversation_id = ""
	_turn_id = -1
	_seen.clear()


func submit(event: Variant, now: float) -> Dictionary:
	## Results are JSON-safe. "started" means moment() admitted playback, not that
	## the clip finished. A refused request is not queued or retried by this bridge.
	var request_id := ""
	if event is Dictionary and _valid_id(event.get("request_id")):
		request_id = event.request_id
	if not enabled:
		return _result(request_id, "rejected", "disabled")
	if not _valid_envelope(event) or not is_finite(now) or now < 0.0:
		return _result(request_id, "rejected", "invalid_payload")
	if not ACTION_CLIPS.has(event.action):
		return _result(request_id, "rejected", "unsupported_action")
	if event.character_id != _character_id:
		return _result(request_id, "rejected", "target_inactive")
	if _conversation_id.is_empty() or event.conversation_id != _conversation_id \
			or int(event.turn_id) != _turn_id:
		return _result(request_id, "rejected", "stale_turn")
	if _seen.has(request_id):
		return _result(request_id, "rejected", "duplicate")
	# Remember refused attempts too: a replay cannot turn a busy rejection into
	# a delayed gesture. A deliberate retry needs a new request_id in this scope.
	_seen[request_id] = true
	if not is_instance_valid(_actions) or not _actions.has_method("moment"):
		return _result(request_id, "rejected", "unavailable")
	var started: Variant = _actions.call("moment", ACTION_CLIPS[event.action], now, ACTION_PRIORITY)
	if not started is bool:
		return _result(request_id, "rejected", "unavailable")
	if not started:
		return _result(request_id, "rejected", "busy")
	return _result(request_id, "started", "")


static func _valid_envelope(event: Variant) -> bool:
	if not event is Dictionary or event.size() != EVENT_FIELDS.size():
		return false
	for field in EVENT_FIELDS:
		if not event.has(field):
			return false
	# The exact field set also rejects raw clip/priority/path injection.
	if event.type != EVENT_TYPE or event.schema != EVENT_SCHEMA:
		return false
	for field in ["request_id", "character_id", "conversation_id", "action"]:
		if not _valid_id(event[field]):
			return false
	return _valid_turn_id(event.turn_id)


static func _valid_id(value: Variant) -> bool:
	return value is String and not value.is_empty() \
		and value.length() <= MAX_ID_LENGTH and value == value.strip_edges()


static func _valid_turn_id(value: Variant) -> bool:
	if value is int:
		return value >= 0 and value <= MAX_JSON_INTEGER
	# Godot's JSON decoder reads all JSON numbers as floats. Accept mathematical
	# integers only, within the exact cross-language JSON integer range.
	if value is float:
		return is_finite(value) and value >= 0.0 \
			and value <= MAX_JSON_INTEGER and floor(value) == value
	return false


static func _result(request_id: String, status: String, reason: String) -> Dictionary:
	return {"type": "character.action.result", "request_id": request_id,
		"status": status, "reason": reason}

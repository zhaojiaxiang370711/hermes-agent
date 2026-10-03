extends SceneTree
## Executes the real adapter against a synthetic arbiter with a real
## AnimationPlayer. It verifies the moment() contract, not the current game,
## proprietary Paimon assets, renderer, camera pipeline, or voice transport.

const Bridge = preload("res://character_action_bridge.gd")
const EXPECTED_CLIPS := {
	"greet": "introduction", "explain": "guiding", "celebrate": "excited",
	"approve": "thumbs_up", "think": "thinking", "shrug": "shrug",
	"apologize": "facepalm", "surprise": "surprised_lean",
}

class SyntheticArbiter extends Node:
	var player := AnimationPlayer.new()
	var calls: Array[Dictionary] = []
	var current := ""
	var event_until := 0.0
	var event_priority := 0
	var hand_state := "holding"
	var photo_phase := ""
	var reset_calls := 0

	func _ready() -> void:
		add_child(player)
		var library := AnimationLibrary.new()
		for clip in EXPECTED_CLIPS.values():
			if library.has_animation(clip):
				continue
			var animation := Animation.new()
			animation.length = 0.5
			library.add_animation(clip, animation)
		player.add_animation_library("", library)

	func moment(clip: String, now: float, priority := 50) -> bool:
		calls.append({"clip": clip, "now": now, "priority": priority})
		if not player.has_animation(clip):
			return false
		if photo_phase == "analyzing" and priority < 100:
			return false
		if now < event_until and priority < event_priority:
			return false
		player.play(clip)
		player.advance(0.0)
		current = clip
		event_until = now + player.get_animation(clip).length
		event_priority = priority
		return true

	func reset(_now: float) -> void:
		reset_calls += 1
		hand_state = "home"
		photo_phase = ""
		player.stop()


var failures := 0
var checks := 0


func _initialize() -> void:
	call_deferred("_run")


func check(ok: bool, message: String) -> void:
	checks += 1
	if not ok:
		failures += 1
		push_error(message)


func event(request_id := "request-1", conversation_id := "conversation-a", turn_id: Variant = 1,
		action := "greet", character_id := "paimon") -> Dictionary:
	return {"type": "character.action", "schema": "huahuo.character.action.v1",
		"request_id": request_id, "character_id": character_id,
		"conversation_id": conversation_id, "turn_id": turn_id, "action": action}


func expect_result(result: Dictionary, status: String, reason: String, message: String) -> void:
	check(result.status == status and result.reason == reason, message + ": " + JSON.stringify(result))
	var decoded: Variant = JSON.parse_string(JSON.stringify(result))
	check(decoded is Dictionary and decoded == result, "Every result round-trips through JSON")


func _run() -> void:
	var arbiter := SyntheticArbiter.new()
	root.add_child(arbiter)
	var bridge := Bridge.new()
	check(bridge.bind("paimon", arbiter), "Bind the actual moment() consumer")
	check(bridge.begin_scope("conversation-a", 1), "Host opens an explicit turn scope")
	expect_result(bridge.submit(event(), 1.0), "rejected", "disabled", "Disabled by default")
	check(arbiter.calls.is_empty(), "Disabled events never call the animation arbiter")
	bridge.enabled = true
	_test_actions(bridge, arbiter)
	_test_invalid_events(bridge, arbiter)
	_test_scope_and_duplicates(bridge, arbiter)
	_test_busy_and_invalidate(bridge, arbiter)
	_test_binding_and_unavailable(bridge)
	arbiter.queue_free()
	await process_frame
	print("CHARACTER_ACTION_BRIDGE_TEST checks=", checks, " failures=", failures)
	quit(0 if failures == 0 else 1)


func _test_actions(bridge, arbiter: SyntheticArbiter) -> void:
	var now := 10.0
	for action in EXPECTED_CLIPS:
		var command := event("action-" + action, "conversation-a", 1, action)
		# Exercise the same decoding boundary as a JSON event from Python.
		var decoded: Variant = JSON.parse_string(JSON.stringify(command))
		expect_result(bridge.submit(decoded, now), "started", "", "Admit semantic action " + action)
		check(arbiter.current == EXPECTED_CLIPS[action]
			and arbiter.player.current_animation == EXPECTED_CLIPS[action],
			"A semantic action calls moment() and starts the real AnimationPlayer")
		check(arbiter.calls.back().priority == 60 and arbiter.event_priority == 60,
			"All agent gestures use fixed priority 60")
		now += 1.0
	check(arbiter.reset_calls == 0 and arbiter.hand_state == "holding",
		"Semantic gestures do not reset hand state")


func _test_invalid_events(bridge, arbiter: SyntheticArbiter) -> void:
	var invalid: Array = [null, [], "not an event"]
	for field in ["type", "schema", "request_id", "character_id", "conversation_id", "turn_id", "action"]:
		var missing := event("missing-" + field)
		missing.erase(field)
		invalid.append(missing)
	for field in ["request_id", "character_id", "conversation_id", "action"]:
		for value in [null, 7, "", " padded ", "a".repeat(129)]:
			var bad_id := event("bad-id-" + field)
			bad_id[field] = value
			invalid.append(bad_id)
	for value in [true, false, "1", -1, 1.5, NAN, INF, 9007199254740992]:
		invalid.append(event("bad-turn", "conversation-a", value))
	for value in [60, 100, 1.5, "100", true, NAN]:
		var injected_priority := event("injected-priority")
		injected_priority["priority"] = value
		invalid.append(injected_priority)
	for field in ["clip", "path", "script"]:
		var injected := event("injected-" + field)
		injected[field] = "res://anything.gd"
		invalid.append(injected)
	var bad_type := event("bad-type")
	bad_type.type = "character.other"
	invalid.append(bad_type)
	var bad_schema := event("bad-schema")
	bad_schema.schema = "huahuo.character.action.v2"
	invalid.append(bad_schema)
	var before := arbiter.calls.size()
	for command in invalid:
		expect_result(bridge.submit(command, 40.0), "rejected", "invalid_payload",
			"Malformed/injected event is rejected before playback")
	for now in [-1.0, NAN, INF]:
		expect_result(bridge.submit(event("bad-time"), now), "rejected", "invalid_payload",
			"Host time must be finite and nonnegative")
	expect_result(bridge.submit(event("unknown-action", "conversation-a", 1, "play_clip"), 40.0),
		"rejected", "unsupported_action", "The model cannot select arbitrary clips")
	expect_result(bridge.submit(event("wrong-target", "conversation-a", 1, "greet", "mofang"), 40.0),
		"rejected", "target_inactive", "An unbound character is not impersonated by the current rig")
	check(arbiter.calls.size() == before, "Rejected malformed events do not call moment()")


func _test_scope_and_duplicates(bridge, arbiter: SyntheticArbiter) -> void:
	check(bridge.begin_scope("conversation-b", 7), "Replace the host scope")
	var command := event("scope-request", "conversation-b", 7, "approve")
	expect_result(bridge.submit(command, 50.0), "started", "", "Current host scope is accepted")
	var before := arbiter.calls.size()
	expect_result(bridge.submit(command, 51.0), "rejected", "duplicate", "A request cannot replay its gesture")
	check(bridge.begin_scope("conversation-b", 7), "Opening the same scope is idempotent")
	expect_result(bridge.submit(command, 52.0), "rejected", "duplicate", "An idempotent scope open retains deduplication")
	check(arbiter.calls.size() == before, "Duplicate events never call moment()")
	bridge.begin_scope("conversation-b", 8)
	expect_result(bridge.submit(command, 53.0), "rejected", "stale_turn", "A superseded turn is refused")
	expect_result(bridge.submit(event("old-conversation", "conversation-a", 8), 53.0),
		"rejected", "stale_turn", "Another conversation cannot borrow the current turn number")
	command.turn_id = 8
	expect_result(bridge.submit(command, 54.0), "started", "", "Deduplication is scoped to one authoritative turn")
	check(not bridge.begin_scope("conversation-b", -1), "Invalid turn scope fails closed")
	expect_result(bridge.submit(command, 55.0), "rejected", "stale_turn", "An invalid scope clears former admission")
	check(not bridge.begin_scope("", 8), "Empty conversation scope fails closed")
	check(not bridge.begin_scope("conversation-b", 9007199254740992), "Inexact JSON turn IDs fail closed")
	bridge.begin_scope("conversation-b", 8)
	bridge.enabled = false
	bridge.enabled = true
	expect_result(bridge.submit(command, 56.0), "rejected", "stale_turn", "Disabling requires a fresh scope when re-enabled")


func _test_busy_and_invalidate(bridge, arbiter: SyntheticArbiter) -> void:
	bridge.begin_scope("conversation-c", 20)
	check(arbiter.moment("surprised_lean", 60.0, 80), "Hand gesture owns the arbiter at priority 80")
	var command := event("busy-request", "conversation-c", 20)
	var before := arbiter.calls.size()
	expect_result(bridge.submit(command, 60.1), "rejected", "busy", "A hand gesture outranks priority 60")
	check(arbiter.calls.size() == before + 1 and arbiter.calls.back().priority == 60
		and arbiter.current == "surprised_lean", "Busy is the real arbiter result, not a queued action")
	expect_result(bridge.submit(command, 61.0), "rejected", "duplicate", "A busy request cannot replay after the hand gesture ends")
	command.request_id = "retry-request"
	expect_result(bridge.submit(command, 61.0), "started", "", "A deliberate fresh request can run after the arbiter releases")
	arbiter.photo_phase = "analyzing"
	check(arbiter.moment("thinking", 62.0, 100), "Photo analysis owns the arbiter")
	command.request_id = "photo-request"
	expect_result(bridge.submit(command, 62.1), "rejected", "busy", "Photo analysis cannot be interrupted by an agent gesture")
	var saved_current := arbiter.current
	var saved_until := arbiter.event_until
	var saved_priority := arbiter.event_priority
	before = arbiter.calls.size()
	bridge.invalidate()
	check(arbiter.hand_state == "holding" and arbiter.photo_phase == "analyzing"
		and arbiter.reset_calls == 0 and arbiter.current == saved_current
		and arbiter.event_until == saved_until and arbiter.event_priority == saved_priority,
		"Scope cancellation never resets or steals hand/photo animation state")
	command.request_id = "late-after-interrupt"
	expect_result(bridge.submit(command, 63.0), "rejected", "stale_turn", "Late action after invalidate cannot reach the rig")
	check(arbiter.calls.size() == before, "Invalidated late events never call moment()")
	arbiter.photo_phase = ""
	bridge.begin_scope("conversation-c", 21)
	command.turn_id = 21
	expect_result(bridge.submit(command, 64.0), "started", "", "An explicit new scope admits fresh work")


func _test_binding_and_unavailable(bridge) -> void:
	var consumer := SyntheticArbiter.new()
	root.add_child(consumer)
	check(bridge.bind("paimon-next", consumer), "Rebind one consumer explicitly")
	expect_result(bridge.submit(event("new-binding", "conversation-c", 21, "greet", "paimon-next"), 70.0),
		"rejected", "stale_turn", "A rebind invalidates previous scope")
	bridge.begin_scope("conversation-d", 1)
	expect_result(bridge.submit(event("old-binding", "conversation-d", 1), 70.0),
		"rejected", "target_inactive", "Only one character binding is active")
	var invalid_consumer := Node.new()
	root.add_child(invalid_consumer)
	check(not bridge.bind("bad", invalid_consumer), "A consumer without moment() cannot bind")
	check(not bridge.bind("", consumer), "An empty character ID cannot bind")
	expect_result(bridge.submit(event("new-binding", "conversation-d", 1, "greet", "paimon-next"), 70.0),
		"started", "", "A failed bind leaves the previous valid consumer intact")
	consumer.free()
	expect_result(bridge.submit(event("freed-consumer", "conversation-d", 1, "greet", "paimon-next"), 71.0),
		"rejected", "unavailable", "A freed consumer fails without invoking a stale object")
	invalid_consumer.queue_free()

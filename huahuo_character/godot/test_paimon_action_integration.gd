extends SceneTree
## Cross-repository contract test: the supplied game arbiter is real, while
## AnimationPlayer clips are synthetic and Python demo replies may be mocked.
## No assets, camera services, network client or live-game scene are loaded.
## Run with -- --paimon-actions-path <paimon_actions.gd> --fixture <demo.json>.

const Bridge = preload("res://character_action_bridge.gd")

var checks := 0
var failures := 0
var fixture_events := 0


func _initialize() -> void:
	call_deferred("_run")


func _options() -> Dictionary:
	var arguments := OS.get_cmdline_user_args()
	var options := {}
	if arguments.size() != 4:
		printerr("Required: --paimon-actions-path <paimon_actions.gd> --fixture <demo.json>")
		return {}
	for index in range(0, arguments.size(), 2):
		var option := arguments[index]
		if option not in ["--paimon-actions-path", "--fixture"] or options.has(option):
			printerr("Unknown or repeated integration-test option: ", option)
			return {}
		options[option] = arguments[index + 1]
	return options


func check(ok: bool, message: String) -> void:
	checks += 1
	if not ok:
		failures += 1
		push_error(message)


func expect_result(result: Dictionary, status: String, reason: String, message: String) -> void:
	check(result.status == status and result.reason == reason, message + ": " + JSON.stringify(result))
	check(JSON.parse_string(JSON.stringify(result)) == result, "Bridge result round-trips through JSON")


func _run() -> void:
	var options := _options()
	if options.is_empty():
		quit(2)
		return
	var actions_path: String = options["--paimon-actions-path"]
	var fixture_path: String = options["--fixture"]
	if not FileAccess.file_exists(actions_path) or not FileAccess.file_exists(fixture_path):
		printerr("Integration inputs must be existing files")
		quit(2)
		return
	var actions_script := load(actions_path) as Script
	if actions_script == null or not actions_script.can_instantiate():
		printerr("Could not load the supplied Paimon arbiter")
		quit(2)
		return
	var fixture: Variant = JSON.parse_string(FileAccess.get_file_as_string(fixture_path))
	if not fixture is Dictionary or not fixture.get("actions") is Array \
			or fixture.actions.is_empty() or fixture.get("discarded") != false \
			or not fixture.get("conversation_id") is String \
			or not Bridge._valid_turn_id(fixture.get("turn_id")):
		printerr("Fixture must be a non-discarded Python demo TurnResult with actions")
		quit(2)
		return
	# Only Paimon has this clip contract. Other configured characters are not
	# attached to a Paimon rig. The binding below never registers a live actor.
	for event in fixture.actions:
		if not Bridge._valid_envelope(event) or not Bridge.ACTION_CLIPS.has(event.action):
			check(false, "Fixture contains an invalid semantic action event")
			continue
		if event.character_id != "paimon":
			continue
		check(event.conversation_id == fixture.conversation_id and event.turn_id == fixture.turn_id,
			"Python event belongs to the host-owned fixture scope")
		_exercise_event(actions_script, event, fixture.conversation_id, int(fixture.turn_id))
		fixture_events += 1
	check(fixture_events > 0, "The Python fixture includes at least one Paimon action")
	await process_frame
	print("PAIMON_ACTION_INTEGRATION_TEST fixture_events=", fixture_events,
		" checks=", checks, " failures=", failures)
	quit(0 if failures == 0 else 1)


func _open_bridge(actions, character_id: String, conversation_id: String, turn_id: int):
	var bridge := Bridge.new()
	check(not bridge.enabled, "The game adapter remains disabled by default")
	check(bridge.bind(character_id, actions), "Host explicitly binds one target to the real arbiter")
	check(bridge.begin_scope(conversation_id, turn_id), "Host explicitly opens the fixture turn")
	bridge.enabled = true
	return bridge


func _exercise_event(actions_script: Script, event: Dictionary, conversation_id: String, turn_id: int) -> void:
	var actions = actions_script.new()
	var model := Node3D.new()
	var player := AnimationPlayer.new()
	var library := AnimationLibrary.new()
	var clips: Array = actions_script.get_script_constant_map().get("CLIPS", [])
	check(not clips.is_empty(), "The real arbiter exposes its required clip library")
	for clip in clips:
		var animation := Animation.new()
		animation.length = 0.75
		library.add_animation(clip, animation)
	player.add_animation_library("", library)
	model.add_child(player)
	root.add_child(model)
	if not actions.configure(model):
		check(false, "The real arbiter configures the synthetic AnimationPlayer")
		model.queue_free()
		return
	check(actions.player == player and actions.current == "initial_pose",
		"The supplied arbiter owns and initializes the real AnimationPlayer")
	for clip in clips:
		var expected_loop := Animation.LOOP_LINEAR if clip in actions.LOOPED else Animation.LOOP_NONE
		check(player.get_animation(clip).loop_mode == expected_loop,
			"The real arbiter configures the loop policy for " + clip)
	var bridge = _open_bridge(actions, event.character_id, conversation_id, turn_id)
	expect_result(bridge.submit(event, 1.0), "started", "", "Consume the original Python action event")
	check(actions.current == Bridge.ACTION_CLIPS[event.action]
		and player.current_animation == Bridge.ACTION_CLIPS[event.action] and player.is_playing(),
		"The semantic event starts playback through the real moment() method")
	check(actions.event_priority == 60 and is_equal_approx(actions.event_until, 1.75),
		"The real arbiter admits agent priority 60 with the actual animation duration")
	expect_result(bridge.submit(event, 1.1), "rejected", "duplicate", "The fixture event is deduplicated")

	# Reset is fixture setup only. The adapter never calls the game's reset().
	actions.reset(10.0)
	actions.hand("grabbed", "neutral", 10.0)
	check(actions.hand_state == "grabbed" and actions.event_priority == 80
		and player.current_animation == "surprised_lean", "A real grab gesture owns priority 80")
	bridge = _open_bridge(actions, event.character_id, conversation_id, turn_id)
	expect_result(bridge.submit(event, 10.1), "rejected", "busy", "The agent cannot preempt a real grab")
	check(actions.current == "surprised_lean" and actions.event_priority == 80
		and is_equal_approx(actions.event_until, 10.75), "A refused agent gesture preserves grab playback")
	bridge.invalidate()
	check(actions.hand_state == "grabbed" and actions.event_priority == 80
		and player.current_animation == "surprised_lean", "Invalidation does not reset the game's hand state")
	expect_result(bridge.submit(event, 10.2), "rejected", "stale_turn", "A late event stays rejected after invalidate")

	actions.photo("analysis_started", 20.0)
	check(actions.photo_phase == "analyzing" and actions.event_priority == 0
		and player.current_animation == "thinking", "Real photo analysis owns the thinking loop")
	bridge = _open_bridge(actions, event.character_id, conversation_id, turn_id)
	expect_result(bridge.submit(event, 20.1), "rejected", "busy", "Photo analysis rejects priority 60 even with no timed gesture")
	check(actions.photo_phase == "analyzing" and actions.current == "thinking"
		and player.current_animation == "thinking", "The real photo-analysis guard preserves playback")
	bridge.invalidate()
	check(actions.photo_phase == "analyzing" and actions.hand_state == "grabbed"
		and actions.event_priority == 0 and player.current_animation == "thinking",
		"Invalidation leaves game photo and hand state untouched")
	expect_result(bridge.submit(event, 21.0), "rejected", "stale_turn", "A late fixture event cannot reopen photo scope")
	actions.update(21.0)
	check(actions.photo_phase == "analyzing" and player.current_animation == "thinking",
		"The game's normal update continues photo analysis after invalidation")
	model.queue_free()

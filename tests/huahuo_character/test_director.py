"""Behavior tests for isolation, bounded dialogue and interruption."""

import asyncio
import json
import unittest

from huahuo_character.director import Director
from huahuo_character.protocol import (
    ActionEvent, AgentReply, CharacterSpec, MAX_TEXT_CHARS, MAX_TURN_ID, ProtocolError,
)


def characters():
    return (
        CharacterSpec("alpha", "huahuo-alpha", "ALPHA_PRIVATE_PERSONA"),
        CharacterSpec("beta", "huahuo-beta", "BETA_PRIVATE_PERSONA"),
    )


class RecordingBackend:
    def __init__(self, replies=None):
        self.calls = []
        self.replies = replies or {}

    async def respond(self, spec, session_id, text):
        self.calls.append((spec, session_id, text))
        return self.replies.get(spec.character_id, AgentReply("A public reply", "greet"))


class LateBackend(RecordingBackend):
    """Deliberately simulates an I/O backend which returns after cancellation."""

    def __init__(self):
        super().__init__()
        self.started = asyncio.Event()
        self.cancelled = asyncio.Event()
        self.release = asyncio.Event()

    async def respond(self, spec, session_id, text):
        self.calls.append((spec, session_id, text))
        if text == "slow":
            self.started.set()
            try:
                await self.release.wait()
            except asyncio.CancelledError:
                self.cancelled.set()
                await self.release.wait()
            return AgentReply("This reply arrived too late", "celebrate", "beta")
        return AgentReply("Current reply", "approve")


class ProtocolTests(unittest.TestCase):
    def test_models_cannot_supply_identity_priority_or_resource_paths(self):
        for field in ("character_id", "conversation_id", "turn_id", "priority", "resource_path"):
            with self.subTest(field=field), self.assertRaises(ProtocolError):
                AgentReply.from_dict({"text": "Hello", field: "malicious"})
        for action in ("res://animations/dance", {"name": "greet", "priority": 99}, "dance"):
            with self.subTest(action=action), self.assertRaises(ProtocolError):
                AgentReply.from_dict({"text": "Hello", "action": action})

    def test_malformed_and_excessive_payloads_are_rejected(self):
        for payload in ([], {}, {"text": " "}, {"text": 123}, {"text": "x" * 8193}):
            with self.subTest(payload_type=type(payload)), self.assertRaises(ProtocolError):
                AgentReply.from_dict(payload)
        for character_id in ("../alpha", "", "alpha;run", "a" * 65):
            with self.subTest(character_id=character_id), self.assertRaises(ProtocolError):
                CharacterSpec(character_id, "profile", "Persona")

    def test_character_profiles_and_ids_cannot_be_reused(self):
        backend = RecordingBackend()
        with self.assertRaises(ProtocolError):
            Director(
                [CharacterSpec("alpha", "shared", "A"), CharacterSpec("beta", "shared", "B")],
                backend,
            )
        with self.assertRaises(ProtocolError):
            Director([characters()[0], characters()[0]], backend)

    def test_json_text_must_cross_the_utf8_transport_boundary(self):
        for escaped in ("\\ud800", "\\udfff"):
            payload = json.loads('{"text":"' + escaped + '"}')
            with self.subTest(escaped=escaped), self.assertRaises(ProtocolError):
                AgentReply.from_dict(payload)
        reply = AgentReply.from_dict(json.loads('{"text":"\\ud83d\\ude00你好"}'))
        encoded = json.dumps({"text": reply.text}, ensure_ascii=False).encode("utf-8")
        self.assertEqual(json.loads(encoded)["text"], reply.text)

    def test_session_identity_is_stable_and_scoped(self):
        one = Director(characters(), RecordingBackend())
        restarted = Director(characters(), RecordingBackend())
        original = one.session_id_for("alpha", "user-one", "scene-one")
        self.assertEqual(original, restarted.session_id_for("alpha", "user-one", "scene-one"))
        for character_id, user_id, scene in (
            ("beta", "user-one", "scene-one"),
            ("alpha", "user-two", "scene-one"),
            ("alpha", "user-one", "scene-two"),
        ):
            self.assertNotEqual(original, one.session_id_for(character_id, user_id, scene))
        self.assertNotEqual(
            one.conversation_id_for("user-one"), one.conversation_id_for("user-two")
        )

    def test_interaction_budget_cannot_be_unbounded_or_noninteger(self):
        for budget in (-1, 5, True, 1.5):
            with self.subTest(budget=budget), self.assertRaises(ProtocolError):
                Director(characters(), RecordingBackend(), max_interactions=budget)

    def test_action_turn_id_survives_godot_json_integer_conversion(self):
        for turn_id in (0, True, 1.5, MAX_TURN_ID + 1):
            with self.subTest(turn_id=turn_id), self.assertRaises(ProtocolError):
                ActionEvent("request-one", "alpha", "scene-one", turn_id, "greet")
        event = ActionEvent("request-one", "alpha", "scene-one", MAX_TURN_ID, "greet")
        self.assertEqual(int(float(event.to_dict()["turn_id"])), event.turn_id)


class DirectorTests(unittest.IsolatedAsyncioTestCase):
    async def test_character_and_user_sessions_are_kept_separate(self):
        backend = RecordingBackend()
        director = Director(characters(), backend)
        first = await director.respond("alpha", "user-one", "First input")
        second = await director.respond("alpha", "user-one", "Second input")
        other_character = await director.respond("beta", "user-one", "Other character")
        other_user = await director.respond("alpha", "user-two", "Other user")
        sessions = [call[1] for call in backend.calls]
        self.assertEqual(sessions[0], sessions[1])
        self.assertNotEqual(sessions[0], sessions[2])
        self.assertNotEqual(sessions[0], sessions[3])
        self.assertEqual(first.conversation_id, other_character.conversation_id)
        self.assertNotEqual(first.conversation_id, other_user.conversation_id)
        self.assertLess(first.turn_id, second.turn_id)
        self.assertLess(second.turn_id, other_character.turn_id)

    async def test_director_alone_stamps_action_scope(self):
        backend = RecordingBackend({"alpha": AgentReply("Hello", "greet", "beta")})
        director = Director(characters(), backend, max_interactions=1)
        result = await director.respond("alpha", "user-one", "Say hello", conversation_id="stage")
        self.assertEqual([item.character_id for item in result.utterances], ["alpha", "beta"])
        self.assertEqual(len({item.request_id for item in result.actions}), len(result.actions))
        for utterance, action in zip(result.utterances, result.actions):
            payload = action.to_dict()
            self.assertEqual(payload["character_id"], utterance.character_id)
            self.assertEqual(payload["conversation_id"], result.conversation_id)
            self.assertEqual(payload["turn_id"], result.turn_id)
            self.assertEqual(payload["type"], "character.action")
            self.assertEqual(payload["schema"], "huahuo.character.action.v1")
            self.assertEqual(utterance.action_request_id, action.request_id)
            self.assertEqual(type(payload["turn_id"]), int)
            self.assertNotIn("priority", payload)
            self.assertNotIn("resource_path", payload)
        self.assertEqual(result.to_dict()["actions"], [item.to_dict() for item in result.actions])

    async def test_relay_passes_only_public_utterance_to_recipient(self):
        backend = RecordingBackend(
            {
                "alpha": AgentReply("PUBLIC_ALPHA_SAYS_HELLO", target_character_id="beta"),
                "beta": AgentReply("PUBLIC_BETA_REPLY"),
            }
        )
        director = Director(characters(), backend)
        await director.respond("beta", "user-two", "USER_TWO_PRIVATE_INPUT")
        await director.respond("alpha", "user-one", "USER_ONE_PRIVATE_INPUT")
        recipient_spec, recipient_session, prompt = backend.calls[-1]
        self.assertEqual(recipient_spec.character_id, "beta")
        self.assertEqual(recipient_spec.persona, "BETA_PRIVATE_PERSONA")
        self.assertEqual(
            json.loads(prompt),
            {
                "type": "scene.utterance",
                "character_id": "alpha",
                "text": "PUBLIC_ALPHA_SAYS_HELLO",
            },
        )
        for private in ("ALPHA_PRIVATE_PERSONA", "USER_ONE_PRIVATE_INPUT", "USER_TWO_PRIVATE_INPUT"):
            self.assertNotIn(private, prompt)
        self.assertNotEqual(recipient_session, backend.calls[0][1])

    async def test_mutual_character_replies_stop_at_budget(self):
        backend = RecordingBackend(
            {
                "alpha": AgentReply("Alpha replies", "explain", "beta"),
                "beta": AgentReply("Beta replies", "think", "alpha"),
            }
        )
        director = Director(characters(), backend, max_interactions=2)
        result = await director.respond("alpha", "user-one", "Discuss this")
        self.assertEqual([item.character_id for item in result.utterances], ["alpha", "beta", "alpha"])
        self.assertEqual(len(backend.calls), 3)
        self.assertEqual(len(result.actions), 3)
        self.assertEqual(backend.calls[0][1], backend.calls[2][1])

    async def test_repeated_speaker_without_action_does_not_steal_later_action(self):
        class SequenceBackend:
            def __init__(self, replies):
                self.replies = iter(replies)

            async def respond(self, spec, session_id, text):
                return next(self.replies)

        for first_action, last_action in ((None, "greet"), ("think", None)):
            with self.subTest(first_action=first_action, last_action=last_action):
                backend = SequenceBackend([
                    AgentReply("Alpha first", first_action, "beta"),
                    AgentReply("Beta replies", "explain", "alpha"),
                    AgentReply("Alpha last", last_action),
                ])
                result = await Director(characters(), backend, max_interactions=2).respond(
                    "alpha", "user-one", "Discuss"
                )
                actions = {action.request_id: action for action in result.actions}
                for utterance, expected in zip(result.utterances, (first_action, "explain", last_action)):
                    if expected is None:
                        self.assertIsNone(utterance.action_request_id)
                        self.assertNotIn("action_request_id", utterance.to_dict())
                    else:
                        linked = actions[utterance.action_request_id]
                        self.assertEqual(linked.action, expected)
                        self.assertEqual(linked.character_id, utterance.character_id)
                        self.assertEqual(len(linked.to_dict()), 7)

    async def test_large_public_reply_has_bounded_valid_json_relay(self):
        public_text = "\x00" * MAX_TEXT_CHARS
        backend = RecordingBackend({"alpha": AgentReply(public_text, target_character_id="beta")})
        result = await Director(characters(), backend, max_interactions=1).respond(
            "alpha", "user-one", "Give a lengthy answer"
        )
        prompt = backend.calls[1][2]
        self.assertLessEqual(len(prompt), MAX_TEXT_CHARS)
        excerpt = json.loads(prompt)
        self.assertTrue(excerpt["truncated"])
        self.assertGreater(len(excerpt["text"]), 0)
        self.assertLess(len(excerpt["text"]), len(public_text))
        self.assertTrue(public_text.startswith(excerpt["text"]))
        self.assertEqual(result.utterances[0].text, public_text)

    async def test_zero_budget_still_allows_initial_user_reply(self):
        backend = RecordingBackend({"alpha": AgentReply("A reply", target_character_id="beta")})
        result = await Director(characters(), backend, max_interactions=0).respond(
            "alpha", "user-one", "Hello"
        )
        self.assertEqual(len(backend.calls), 1)
        self.assertEqual([item.character_id for item in result.utterances], ["alpha"])

    async def test_disallowed_character_action_is_rejected(self):
        backend = RecordingBackend({"alpha": AgentReply("Celebrate!", "celebrate")})
        director = Director(
            [CharacterSpec("alpha", "alpha-profile", "Reserved", frozenset({"greet"}))], backend
        )
        with self.assertRaises(ProtocolError):
            await director.respond("alpha", "user-one", "Hello")

    async def test_unknown_or_self_target_is_rejected(self):
        for target in ("unknown", "alpha"):
            with self.subTest(target=target), self.assertRaises(ProtocolError):
                await Director(
                    characters(), RecordingBackend({"alpha": AgentReply("Hello", "greet", target)})
                ).respond("alpha", "user-one", "Hello")

    async def test_invalid_input_does_not_start_backend(self):
        backend = RecordingBackend()
        director = Director(characters(), backend)
        with self.assertRaises(ProtocolError):
            await director.respond("unknown", "user-one", "Hello")
        with self.assertRaises(ProtocolError):
            await director.respond("alpha", "../user", "Hello")
        with self.assertRaises(ProtocolError):
            await director.respond("alpha", "user-one", " ")
        self.assertEqual(backend.calls, [])

    async def test_explicit_interrupt_discards_late_reply_and_does_not_relay(self):
        backend = LateBackend()
        director = Director(characters(), backend)
        old = asyncio.create_task(director.respond("alpha", "user-one", "slow"))
        await asyncio.wait_for(backend.started.wait(), 3)
        director.interrupt("user-one")
        await asyncio.wait_for(backend.cancelled.wait(), 3)
        backend.release.set()
        result = await asyncio.wait_for(old, 3)
        self.assertTrue(result.discarded)
        self.assertEqual(result.utterances, ())
        self.assertEqual(result.actions, ())
        self.assertEqual(len(backend.calls), 1)

    async def test_replacement_waits_for_same_session_and_emits_only_current_turn(self):
        backend = LateBackend()
        director = Director(characters(), backend)
        old = asyncio.create_task(director.respond("alpha", "user-one", "slow"))
        await asyncio.wait_for(backend.started.wait(), 3)
        new = asyncio.create_task(director.respond("alpha", "user-one", "new input"))
        await asyncio.wait_for(backend.cancelled.wait(), 3)
        self.assertEqual(len(backend.calls), 1)
        backend.release.set()
        previous, current = await asyncio.wait_for(asyncio.gather(old, new), 3)
        self.assertTrue(previous.discarded)
        self.assertFalse(current.discarded)
        self.assertEqual([item.text for item in current.utterances], ["Current reply"])
        self.assertGreater(current.turn_id, previous.turn_id)
        self.assertEqual(backend.calls[0][1], backend.calls[1][1])
        self.assertTrue(all(item.turn_id == current.turn_id for item in current.actions))

    async def test_switching_character_invalidates_previous_speaker(self):
        backend = LateBackend()
        director = Director(characters(), backend)
        old = asyncio.create_task(director.respond("alpha", "user-one", "slow"))
        await asyncio.wait_for(backend.started.wait(), 3)
        current = await director.respond("beta", "user-one", "Address beta instead")
        await asyncio.wait_for(backend.cancelled.wait(), 3)
        self.assertEqual([item.character_id for item in current.utterances], ["beta"])
        backend.release.set()
        previous = await asyncio.wait_for(old, 3)
        self.assertTrue(previous.discarded)
        self.assertEqual(previous.actions, ())

    async def test_caller_cancellation_cannot_be_swallowed_by_backend(self):
        backend = LateBackend()
        director = Director(characters(), backend)
        old = asyncio.create_task(director.respond("alpha", "user-one", "slow"))
        await asyncio.wait_for(backend.started.wait(), 3)
        old.cancel()
        await asyncio.wait_for(backend.cancelled.wait(), 3)
        backend.release.set()
        with self.assertRaises(asyncio.CancelledError):
            await asyncio.wait_for(old, 3)
        # The cancelled reply must not invoke its requested beta relay, and
        # the persisted-session lock must still admit the next interaction.
        self.assertEqual(len(backend.calls), 1)
        current = await director.respond("alpha", "user-one", "New input")
        self.assertFalse(current.discarded)
        self.assertEqual([item.text for item in current.utterances], ["Current reply"])
        self.assertTrue(all(item.turn_id == current.turn_id for item in current.actions))

    async def test_other_user_does_not_interrupt_ongoing_user(self):
        backend = LateBackend()
        director = Director(characters(), backend, max_interactions=0)
        old = asyncio.create_task(director.respond("alpha", "user-one", "slow"))
        await asyncio.wait_for(backend.started.wait(), 3)
        other = await director.respond("alpha", "user-two", "Another user speaks")
        self.assertFalse(backend.cancelled.is_set())
        backend.release.set()
        previous = await asyncio.wait_for(old, 3)
        self.assertFalse(previous.discarded)
        self.assertNotEqual(previous.conversation_id, other.conversation_id)
        self.assertNotEqual(backend.calls[0][1], backend.calls[1][1])


if __name__ == "__main__":
    unittest.main()

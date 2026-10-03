"""Exercise the persistent Director through real authenticated loopback HTTP."""

from __future__ import annotations

import argparse
import asyncio
from concurrent.futures import ThreadPoolExecutor
import http.client
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
from pathlib import Path
import socket
import tempfile
import threading
import unittest
from unittest.mock import patch

from huahuo_character.hermes_backend import HermesBackend
from huahuo_character.protocol import AgentReply, CharacterSpec
from huahuo_character.server import CharacterServer, server_from_args


TOKEN = "new-synthetic-character-server-token-only"
SPECS = (CharacterSpec("alpha", "profile-alpha", "Alpha"), CharacterSpec("beta", "profile-beta", "Beta"))


class RecordingBackend:
    def __init__(self):
        self.calls = []

    async def respond(self, spec, session_id, text):
        self.calls.append((spec.character_id, session_id, text))
        return AgentReply("Reply to " + text, "greet")


class BlockingBackend(RecordingBackend):
    def __init__(self, ignore_cancel=False):
        super().__init__()
        self.started = threading.Event()
        self.cancelled = threading.Event()
        self.release = asyncio.Event()
        self.ignore_cancel = ignore_cancel

    async def respond(self, spec, session_id, text):
        self.calls.append((spec.character_id, session_id, text))
        if text == "slow":
            self.started.set()
            try:
                await self.release.wait()
            except asyncio.CancelledError:
                self.cancelled.set()
                if self.ignore_cancel:
                    await self.release.wait()
                else:
                    raise
        return AgentReply("Reply to " + text, "greet")


class CharacterServerTests(unittest.TestCase):
    def setUp(self):
        self.backend = RecordingBackend()
        self.server = CharacterServer(SPECS, self.backend, TOKEN, max_interactions=0).start()
        self.pool = ThreadPoolExecutor(max_workers=4)

    def tearDown(self):
        if isinstance(self.backend, BlockingBackend):
            self.server._loop.call_soon_threadsafe(self.backend.release.set)
        self.server.close()
        self.pool.shutdown(wait=True)

    def change_backend(self, backend, **options):
        self.server.close()
        self.backend = backend
        self.server = CharacterServer(SPECS, backend, TOKEN, max_interactions=0, **options).start()

    def request(self, path="/v1/turn", payload=None, *, token=TOKEN, method="POST", body=None, headers=None):
        connection = http.client.HTTPConnection("127.0.0.1", self.server._http.server_port, timeout=8.0)
        request_headers = {"Content-Type": "application/json"}
        if token is not None:
            request_headers["Authorization"] = "Bearer " + token
        if headers:
            request_headers.update(headers)
        if body is None and payload is not None:
            body = json.dumps(payload, ensure_ascii=False).encode("utf-8")
        try:
            connection.request(method, path, body=body, headers=request_headers)
            response = connection.getresponse()
            raw = response.read()
            return response.status, json.loads(raw)
        finally:
            connection.close()

    @staticmethod
    def turn(text="hello", user="user-one", character="alpha", scene="scene-one"):
        return {"character_id": character, "user_id": user, "conversation_id": scene, "text": text}

    def test_health_is_nonsecret_and_turns_require_the_new_bearer(self):
        code, health = self.request("/health", method="GET", token=None)
        self.assertEqual(code, 200)
        self.assertEqual(health["instance_id"], self.server.instance_id)
        self.assertEqual(health["status"], "ready")
        self.assertNotIn(TOKEN, json.dumps(health))
        for token in (None, "wrong-secret"):
            with self.subTest(token_supplied=token is not None):
                code, data = self.request(payload=self.turn(), token=token)
                self.assertEqual(code, 401)
                self.assertNotIn("wrong-secret", json.dumps(data))
        self.assertEqual(self.backend.calls, [])

    def test_process_reuses_director_and_keeps_character_user_scene_sessions_separate(self):
        _, first = self.request(payload=self.turn("first"))
        _, second = self.request(payload=self.turn("second"))
        _, other_character = self.request(payload=self.turn(character="beta"))
        _, other_user = self.request(payload=self.turn(user="user-two"))
        _, other_scene = self.request(payload=self.turn(scene="scene-two"))
        sessions = [call[1] for call in self.backend.calls]
        self.assertEqual(sessions[0], sessions[1])
        self.assertEqual(len(set(sessions)), 4)
        self.assertLess(first["turn_id"], second["turn_id"])
        self.assertEqual(second["conversation_id"], other_character["conversation_id"])
        self.assertNotEqual(second["conversation_id"], other_user["conversation_id"])
        self.assertNotEqual(second["conversation_id"], other_scene["conversation_id"])
        for data in (first, second):
            self.assertEqual(data["instance_id"], self.server.instance_id)
            self.assertLessEqual(len(data["conversation_id"]), 64)
            action = data["actions"][0]
            self.assertEqual(action["conversation_id"], data["conversation_id"])
            self.assertEqual(action["request_id"], data["utterances"][0]["action_request_id"])
            self.assertEqual(len(action), 7)

    def test_restart_changes_playback_scope_without_losing_hermes_session_identity(self):
        _, first = self.request(payload=self.turn())
        session = self.backend.calls[0][1]
        old_instance = self.server.instance_id
        self.server.close()
        self.server = CharacterServer(SPECS, self.backend, TOKEN, max_interactions=0).start()
        _, restarted = self.request(payload=self.turn())
        self.assertNotEqual(old_instance, restarted["instance_id"])
        self.assertNotEqual(first["conversation_id"], restarted["conversation_id"])
        self.assertEqual(first["turn_id"], restarted["turn_id"])
        self.assertEqual(self.backend.calls[-1][1], session)

    def test_interrupt_invalidates_a_late_reply_even_if_backend_ignores_cancellation(self):
        backend = BlockingBackend(ignore_cancel=True)
        self.change_backend(backend)
        slow = self.pool.submit(self.request, payload=self.turn("slow"))
        self.assertTrue(backend.started.wait(5.0))
        code, interrupted = self.request("/v1/interrupt", {"user_id": "user-one", "conversation_id": "scene-one"})
        self.assertEqual(code, 200)
        self.assertTrue(backend.cancelled.wait(5.0))
        self.server._loop.call_soon_threadsafe(backend.release.set)
        code, result = slow.result(timeout=8.0)
        self.assertEqual(code, 200)
        self.assertTrue(result["discarded"])
        self.assertEqual(result["utterances"], [])
        self.assertEqual(result["actions"], [])
        self.assertEqual(result["conversation_id"], interrupted["conversation_id"])
        self.assertLess(result["turn_id"], interrupted["turn_id"])
        _, replacement = self.request(payload=self.turn("replacement"))
        self.assertGreater(replacement["turn_id"], interrupted["turn_id"])
        self.assertEqual(backend.calls[0][1], backend.calls[1][1])

    def test_other_user_remains_live_during_a_blocked_user_turn(self):
        backend = BlockingBackend()
        self.change_backend(backend)
        slow = self.pool.submit(self.request, payload=self.turn("slow"))
        self.assertTrue(backend.started.wait(5.0))
        code, other = self.request(payload=self.turn(user="user-two"))
        self.assertEqual(code, 200)
        self.assertFalse(other["discarded"])
        self.assertFalse(backend.cancelled.is_set())
        self.server._loop.call_soon_threadsafe(backend.release.set)
        self.assertEqual(slow.result(timeout=8.0)[0], 200)

    def test_request_budget_cancels_backend_and_does_not_return_partial_output(self):
        backend = BlockingBackend()
        self.change_backend(backend, request_timeout_s=2.0)
        code, data = self.request(payload=self.turn("slow"))
        self.assertEqual(code, 504)
        self.assertEqual(data["error"]["code"], "turn_timeout")
        self.assertTrue(backend.cancelled.wait(5.0))
        self.assertNotIn("utterances", data)
        code, current = self.request(payload=self.turn("next"))
        self.assertEqual(code, 200)
        self.assertFalse(current["discarded"])

    def test_client_disconnect_cancels_the_exact_turn(self):
        backend = BlockingBackend()
        self.change_backend(backend)
        raw = json.dumps(self.turn("slow")).encode("utf-8")
        connection = socket.create_connection(("127.0.0.1", self.server._http.server_port), timeout=5.0)
        try:
            header = (
                "POST /v1/turn HTTP/1.0\r\nContent-Type: application/json\r\n"
                f"Authorization: Bearer {TOKEN}\r\nContent-Length: {len(raw)}\r\n\r\n"
            ).encode("ascii")
            connection.sendall(header + raw)
            self.assertTrue(backend.started.wait(5.0))
        finally:
            connection.close()
        self.assertTrue(backend.cancelled.wait(5.0))
        code, data = self.request(payload=self.turn("new"))
        self.assertEqual(code, 200)
        self.assertFalse(data["discarded"])

    def test_http_input_limits_and_strict_fields_reject_before_backend(self):
        for payload in (
            self.turn(character="unknown"), self.turn(user="../user"), self.turn(text=" "),
            self.turn(text="x" * 8193), {**self.turn(), "priority": 100},
            {"user_id": "user-one"}, [],
        ):
            with self.subTest(payload_type=type(payload)):
                code, data = self.request(payload=payload)
                self.assertEqual(code, 400)
                self.assertEqual(data["error"]["code"], "invalid_request")
        for body in (b'{"user_id":"a","user_id":"b"}', b'{"text":"\\ud800"}', b"not-json"):
            self.assertEqual(self.request(body=body)[0], 400)
        self.assertEqual(self.request(body=b"x" * 65_537)[0], 413)
        self.assertEqual(self.request(payload=self.turn(), headers={"Transfer-Encoding": "chunked"})[0], 400)
        self.assertEqual(self.request(payload=self.turn(), headers={"Content-Type": "text/plain"})[0], 415)
        self.assertEqual(self.backend.calls, [])

    def test_backend_failures_do_not_expose_remote_diagnostics_or_credentials(self):
        class BrokenBackend:
            async def respond(self, *args):
                raise RuntimeError("remote secret: " + TOKEN)

        self.change_backend(BrokenBackend())
        code, data = self.request(payload=self.turn())
        self.assertEqual(code, 502)
        self.assertNotIn(TOKEN, json.dumps(data))
        self.assertNotIn("remote secret", json.dumps(data))

    def test_host_to_hermes_http_bridge_preserves_profile_keys_and_stable_sessions(self):
        records = []
        outputs = {}

        class HermesHandler(BaseHTTPRequestHandler):
            def log_message(self, *args):
                pass

            def send(self, code, data):
                raw = json.dumps(data).encode("utf-8")
                self.send_response(code)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(raw)))
                self.end_headers()
                self.wfile.write(raw)

            def do_POST(self):
                body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
                records.append((self.path, dict(self.headers), body))
                run_id = f"run_{len(records)}"
                outputs[run_id] = json.dumps({"text": "Hermes transport test", "action": "greet"})
                self.send(202, {"run_id": run_id, "status": "started"})

            def do_GET(self):
                run_id = self.path.rsplit("/", 1)[1]
                self.send(200, {"run_id": run_id, "status": "completed", "completed": True,
                                "partial": False, "interrupted": False, "output": outputs[run_id]})

        upstream = ThreadingHTTPServer(("127.0.0.1", 0), HermesHandler)
        upstream.daemon_threads = True
        upstream_thread = threading.Thread(target=upstream.serve_forever, kwargs={"poll_interval": 0.1}, daemon=True)
        upstream_thread.start()
        tokens = {"profile-alpha": "synthetic-alpha-key", "profile-beta": "synthetic-beta-key"}
        try:
            self.change_backend(HermesBackend(f"http://127.0.0.1:{upstream.server_port}", tokens))
            for character in ("alpha", "beta", "alpha"):
                code, response = self.request(payload=self.turn(character=character))
                self.assertEqual(code, 200)
                self.assertEqual(response["utterances"][0]["character_id"], character)
            for (path, headers, body), character in zip(records, ("alpha", "beta", "alpha")):
                self.assertEqual(path, f"/p/profile-{character}/v1/runs")
                self.assertEqual(headers["Authorization"], f"Bearer {tokens['profile-' + character]}")
                self.assertEqual(headers["X-Hermes-Session-Key"], "huahuo:" + body["session_id"])
            self.assertEqual(records[0][2]["session_id"], records[2][2]["session_id"])
            self.assertNotEqual(records[0][2]["session_id"], records[1][2]["session_id"])
        finally:
            self.server.close()
            upstream.shutdown()
            upstream.server_close()
            upstream_thread.join(timeout=2.0)

    def test_service_rejects_nonloopback_bind_and_weak_tokens(self):
        for options in ({"host": "0.0.0.0"}, {"host": "example.com"}, {"request_timeout_s": 0}, {"max_requests": True}):
            with self.subTest(options=options), self.assertRaises(ValueError):
                CharacterServer(SPECS, RecordingBackend(), TOKEN, **options)
        for token in ("short", "x" * 513, "x" * 31 + "\n"):
            with self.subTest(token_length=len(token)), self.assertRaises(ValueError):
                CharacterServer(SPECS, RecordingBackend(), token)

    def test_env_factory_uses_explicit_new_tokens_and_mock_is_opt_in(self):
        with tempfile.TemporaryDirectory() as directory:
            config = Path(directory) / "characters.json"
            config.write_text(json.dumps({"characters": [{
                "character_id": "alpha", "hermes_profile": "profile-alpha", "persona": "Alpha",
            }], "max_interactions": 0}), encoding="utf-8")
            args = argparse.Namespace(config=config, host="127.0.0.1", port=0, request_timeout=5.0,
                                      backend="hermes", hermes_url="http://127.0.0.1:8642")
            with patch.dict("os.environ", {"HUAHUO_CHARACTER_SERVER_TOKEN": TOKEN}, clear=True):
                with self.assertRaises(ValueError):
                    server_from_args(args)
                args.backend = "mock"
                offline = server_from_args(args)
                offline.close()
            args.backend = "hermes"
            with patch.dict("os.environ", {
                "HUAHUO_CHARACTER_SERVER_TOKEN": TOKEN,
                "HUAHUO_HERMES_API_TOKENS": json.dumps({"profile-alpha": "synthetic-profile-token"}),
            }, clear=True):
                live = server_from_args(args)
                try:
                    self.assertIsInstance(live.director.backend, HermesBackend)
                finally:
                    live.close()


if __name__ == "__main__":
    unittest.main()

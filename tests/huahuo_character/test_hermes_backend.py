"""Exercise the real urllib transport against an isolated loopback HTTP server."""

from __future__ import annotations

import asyncio
import json
import threading
import time
import unittest
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

from huahuo_character.hermes_backend import HermesBackend, HermesBackendError
from huahuo_character.protocol import CharacterSpec, MAX_TEXT_CHARS


TOKEN = "synthetic-test-token-only"
SPEC = CharacterSpec("luna", "luna-profile", "You are Luna.")


class Scenario:
    def __init__(self) -> None:
        self.records: list[tuple[str, str, dict, dict]] = []
        self.submit_status = 202
        self.poll_http_status = 200
        self.submit_delay_s = 0.0
        self.redirect_to: str | None = None
        self.submitted = threading.Event()
        self.polled = threading.Event()
        self.stopped = threading.Event()
        self.polls: list[dict] = [self.completed()]
        self.lock = threading.Lock()
        scenario = self

        class Handler(BaseHTTPRequestHandler):
            def log_message(self, *args):
                pass

            def _handle(self):
                body = {}
                length = int(self.headers.get("Content-Length", "0"))
                if length:
                    body = json.loads(self.rfile.read(length))
                with scenario.lock:
                    scenario.records.append((self.command, self.path, dict(self.headers), body))
                code = 200
                if self.command == "POST" and self.path.endswith("/v1/runs"):
                    scenario.submitted.set()
                    time.sleep(scenario.submit_delay_s)
                    if scenario.redirect_to:
                        self.send_response(302)
                        self.send_header("Location", scenario.redirect_to)
                        self.send_header("Content-Length", "0")
                        self.end_headers()
                        return
                    code = scenario.submit_status
                    result = {"run_id": "run_demo", "status": "started"}
                elif self.command == "POST" and self.path.endswith("/stop"):
                    scenario.stopped.set()
                    result = {"run_id": "run_demo", "status": "stopping"}
                else:
                    scenario.polled.set()
                    code = scenario.poll_http_status
                    with scenario.lock:
                        result = scenario.polls.pop(0) if len(scenario.polls) > 1 else scenario.polls[0]
                if code >= 400:
                    # A hostile or overly verbose server must not leak this in client errors.
                    result = {"error": {"message": TOKEN, "request_headers": TOKEN}}
                raw = json.dumps(result, ensure_ascii=False).encode()
                self.send_response(code)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(raw)))
                self.end_headers()
                try:
                    self.wfile.write(raw)
                except (BrokenPipeError, ConnectionResetError):
                    pass

            do_POST = _handle
            do_GET = _handle

        self.server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.server.daemon_threads = True
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.thread.start()
        self.url = f"http://127.0.0.1:{self.server.server_port}"

    @staticmethod
    def completed(output=None, **overrides) -> dict:
        result = {
            "run_id": "run_demo", "status": "completed", "completed": True,
            "partial": False, "interrupted": False,
            "output": output if output is not None else json.dumps({"text": "Hello", "action": "greet"}),
        }
        result.update(overrides)
        return result

    async def close(self) -> None:
        await asyncio.to_thread(self.server.shutdown)
        self.server.server_close()
        self.thread.join(timeout=1)


class HermesHTTPBackendTests(unittest.IsolatedAsyncioTestCase):
    async def asyncSetUp(self):
        self.scenario = Scenario()

    async def asyncTearDown(self):
        await self.scenario.close()

    def backend(self, **kwargs) -> HermesBackend:
        options = {"timeout_s": 3.0, "http_timeout_s": 2.0, "poll_interval_s": 0.01}
        options.update(kwargs)
        return HermesBackend(self.scenario.url, {SPEC.hermes_profile: TOKEN}, **options)

    async def test_profile_route_session_continuation_and_idempotency_key(self):
        backend = self.backend()
        self.scenario.polls = [Scenario.completed(json.dumps({
            "text": "你好", "action": "greet", "target_character_id": "sol",
        }))]
        first = await backend.respond(SPEC, "stable-session", "first input")
        second = await backend.respond(SPEC, "stable-session", "second input")
        self.assertEqual(first.text, "你好")
        self.assertEqual(second.target_character_id, "sol")
        submits = [r for r in self.scenario.records if r[0] == "POST" and r[1].endswith("/v1/runs")]
        self.assertEqual(len(submits), 2)
        for _, path, headers, body in self.scenario.records:
            self.assertTrue(path.startswith("/p/luna-profile/v1/runs"))
            self.assertEqual(headers["Authorization"], f"Bearer {TOKEN}")
        for record in submits:
            self.assertEqual(record[3]["session_id"], "stable-session")
            self.assertNotIn("conversation_history", record[3])
            self.assertNotIn("previous_response_id", record[3])
            self.assertEqual(record[2]["X-Hermes-Session-Key"], "huahuo:stable-session")
        self.assertEqual(submits[0][3]["instructions"], submits[1][3]["instructions"])
        self.assertNotEqual(submits[0][2]["Idempotency-Key"], submits[1][2]["Idempotency-Key"])
        self.assertNotIn(TOKEN, repr(backend))

    async def test_pending_runs_are_polled_until_final_json(self):
        self.scenario.polls = [
            {"run_id": "run_demo", "status": "queued"},
            {"run_id": "run_demo", "status": "running"}, Scenario.completed(),
        ]
        reply = await self.backend().respond(SPEC, "session", "hello")
        self.assertEqual(reply.action, "greet")
        self.assertEqual(len([r for r in self.scenario.records if r[0] == "GET"]), 3)
        self.assertFalse(self.scenario.stopped.is_set())

    async def test_async_cancel_requests_stop(self):
        self.scenario.polls = [{"run_id": "run_demo", "status": "running"}]
        task = asyncio.create_task(self.backend().respond(SPEC, "session", "hello"))
        self.assertTrue(await asyncio.to_thread(self.scenario.polled.wait, 2.0))
        task.cancel()
        with self.assertRaises(asyncio.CancelledError):
            await task
        self.assertTrue(self.scenario.stopped.is_set())

    async def test_cancel_during_submit_recovers_accepted_run_and_stops_it(self):
        self.scenario.submit_delay_s = 0.1
        task = asyncio.create_task(self.backend().respond(SPEC, "session", "hello"))
        self.assertTrue(await asyncio.to_thread(self.scenario.submitted.wait, 2.0))
        task.cancel()
        with self.assertRaises(asyncio.CancelledError):
            await task
        self.assertTrue(self.scenario.stopped.is_set())

    async def test_time_budget_requests_stop(self):
        self.scenario.polls = [{"run_id": "run_demo", "status": "running"}]
        with self.assertRaisesRegex(HermesBackendError, "time budget"):
            await self.backend(timeout_s=2.0, poll_interval_s=0.05).respond(SPEC, "session", "hello")
        self.assertTrue(self.scenario.stopped.is_set())

    async def test_unknown_failed_and_approval_states_are_not_success(self):
        for state in ("unexpected", "failed", "cancelled", "interrupted", "waiting_for_approval", {}):
            with self.subTest(state=state):
                self.scenario.polls = [{"run_id": "run_demo", "status": state, "error": TOKEN}]
                self.scenario.stopped.clear()
                with self.assertRaises(HermesBackendError) as caught:
                    await self.backend().respond(SPEC, "session", "hello")
                self.assertNotIn(TOKEN, str(caught.exception))
                self.assertTrue(self.scenario.stopped.is_set())

    async def test_429_submit_and_http_failure_do_not_leak_credentials(self):
        self.scenario.submit_status = 429
        with self.assertRaisesRegex(HermesBackendError, "HTTP 429") as caught:
            await self.backend().respond(SPEC, "session", "hello")
        self.assertNotIn(TOKEN, str(caught.exception))
        self.assertFalse(self.scenario.stopped.is_set())
        self.scenario.submit_status = 202
        self.scenario.poll_http_status = 500
        with self.assertRaisesRegex(HermesBackendError, "HTTP 500") as caught:
            await self.backend().respond(SPEC, "session", "hello")
        self.assertNotIn(TOKEN, str(caught.exception))
        self.assertTrue(self.scenario.stopped.is_set())

    async def test_free_text_bad_json_and_metadata_cannot_become_actions(self):
        invalid = (
            "hello", "```json\n{\"text\":\"hello\"}\n```", "[]",
            '{"text":"hello","text":"duplicate"}',
            json.dumps({"text": "hello", "action": "run_shell"}),
            json.dumps({"text": "hello", "action": False}),
            json.dumps({"text": "hello", "character_id": "forged"}),
            json.dumps({"text": "hello", "metadata": {"priority": 999}}),
            json.dumps({"text": "hello", "target_character_id": "../bad"}),
            json.dumps({"text": ""}), json.dumps({"text": "x" * (MAX_TEXT_CHARS + 1)}),
        )
        for output in invalid:
            with self.subTest(output=output[:60]):
                self.scenario.polls = [Scenario.completed(output)]
                with self.assertRaises(HermesBackendError):
                    await self.backend().respond(SPEC, "session", "hello")

    async def test_only_character_enabled_actions_are_accepted(self):
        restricted = CharacterSpec("luna", "luna-profile", "You are Luna.", frozenset({"think"}))
        with self.assertRaisesRegex(HermesBackendError, "unavailable"):
            await self.backend().respond(restricted, "session", "hello")

    async def test_completed_requires_boolean_success_flags(self):
        for change in ({"completed": False}, {"completed": "true"}, {"partial": True}, {"partial": "false"}, {"interrupted": True}):
            with self.subTest(change=change):
                self.scenario.polls = [Scenario.completed(**change)]
                with self.assertRaisesRegex(HermesBackendError, "incomplete"):
                    await self.backend().respond(SPEC, "session", "hello")

    async def test_input_output_and_http_response_limits(self):
        with self.assertRaisesRegex(HermesBackendError, "input"):
            await self.backend(max_input_chars=3).respond(SPEC, "session", "too long")
        self.assertEqual(self.scenario.records, [])
        self.scenario.polls = [Scenario.completed(json.dumps({"text": "x" * 200}))]
        with self.assertRaisesRegex(HermesBackendError, "output"):
            await self.backend(max_output_chars=32).respond(SPEC, "session", "hello")
        with self.assertRaisesRegex(HermesBackendError, "HTTP response"):
            await self.backend(max_response_bytes=128).respond(SPEC, "session", "hello")

    async def test_reply_container_allows_escaped_maximum_decoded_text(self):
        # Container bounds are distinct from AgentReply's decoded text bound.
        public_text = "\x01" * MAX_TEXT_CHARS
        self.scenario.polls = [Scenario.completed(json.dumps({"text": public_text}))]
        reply = await self.backend().respond(SPEC, "session", "hello")
        self.assertEqual(reply.text, public_text)

    async def test_profile_token_scope_returns_to_first_profile_without_fallback(self):
        second = CharacterSpec("sol", "sol-profile", "You are Sol.")
        second_token = "synthetic-sol-token-only"
        backend = HermesBackend(self.scenario.url, {
            SPEC.hermes_profile: TOKEN, second.hermes_profile: second_token,
        })
        for spec in (SPEC, second, SPEC):
            await backend.respond(spec, "stable-session", "hello")
        submits = [r for r in self.scenario.records if r[0] == "POST" and r[1].endswith("/v1/runs")]
        self.assertEqual([r[1] for r in submits], [
            "/p/luna-profile/v1/runs", "/p/sol-profile/v1/runs", "/p/luna-profile/v1/runs",
        ])
        self.assertEqual([r[2]["Authorization"] for r in submits], [
            f"Bearer {TOKEN}", f"Bearer {second_token}", f"Bearer {TOKEN}",
        ])

    async def test_redirect_never_forwards_token(self):
        target = Scenario()
        try:
            self.scenario.redirect_to = target.url + "/capture"
            with self.assertRaisesRegex(HermesBackendError, "redirect"):
                await self.backend().respond(SPEC, "session", "hello")
            self.assertEqual(target.records, [])
        finally:
            await target.close()

    async def test_invalid_session_or_missing_token_never_submits(self):
        for session in ("../other", "bad\r\nAuthorization:x", "x" * 201):
            with self.assertRaises(HermesBackendError):
                await self.backend().respond(SPEC, session, "hello")
        with self.assertRaisesRegex(HermesBackendError, "token"):
            await HermesBackend(self.scenario.url, {}).respond(SPEC, "session", "hello")
        self.assertEqual(self.scenario.records, [])


class HermesBackendConfigurationTests(unittest.TestCase):
    def test_only_explicit_loopback_origins_are_allowed(self):
        for url in ("https://example.com", "http://127.0.0.1.evil", "http://user:secret@127.0.0.1", "http://127.0.0.1/path", "http://127.0.0.1?token=secret", "http://127.0.0.1#fragment", "http://127.0.0.1:bad", "\nhttp://127.0.0.1"):
            with self.subTest(url=url), self.assertRaises(ValueError):
                HermesBackend(url, {})
        HermesBackend("http://127.0.0.1:8642", {})
        HermesBackend("http://[::1]:8642", {})

    def test_invalid_limits_profiles_and_tokens_fail_without_echoing_values(self):
        cases = (
            {"timeout_s": float("inf")}, {"poll_interval_s": 0}, {"max_output_chars": True},
        )
        for options in cases:
            with self.assertRaises(ValueError):
                HermesBackend("http://127.0.0.1", {}, **options)
        for tokens in ({"../profile": TOKEN}, {"luna": TOKEN + "\r\nheader:x"}):
            with self.assertRaises(ValueError) as caught:
                HermesBackend("http://127.0.0.1", tokens)
            self.assertNotIn(TOKEN, str(caught.exception))

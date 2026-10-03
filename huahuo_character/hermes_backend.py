"""Loopback HTTP bridge to Hermes profiles; no model or credential discovery."""

from __future__ import annotations

import asyncio
import http.client
import ipaddress
import json
import math
import re
import time
import urllib.error
import urllib.parse
import urllib.request
import uuid
from collections.abc import Mapping
from typing import Any

from .protocol import AgentReply, CharacterSpec


class HermesBackendError(RuntimeError):
    """A bounded, credential-free failure from the character backend."""


class _NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, req, fp, code, msg, headers, newurl):
        fp.close()
        raise HermesBackendError("Hermes HTTP redirects are refused")


def _object_without_duplicates(pairs: list[tuple[str, Any]]) -> dict[str, Any]:
    result: dict[str, Any] = {}
    for key, value in pairs:
        if key in result:
            raise ValueError("Duplicate JSON field")
        result[key] = value
    return result


class HermesBackend:
    """Implement the Director backend using ``/p/<profile>/v1/runs``.

    API tokens are supplied explicitly by the host, never read from Hermes
    homes or environment files. Input is a new turn on a stable server-side
    session. This requires current Hermes session-continuation semantics.
    ``stop`` requests cooperative cancellation; the Director remains responsible
    for discarding replies from interrupted turns and stopping local playback.
    """

    _PROFILE = re.compile(r"[A-Za-z0-9][A-Za-z0-9_-]{0,63}\Z")
    _SESSION = re.compile(r"[A-Za-z0-9][A-Za-z0-9:_-]{0,199}\Z")
    _RUN = re.compile(r"[A-Za-z0-9][A-Za-z0-9_-]{0,127}\Z")
    _PENDING = frozenset({"queued", "running", "waiting_for_approval", "stopping"})
    _TERMINAL = frozenset({"completed", "failed", "cancelled", "interrupted"})

    def __init__(
        self,
        base_url: str,
        api_tokens: Mapping[str, str],
        *,
        timeout_s: float = 15.0,
        http_timeout_s: float = 3.0,
        poll_interval_s: float = 0.1,
        max_input_chars: int = 8192,
        max_output_chars: int = 65_536,
        max_response_bytes: int = 131_072,
    ) -> None:
        if not isinstance(base_url, str) or len(base_url) > 2048 or any(ord(c) < 33 for c in base_url):
            raise ValueError("Hermes base_url must be a loopback HTTP origin")
        try:
            parsed = urllib.parse.urlsplit(base_url)
            host = parsed.hostname
            _ = parsed.port
            loopback = host == "localhost" or bool(host and ipaddress.ip_address(host).is_loopback)
        except (ValueError, TypeError):
            loopback = False
            parsed = None
        if (
            parsed is None
            or parsed.scheme not in {"http", "https"}
            or not loopback
            or parsed.username is not None
            or parsed.password is not None
            or parsed.path not in {"", "/"}
            or parsed.query
            or parsed.fragment
        ):
            raise ValueError("Hermes base_url must be a loopback HTTP origin")
        for value in (timeout_s, http_timeout_s, poll_interval_s):
            if isinstance(value, bool) or not isinstance(value, (int, float)) or not math.isfinite(value) or value <= 0:
                raise ValueError("Hermes time limits must be positive finite numbers")
        for value in (max_input_chars, max_output_chars, max_response_bytes):
            if isinstance(value, bool) or not isinstance(value, int) or value <= 0:
                raise ValueError("Hermes size limits must be positive integers")
        self._api_tokens: dict[str, str] = {}
        if not isinstance(api_tokens, Mapping):
            raise ValueError("Hermes API tokens must be supplied by profile")
        for profile, token in api_tokens.items():
            if not isinstance(profile, str) or not self._PROFILE.fullmatch(profile):
                raise ValueError("Invalid Hermes profile identifier")
            if not isinstance(token, str) or not token or any(ord(c) < 33 or ord(c) > 126 for c in token):
                raise ValueError("Hermes API tokens must be non-empty visible ASCII")
            self._api_tokens[profile] = token
        self._base_url = base_url.rstrip("/")
        self._timeout_s = float(timeout_s)
        self._http_timeout_s = float(http_timeout_s)
        self._poll_interval_s = float(poll_interval_s)
        self._max_input_chars = max_input_chars
        self._max_output_chars = max_output_chars
        self._max_response_bytes = max_response_bytes

    def _request_sync(
        self,
        profile: str,
        method: str,
        suffix: str,
        body: dict[str, Any] | None = None,
        *,
        extra_headers: dict[str, str] | None = None,
        timeout_s: float,
        expected_status: int = 200,
    ) -> dict[str, Any]:
        headers = {"Authorization": f"Bearer {self._api_tokens[profile]}", "Accept": "application/json"}
        if extra_headers:
            headers.update(extra_headers)
        data = None
        if body is not None:
            data = json.dumps(body, ensure_ascii=False, allow_nan=False).encode("utf-8")
            headers["Content-Type"] = "application/json"
        request = urllib.request.Request(
            f"{self._base_url}/p/{profile}{suffix}", data=data, headers=headers, method=method
        )
        # Ignore process proxy settings and never follow a credential-bearing redirect.
        opener = urllib.request.build_opener(urllib.request.ProxyHandler({}), _NoRedirect())
        try:
            with opener.open(request, timeout=timeout_s) as response:
                if response.status != expected_status:
                    raise HermesBackendError("Unexpected Hermes HTTP status")
                raw = response.read(self._max_response_bytes + 1)
        except urllib.error.HTTPError as exc:
            exc.close()
            raise HermesBackendError(f"Hermes HTTP {exc.code}") from None
        except HermesBackendError:
            raise
        except (urllib.error.URLError, TimeoutError, OSError, http.client.HTTPException):
            raise HermesBackendError("Hermes HTTP request failed") from None
        if len(raw) > self._max_response_bytes:
            raise HermesBackendError("Hermes HTTP response exceeds the size limit")
        try:
            result = json.loads(raw, object_pairs_hook=_object_without_duplicates)
        except (ValueError, UnicodeError, RecursionError):
            raise HermesBackendError("Invalid Hermes HTTP JSON") from None
        if not isinstance(result, dict):
            raise HermesBackendError("Hermes HTTP JSON must be an object")
        return result

    async def _request(self, *args, timeout_s: float | None = None, **kwargs) -> dict[str, Any]:
        return await asyncio.to_thread(
            self._request_sync, *args,
            timeout_s=min(self._http_timeout_s, timeout_s or self._http_timeout_s), **kwargs
        )

    async def _stop(self, profile: str, run_id: str) -> None:
        try:
            await asyncio.wait_for(
                self._request(profile, "POST", f"/v1/runs/{run_id}/stop", {}),
                timeout=self._http_timeout_s + 0.1,
            )
        except (HermesBackendError, TimeoutError):
            # Cancellation is best effort; never obscure the original failure.
            pass

    def _reply(self, spec: CharacterSpec, status: dict[str, Any]) -> AgentReply:
        if status.get("completed") is not True or status.get("partial") is not False or status.get("interrupted") is not False:
            raise HermesBackendError("Hermes returned an incomplete character turn")
        output = status.get("output")
        if not isinstance(output, str) or len(output) > self._max_output_chars:
            raise HermesBackendError("Hermes character output is missing or exceeds the size limit")
        try:
            data = json.loads(output, object_pairs_hook=_object_without_duplicates)
            reply = AgentReply.from_dict(data)
        except (ValueError, TypeError, UnicodeError, RecursionError):
            raise HermesBackendError("Hermes character output must match the reply JSON contract") from None
        if reply.action is not None and reply.action not in spec.allowed_actions:
            raise HermesBackendError("Hermes requested an action unavailable to this character")
        return reply

    async def respond(self, spec: CharacterSpec, session_id: str, text: str) -> AgentReply:
        profile = spec.hermes_profile
        if profile not in self._api_tokens:
            raise HermesBackendError("No API token supplied for the character profile")
        if not isinstance(session_id, str) or not self._SESSION.fullmatch(session_id):
            raise HermesBackendError("Invalid character session identifier")
        if not isinstance(text, str) or not text.strip() or len(text) > self._max_input_chars:
            raise HermesBackendError("Character input is empty or exceeds the size limit")
        if not isinstance(spec.persona, str) or len(spec.persona) > self._max_input_chars:
            raise HermesBackendError("Character persona exceeds the size limit")
        # Persona and schema stay constant for a character's entire conversation.
        instructions = (
            spec.persona + "\nReturn a single JSON object, with no markdown or other text. "
            "Required field: text (a non-empty string). Optional fields: action "
            "(one allowed action string or null), target_character_id (another character ID or null). "
            "No other fields are permitted. Allowed actions: " + ", ".join(sorted(spec.allowed_actions))
        )
        headers = {"Idempotency-Key": uuid.uuid4().hex, "X-Hermes-Session-Key": f"huahuo:{session_id}"}
        deadline = time.monotonic() + self._timeout_s
        run_id: str | None = None
        submission: asyncio.Task | None = None

        async def cleanup() -> None:
            nonlocal run_id
            # Shield submit: urllib cannot be preempted. Recover an accepted run ID
            # within the HTTP bound so cancellation can still request stop.
            if run_id is None and submission is not None:
                try:
                    accepted = await asyncio.wait_for(asyncio.shield(submission), self._http_timeout_s + 0.1)
                    candidate = accepted.get("run_id")
                    if isinstance(candidate, str) and self._RUN.fullmatch(candidate):
                        run_id = candidate
                except (HermesBackendError, TimeoutError):
                    pass
            if run_id is not None:
                await self._stop(profile, run_id)

        try:
            submission = asyncio.create_task(self._request(
                profile, "POST", "/v1/runs",
                {"input": text, "instructions": instructions, "session_id": session_id},
                extra_headers=headers, expected_status=202,
                timeout_s=max(0.001, deadline - time.monotonic()),
            ))
            accepted = await asyncio.wait_for(asyncio.shield(submission), self._timeout_s)
            candidate = accepted.get("run_id")
            if not isinstance(candidate, str) or not self._RUN.fullmatch(candidate):
                raise HermesBackendError("Hermes did not return a valid run identifier")
            run_id = candidate
            while True:
                remaining = deadline - time.monotonic()
                if remaining <= 0:
                    raise TimeoutError
                status = await asyncio.wait_for(
                    self._request(profile, "GET", f"/v1/runs/{run_id}", timeout_s=remaining), remaining
                )
                if status.get("run_id") != run_id:
                    raise HermesBackendError("Hermes returned a different run identifier")
                state = status.get("status")
                if not isinstance(state, str):
                    raise HermesBackendError("Hermes returned an unknown run state")
                if state == "completed":
                    return self._reply(spec, status)
                if state in self._TERMINAL:
                    raise HermesBackendError("Hermes character run did not complete")
                if state == "waiting_for_approval":
                    raise HermesBackendError("Hermes character run requires operator approval")
                if state not in self._PENDING:
                    raise HermesBackendError("Hermes returned an unknown run state")
                await asyncio.sleep(min(self._poll_interval_s, max(0, deadline - time.monotonic())))
        except asyncio.CancelledError:
            await cleanup()
            raise
        except TimeoutError:
            await cleanup()
            raise HermesBackendError("Hermes character turn exceeded its time budget") from None
        except HermesBackendError:
            await cleanup()
            raise

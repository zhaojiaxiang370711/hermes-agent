"""Authenticated loopback host for one persistent character Director.

Hermes transcript keys remain stable across restarts. Public playback scopes
include a process instance, so a former process's turn cannot be replayed into
the replacement. This service does not capture audio or play speech itself.
"""

from __future__ import annotations

import argparse
import asyncio
import concurrent.futures
from hashlib import sha256
import hmac
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import ipaddress
import json
import math
import os
from pathlib import Path
import select
import signal
import socket
import sys
import threading
import time
from typing import Any, Iterable
from uuid import uuid4

from .cli import EXAMPLE, MockBackend, load_characters
from .director import AgentBackend, Director
from .hermes_backend import HermesBackend
from .protocol import CharacterSpec, ProtocolError, validate_identifier, validate_text


def _json_object(pairs: list[tuple[str, Any]]) -> dict[str, Any]:
    result: dict[str, Any] = {}
    for key, value in pairs:
        if key in result:
            raise ValueError("Duplicate JSON field")
        result[key] = value
    return result


class CharacterServer:
    """Own a Director event loop and a bounded, loopback-only HTTP listener.

    The host explicitly supplies a newly generated bearer token. Threaded HTTP
    handlers submit work to the *same* asyncio loop, so interruption and session
    locks retain their meaning between requests. Use ``start``/``close`` or the
    context manager; the CLI keeps this object alive until SIGINT/SIGTERM.
    """

    def __init__(
        self,
        specs: Iterable[CharacterSpec],
        backend: AgentBackend,
        token: str,
        *,
        host: str = "127.0.0.1",
        port: int = 0,
        max_interactions: int = 1,
        request_timeout_s: float = 60.0,
        body_timeout_s: float = 5.0,
        max_body_bytes: int = 65_536,
        max_requests: int = 32,
        backend_name: str = "hermes",
    ) -> None:
        try:
            address = ipaddress.ip_address(host)
        except ValueError:
            address = ipaddress.ip_address("127.0.0.1") if host == "localhost" else None
        if address is None or not address.is_loopback:
            raise ValueError("Character server must bind a loopback address")
        if type(port) is not int or not 0 <= port <= 65_535:
            raise ValueError("Invalid character server port")
        if not isinstance(token, str) or not 32 <= len(token) <= 512 or any(not 33 <= ord(c) <= 126 for c in token):
            raise ValueError("A newly generated visible ASCII bearer token is required")
        for value in (request_timeout_s, body_timeout_s):
            if isinstance(value, bool) or not isinstance(value, (int, float)) or not math.isfinite(value) or value <= 0:
                raise ValueError("Request time limits must be positive finite numbers")
        for value in (max_body_bytes, max_requests):
            if type(value) is not int or value <= 0:
                raise ValueError("Request limits must be positive integers")
        if backend_name not in {"hermes", "mock"}:
            raise ValueError("Invalid backend name")
        specs = tuple(specs)
        self.director = Director(specs, backend, max_interactions=max_interactions)
        self.instance_id = uuid4().hex
        self._characters = frozenset(spec.character_id for spec in specs)
        self._token = token.encode("ascii")
        self._request_timeout_s = float(request_timeout_s)
        self._body_timeout_s = float(body_timeout_s)
        self._max_body_bytes = max_body_bytes
        self._backend_name = backend_name
        self._loop = asyncio.new_event_loop()
        self._ready = threading.Event()
        self._started = False
        self._closed = False
        self._loop_thread = threading.Thread(target=self._run_loop, name="character-director", daemon=True)
        service = self

        class Handler(BaseHTTPRequestHandler):
            # One request per connection makes disconnect cancellation unambiguous.
            protocol_version = "HTTP/1.0"

            def setup(self):
                super().setup()
                self.connection.settimeout(service._body_timeout_s)

            def log_message(self, *args):
                pass  # Never log headers, bodies, identities or credentials.

            def _send(self, code: int, payload: dict[str, Any]) -> None:
                raw = json.dumps(payload, ensure_ascii=False, allow_nan=False).encode("utf-8")
                self.close_connection = True
                try:
                    self.send_response(code)
                    self.send_header("Content-Type", "application/json; charset=utf-8")
                    self.send_header("Content-Length", str(len(raw)))
                    self.send_header("Cache-Control", "no-store")
                    self.send_header("Connection", "close")
                    self.end_headers()
                    self.wfile.write(raw)
                except (OSError, ValueError):
                    pass

            def _error(self, code: int, name: str) -> None:
                self._send(code, {"error": {"code": name}, "instance_id": service.instance_id})

            def do_GET(self):
                if self.path == "/health":
                    self._send(200, {
                        "status": "ready", "instance_id": service.instance_id,
                        "backend": service._backend_name, "protocol": "huahuo.character.http.v1",
                    })
                else:
                    self._error(404, "not_found")

            def do_POST(self):
                auth = self.headers.get_all("Authorization", [])
                if len(auth) != 1 or not auth[0].startswith("Bearer ") or not hmac.compare_digest(
                    auth[0][7:].encode("utf-8"), service._token
                ):
                    self._error(401, "unauthorized")
                    return
                if self.path not in {"/v1/turn", "/v1/interrupt"}:
                    self._error(404, "not_found")
                    return
                lengths = self.headers.get_all("Content-Length", [])
                if self.headers.get("Transfer-Encoding") or len(lengths) != 1 or not lengths[0].isascii() or not lengths[0].isdecimal():
                    self._error(400, "invalid_body_length")
                    return
                length = int(lengths[0]) if len(lengths[0]) <= 10 else service._max_body_bytes + 1
                if not 0 < length <= service._max_body_bytes:
                    self._error(413, "body_too_large")
                    return
                if self.headers.get_content_type() != "application/json":
                    self._error(415, "json_required")
                    return
                try:
                    raw = self.rfile.read(length)
                    if len(raw) != length:
                        raise ValueError("Incomplete body")
                    payload = json.loads(raw.decode("utf-8"), object_pairs_hook=_json_object)
                    service._validate(self.path, payload)
                except TimeoutError:
                    self._error(408, "body_timeout")
                    return
                except (ValueError, TypeError, UnicodeError, RecursionError, OSError):
                    self._error(400, "invalid_request")
                    return
                future = asyncio.run_coroutine_threadsafe(service._dispatch(self.path, payload), service._loop)
                deadline = time.monotonic() + service._request_timeout_s
                while True:
                    try:
                        result = future.result(timeout=min(0.1, max(0.001, deadline - time.monotonic())))
                        self._send(200, result)
                        return
                    except concurrent.futures.TimeoutError:
                        if future.done():
                            try:
                                finished = future.result()
                            except concurrent.futures.CancelledError:
                                self._error(409, "turn_cancelled")
                            except Exception:
                                self._error(502, "character_backend_failed")
                            else:
                                self._send(200, finished)
                            return
                        if time.monotonic() >= deadline:
                            future.cancel()
                            self._error(504, "turn_timeout")
                            return
                        if self._disconnected():
                            future.cancel()
                            return
                    except concurrent.futures.CancelledError:
                        self._error(409, "turn_cancelled")
                        return
                    except Exception:
                        self._error(502, "character_backend_failed")
                        return

            def _disconnected(self) -> bool:
                try:
                    readable, _, _ = select.select([self.connection], [], [], 0)
                    return bool(readable) and self.connection.recv(1, socket.MSG_PEEK) == b""
                except OSError:
                    return True

            def do_PUT(self):
                self._error(405, "method_not_allowed")

            do_DELETE = do_PATCH = do_OPTIONS = do_PUT

        class HTTPServer(ThreadingHTTPServer):
            daemon_threads = True
            block_on_close = False
            address_family = socket.AF_INET6 if address.version == 6 else socket.AF_INET

            def __init__(self):
                self._slots = threading.BoundedSemaphore(max_requests)
                super().__init__((str(address), port), Handler)

            def process_request(self, request, client_address):
                if not self._slots.acquire(blocking=False):
                    try:
                        request.sendall(b"HTTP/1.0 503 Service Unavailable\r\nConnection: close\r\nContent-Length: 0\r\n\r\n")
                    except OSError:
                        pass
                    self.shutdown_request(request)
                    return
                try:
                    super().process_request(request, client_address)
                except BaseException:
                    self._slots.release()
                    raise

            def process_request_thread(self, request, client_address):
                try:
                    super().process_request_thread(request, client_address)
                finally:
                    self._slots.release()

            def handle_error(self, request, client_address):
                pass  # A malformed transport must not produce diagnostic dumps.

        try:
            self._http = HTTPServer()
        except BaseException:
            self._loop.close()
            raise
        self._http_thread = threading.Thread(target=self._http.serve_forever, kwargs={"poll_interval": 0.1}, name="character-http", daemon=True)

    @property
    def url(self) -> str:
        host, port = self._http.server_address[:2]
        return f"http://{'[' + host + ']' if ':' in host else host}:{port}"

    def _run_loop(self) -> None:
        asyncio.set_event_loop(self._loop)
        self._ready.set()
        self._loop.run_forever()

    def start(self) -> CharacterServer:
        if self._closed or self._started:
            raise RuntimeError("Character server cannot be started again")
        self._started = True
        self._loop_thread.start()
        if not self._ready.wait(5):
            raise RuntimeError("Character event loop did not start")
        self._http_thread.start()
        return self

    def _scope(self, user_id: str, conversation_id: str) -> str:
        stable = self.director.conversation_id_for(user_id, conversation_id)
        return "scene-" + sha256(f"{self.instance_id}:{stable}".encode("ascii")).hexdigest()[:48]

    def _validate(self, path: str, data: Any) -> None:
        fields = {"user_id", "conversation_id"}
        if path == "/v1/turn":
            fields |= {"character_id", "text"}
        if not isinstance(data, dict) or set(data) != fields:
            raise ProtocolError("Invalid request fields")
        validate_identifier(data["user_id"], "user_id")
        validate_identifier(data["conversation_id"], "conversation_id")
        if path == "/v1/turn":
            validate_identifier(data["character_id"], "character_id")
            if data["character_id"] not in self._characters:
                raise ProtocolError("Unknown character")
            validate_text(data["text"])

    async def _dispatch(self, path: str, data: dict[str, Any]) -> dict[str, Any]:
        public_scope = self._scope(data["user_id"], data["conversation_id"])
        if path == "/v1/interrupt":
            return {
                "instance_id": self.instance_id, "conversation_id": public_scope,
                "turn_id": self.director.interrupt(data["user_id"], data["conversation_id"]),
            }
        result = await self.director.respond(
            data["character_id"], data["user_id"], data["text"], conversation_id=data["conversation_id"]
        )
        payload = result.to_dict()
        payload["conversation_id"] = public_scope
        for action in payload["actions"]:
            action["conversation_id"] = public_scope
        payload["instance_id"] = self.instance_id
        return payload

    async def _drain(self) -> None:
        tasks = [task for task in asyncio.all_tasks() if task is not asyncio.current_task()]
        for task in tasks:
            task.cancel()
        if tasks:
            await asyncio.wait(tasks, timeout=8.0)

    def close(self) -> None:
        if self._closed:
            return
        self._closed = True
        if self._started:
            self._http.shutdown()
        self._http.server_close()
        if self._started:
            try:
                asyncio.run_coroutine_threadsafe(self._drain(), self._loop).result(timeout=9.0)
            except (TimeoutError, concurrent.futures.CancelledError):
                pass
            self._loop.call_soon_threadsafe(self._loop.stop)
            self._loop_thread.join(timeout=2.0)
            self._http_thread.join(timeout=2.0)
        if not self._loop.is_running():
            self._loop.close()

    def __enter__(self) -> CharacterServer:
        return self.start()

    def __exit__(self, *args) -> None:
        self.close()


def server_from_args(args: argparse.Namespace) -> CharacterServer:
    """Read only the host's explicit service and profile-token injections."""
    specs, hops = load_characters(args.config)
    token = os.environ.get("HUAHUO_CHARACTER_SERVER_TOKEN", "")
    if args.backend == "mock":
        backend = MockBackend(specs)
    else:
        raw_tokens = os.environ.get("HUAHUO_HERMES_API_TOKENS", "")
        tokens = json.loads(raw_tokens, object_pairs_hook=_json_object)
        if not isinstance(tokens, dict) or set(tokens) != {spec.hermes_profile for spec in specs}:
            raise ValueError("Explicit API token map must cover the configured profiles")
        backend = HermesBackend(args.hermes_url, tokens)
    return CharacterServer(
        specs, backend, token, host=args.host, port=args.port,
        max_interactions=hops, request_timeout_s=args.request_timeout, backend_name=args.backend,
    )


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description="Persistent loopback Huahuo character host")
    parser.add_argument("--config", type=Path, default=EXAMPLE)
    parser.add_argument("--host", default="127.0.0.1")
    parser.add_argument("--port", type=int, default=8643)
    parser.add_argument("--backend", choices=("hermes", "mock"), default="hermes")
    parser.add_argument("--hermes-url", default="http://127.0.0.1:8642")
    parser.add_argument("--request-timeout", type=float, default=60.0)
    args = parser.parse_args(argv)
    stop = threading.Event()
    try:
        with server_from_args(args) as server:
            for signum in (signal.SIGINT, signal.SIGTERM):
                signal.signal(signum, lambda *_: stop.set())
            print(json.dumps({"url": server.url, "instance_id": server.instance_id, "backend": args.backend}), flush=True)
            stop.wait()
    except (ValueError, TypeError, OSError, RuntimeError) as exc:
        print(f"character server failed: {type(exc).__name__}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

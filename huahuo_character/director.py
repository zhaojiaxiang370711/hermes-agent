"""Bounded character dialogue with scoped sessions and stale-turn rejection.

The director is intentionally in-memory. Hermes owns persisted conversation
history; animation resources and playback priority belong to the game engine.
"""

from __future__ import annotations

import asyncio
from hashlib import sha256
import json
from typing import Iterable, Protocol
from uuid import uuid4

from .protocol import (
    ActionEvent,
    AgentReply,
    CharacterSpec,
    MAX_TEXT_CHARS,
    MAX_TURN_ID,
    ProtocolError,
    TurnResult,
    Utterance,
    validate_identifier,
    validate_text,
)


class AgentBackend(Protocol):
    async def respond(self, spec: CharacterSpec, session_id: str, text: str) -> AgentReply:
        """Reply within the supplied Hermes profile and persisted session."""
        ...


def _scope(prefix: str, *parts: str) -> str:
    encoded = json.dumps(parts, ensure_ascii=True, separators=(",", ":")).encode("utf-8")
    return prefix + sha256(encoded).hexdigest()[:48]


def _public_prompt(character_id: str, text: str) -> str:
    """Fit the public utterance envelope inside the backend input contract.

    JSON escaping can expand a legal reply several times. Truncate only this
    recipient's public scene excerpt, with an explicit marker; the original
    user-facing utterance and the source character's history remain complete.
    """
    payload = {"type": "scene.utterance", "character_id": character_id, "text": text}

    def encode() -> str:
        return json.dumps(payload, ensure_ascii=False, separators=(",", ":"))

    prompt = encode()
    if len(prompt) <= MAX_TEXT_CHARS:
        return prompt
    payload["truncated"] = True
    low, high = 0, len(text)
    while low < high:
        middle = (low + high + 1) // 2
        payload["text"] = text[:middle]
        if len(encode()) <= MAX_TEXT_CHARS:
            low = middle
        else:
            high = middle - 1
    payload["text"] = text[:low]
    return encode()


class Director:
    """One speaker at a time per user and scene, with at most four relays.

    ``max_interactions`` counts *additional* character replies after the first.
    Public utterances can be relayed; another character's persona, private
    history and any other user's interaction are never copied by this layer.
    Use one Director per event loop. Persistent engine scopes should be reset
    when this process restarts because turn counters are not persisted yet.
    """

    def __init__(
        self,
        specs: Iterable[CharacterSpec],
        backend: AgentBackend,
        *,
        max_interactions: int = 2,
    ) -> None:
        if type(max_interactions) is not int or not 0 <= max_interactions <= 4:
            raise ProtocolError("max_interactions must be an integer from 0 to 4")
        self._specs: dict[str, CharacterSpec] = {}
        profiles: set[str] = set()
        for spec in specs:
            if not isinstance(spec, CharacterSpec):
                raise ProtocolError("all characters must be CharacterSpec instances")
            if spec.character_id in self._specs:
                raise ProtocolError("character_id must be unique")
            if spec.hermes_profile in profiles:
                raise ProtocolError("each character requires a distinct Hermes profile")
            self._specs[spec.character_id] = spec
            profiles.add(spec.hermes_profile)
        if not self._specs:
            raise ProtocolError("at least one character is required")
        self.backend = backend
        self.max_interactions = max_interactions
        self._turns: dict[tuple[str, str], int] = {}
        self._active: dict[tuple[str, str], asyncio.Task[AgentReply]] = {}
        self._session_locks: dict[str, asyncio.Lock] = {}

    @staticmethod
    def conversation_id_for(user_id: str, conversation_id: str = "default") -> str:
        validate_identifier(user_id, "user_id")
        validate_identifier(conversation_id, "conversation_id")
        return _scope("scene-", user_id, conversation_id)

    def session_id_for(
        self, character_id: str, user_id: str, conversation_id: str = "default"
    ) -> str:
        self._character(character_id)
        self.conversation_id_for(user_id, conversation_id)
        return _scope("huahuo-", character_id, user_id, conversation_id)

    def _character(self, character_id: str) -> CharacterSpec:
        validate_identifier(character_id, "character_id")
        try:
            return self._specs[character_id]
        except KeyError as exc:
            raise ProtocolError("unknown character_id") from exc

    def interrupt(self, user_id: str, conversation_id: str = "default") -> int:
        """Invalidate the scene turn immediately, then request backend cancellation.

        Even a backend which ignores cancellation cannot emit a late utterance
        or action through this director. The engine must also stop already
        playing speech or animation when its input handler interrupts a turn.
        """
        self.conversation_id_for(user_id, conversation_id)
        scene = (user_id, conversation_id)
        turn_id = self._turns.get(scene, 0) + 1
        if turn_id > MAX_TURN_ID:
            raise ProtocolError("turn counter exhausted; begin a new conversation")
        self._turns[scene] = turn_id
        active = self._active.pop(scene, None)
        if active is not None and not active.done():
            active.cancel()
        return turn_id

    async def respond(
        self,
        character_id: str,
        user_id: str,
        text: str,
        *,
        conversation_id: str = "default",
    ) -> TurnResult:
        """Start a new turn and return only outputs still belonging to that turn."""
        spec = self._character(character_id)
        validate_text(text)
        scoped_conversation = self.conversation_id_for(user_id, conversation_id)
        scene = (user_id, conversation_id)
        owner_task = asyncio.current_task()
        turn_id = self.interrupt(user_id, conversation_id)
        utterances: list[Utterance] = []
        actions: list[ActionEvent] = []

        def current() -> bool:
            return self._turns.get(scene) == turn_id

        def discarded() -> TurnResult:
            return TurnResult(scoped_conversation, turn_id, discarded=True)

        for _ in range(self.max_interactions + 1):
            if not current():
                return discarded()
            session_id = self.session_id_for(spec.character_id, user_id, conversation_id)
            lock = self._session_locks.setdefault(session_id, asyncio.Lock())
            task: asyncio.Task[AgentReply] | None = None
            try:
                # Serialize local backend lifetimes for a persisted session.
                # Remote writers still rely on Hermes' durable session lease;
                # a stop acknowledgement is not proof the remote worker exited.
                async with lock:
                    if not current():
                        return discarded()
                    task = asyncio.create_task(self.backend.respond(spec, session_id, text))
                    self._active[scene] = task
                    reply = await task
            except asyncio.CancelledError:
                if not current():
                    return discarded()
                self.interrupt(user_id, conversation_id)
                raise
            except Exception:
                if not current():
                    return discarded()
                raise
            finally:
                if task is not None and self._active.get(scene) is task:
                    self._active.pop(scene, None)
            # Awaiting a child propagates the caller's cancellation into that
            # child. A backend may swallow it and return normally, leaving the
            # owner cancelled but this code running; do not publish that reply.
            if owner_task is not None and owner_task.cancelling():
                if current():
                    self.interrupt(user_id, conversation_id)
                raise asyncio.CancelledError
            if not current():
                return discarded()
            if not isinstance(reply, AgentReply):
                raise ProtocolError("backend must return AgentReply")
            if reply.action is not None and reply.action not in spec.allowed_actions:
                raise ProtocolError("character is not allowed to perform this action")
            target = None
            if reply.target_character_id is not None:
                target = self._character(reply.target_character_id)
                if target.character_id == spec.character_id:
                    raise ProtocolError("a character cannot relay to itself")
            utterances.append(Utterance(spec.character_id, reply.text))
            if reply.action is not None:
                actions.append(
                    ActionEvent(
                        request_id=str(uuid4()),
                        character_id=spec.character_id,
                        conversation_id=scoped_conversation,
                        turn_id=turn_id,
                        action=reply.action,
                    )
                )
            if target is None:
                break
            # This is a new input appended to the recipient's own session,
            # rather than transplanted history or a changed system prompt.
            text = _public_prompt(spec.character_id, reply.text)
            spec = target
        return TurnResult(scoped_conversation, turn_id, tuple(utterances), tuple(actions))

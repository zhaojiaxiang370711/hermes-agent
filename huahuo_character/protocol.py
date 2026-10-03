"""Small, provider-independent contracts for Huahuo character interaction.

The agent may choose words, an allowed semantic action and another character.
The director, rather than the model, owns identity and animation event scope.
"""

from __future__ import annotations

from dataclasses import dataclass
import re
from typing import Any, Mapping


ACTIONS = frozenset(
    {"greet", "explain", "celebrate", "approve", "think", "shrug", "apologize", "surprise"}
)
ACTION_SCHEMA = "huahuo.character.action.v1"
MAX_TEXT_CHARS = 8192
MAX_TURN_ID = (1 << 53) - 1
_IDENTIFIER = re.compile(r"[A-Za-z0-9][A-Za-z0-9_-]{0,63}\Z")


class ProtocolError(ValueError):
    """A character input or reply violates the public protocol."""


def validate_identifier(value: str, field: str) -> str:
    """Identifiers are names, never filesystem paths or executable expressions."""
    if not isinstance(value, str) or _IDENTIFIER.fullmatch(value) is None:
        raise ProtocolError(f"{field} must be a 1–64 character ASCII identifier")
    return value


def validate_text(value: str, field: str = "text") -> str:
    if not isinstance(value, str) or not value.strip() or len(value) > MAX_TEXT_CHARS:
        raise ProtocolError(f"{field} must contain 1–{MAX_TEXT_CHARS} characters")
    # JSON escape sequences can decode to isolated surrogate code points,
    # which cannot cross the UTF-8 boundary used by Hermes HTTP and Godot.
    try:
        value.encode("utf-8")
    except UnicodeEncodeError as exc:
        raise ProtocolError(f"{field} must contain valid Unicode text") from exc
    return value


@dataclass(frozen=True)
class CharacterSpec:
    character_id: str
    hermes_profile: str
    persona: str
    allowed_actions: frozenset[str] = ACTIONS

    def __post_init__(self) -> None:
        validate_identifier(self.character_id, "character_id")
        validate_identifier(self.hermes_profile, "hermes_profile")
        validate_text(self.persona, "persona")
        if isinstance(self.allowed_actions, (str, bytes)):
            raise ProtocolError("allowed_actions must be a collection of action names")
        try:
            allowed = frozenset(self.allowed_actions)
        except TypeError as exc:
            raise ProtocolError("allowed_actions must be a collection of action names") from exc
        if not allowed <= ACTIONS:
            raise ProtocolError("allowed_actions contains an unsupported semantic action")
        object.__setattr__(self, "allowed_actions", allowed)


@dataclass(frozen=True)
class AgentReply:
    text: str
    action: str | None = None
    target_character_id: str | None = None

    def __post_init__(self) -> None:
        validate_text(self.text)
        if self.action is not None and (
            not isinstance(self.action, str) or self.action not in ACTIONS
        ):
            raise ProtocolError("action must be an allowed semantic action name")
        if self.target_character_id is not None:
            validate_identifier(self.target_character_id, "target_character_id")

    @classmethod
    def from_dict(cls, payload: Mapping[str, Any]) -> AgentReply:
        """Reject additional model fields, including identity, priority and paths."""
        if not isinstance(payload, Mapping):
            raise ProtocolError("agent reply must be a JSON object")
        if set(payload) - {"text", "action", "target_character_id"}:
            raise ProtocolError("agent reply contains unsupported fields")
        if "text" not in payload:
            raise ProtocolError("agent reply requires text")
        return cls(
            text=payload["text"],
            action=payload.get("action"),
            target_character_id=payload.get("target_character_id"),
        )


@dataclass(frozen=True)
class Utterance:
    character_id: str
    text: str
    action_request_id: str | None = None

    def __post_init__(self) -> None:
        validate_identifier(self.character_id, "character_id")
        validate_text(self.text)
        if self.action_request_id is not None:
            validate_identifier(self.action_request_id, "action_request_id")

    def to_dict(self) -> dict[str, str]:
        payload = {"character_id": self.character_id, "text": self.text}
        if self.action_request_id is not None:
            payload["action_request_id"] = self.action_request_id
        return payload


@dataclass(frozen=True)
class ActionEvent:
    request_id: str
    character_id: str
    conversation_id: str
    turn_id: int
    action: str

    def __post_init__(self) -> None:
        validate_identifier(self.request_id, "request_id")
        validate_identifier(self.character_id, "character_id")
        validate_identifier(self.conversation_id, "conversation_id")
        if type(self.turn_id) is not int or not 1 <= self.turn_id <= MAX_TURN_ID:
            raise ProtocolError("turn_id must be a positive integer representable by Godot JSON")
        if not isinstance(self.action, str) or self.action not in ACTIONS:
            raise ProtocolError("action must be an allowed semantic action name")

    def to_dict(self) -> dict[str, str | int]:
        return {
            "type": "character.action",
            "schema": ACTION_SCHEMA,
            "request_id": self.request_id,
            "character_id": self.character_id,
            "conversation_id": self.conversation_id,
            "turn_id": self.turn_id,
            "action": self.action,
        }


@dataclass(frozen=True)
class TurnResult:
    conversation_id: str
    turn_id: int
    utterances: tuple[Utterance, ...] = ()
    actions: tuple[ActionEvent, ...] = ()
    discarded: bool = False

    def to_dict(self) -> dict[str, Any]:
        return {
            "conversation_id": self.conversation_id,
            "turn_id": self.turn_id,
            "utterances": [item.to_dict() for item in self.utterances],
            "actions": [item.to_dict() for item in self.actions],
            "discarded": self.discarded,
        }

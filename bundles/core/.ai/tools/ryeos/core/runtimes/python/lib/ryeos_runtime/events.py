"""Daemon-backed bundle/domain event helpers for Python tools.

Bundle identity is intentionally not an argument. The daemon derives the
effective bundle from the verified executing tool context attached to the
callback token.
"""

from __future__ import annotations

from typing import Any

from ._rpc import RyeOSRuntimeError, request

__all__ = ["RyeOSRuntimeError", "append", "create_chain", "read_chain", "scan"]


def append(
    *,
    event_kind: str,
    chain_id: str,
    event_type: str,
    payload: Any | None = None,
    schema_version: int = 1,
    expected_chain_head_hash: str | None = None,
    idempotency_key: str | None = None,
    correlation_id: str | None = None,
    causation_id: str | None = None,
) -> dict[str, Any]:
    params: dict[str, Any] = {
        "event_kind": event_kind,
        "chain_id": chain_id,
        "event_type": event_type,
        "schema_version": schema_version,
        "payload": {} if payload is None else payload,
    }
    _put_optional(params, "expected_chain_head_hash", expected_chain_head_hash)
    _put_optional(params, "idempotency_key", idempotency_key)
    _put_optional(params, "correlation_id", correlation_id)
    _put_optional(params, "causation_id", causation_id)
    return request("runtime.domain_events_append", params)


def create_chain(
    *,
    event_kind: str,
    chain_id: str,
    event_type: str,
    payload: Any | None = None,
    schema_version: int = 1,
    idempotency_key: str | None = None,
    correlation_id: str | None = None,
    causation_id: str | None = None,
) -> dict[str, Any]:
    return append(
        event_kind=event_kind,
        chain_id=chain_id,
        event_type=event_type,
        payload=payload,
        schema_version=schema_version,
        expected_chain_head_hash=None,
        idempotency_key=idempotency_key,
        correlation_id=correlation_id,
        causation_id=causation_id,
    )


def read_chain(*, event_kind: str, chain_id: str) -> list[dict[str, Any]]:
    result = request(
        "runtime.domain_events_read_chain",
        {"event_kind": event_kind, "chain_id": chain_id},
    )
    return result.get("events", [])


def scan(*, event_kind: str) -> list[dict[str, Any]]:
    result = request("runtime.domain_events_scan", {"event_kind": event_kind})
    return result.get("events", [])


def _put_optional(params: dict[str, Any], key: str, value: Any | None) -> None:
    if value is not None:
        params[key] = value

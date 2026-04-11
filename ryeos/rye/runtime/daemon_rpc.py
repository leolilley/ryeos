from __future__ import annotations

from contextlib import contextmanager
from contextvars import ContextVar, Token
import itertools
import os
from pathlib import Path
import socket
import struct
import tempfile
from typing import Any, Dict


_SOCKET_PATH: ContextVar[str | None] = ContextVar("ryeosd_socket_path", default=None)
_THREAD_ID: ContextVar[str | None] = ContextVar("ryeosd_thread_id", default=None)
_CHAIN_ROOT_ID: ContextVar[str | None] = ContextVar("ryeosd_chain_root_id", default=None)


class RpcError(RuntimeError):
    def __init__(
        self,
        code: str,
        message: str,
        *,
        retryable: bool = False,
        details: Any = None,
    ) -> None:
        super().__init__(f"{code}: {message}")
        self.code = code
        self.message = message
        self.retryable = retryable
        self.details = details


class DaemonRpcClient:
    _request_ids = itertools.count(1)

    def __init__(self, socket_path: str):
        self.socket_path = socket_path

    def request(self, method: str, params: Dict[str, Any]) -> Any:
        request_id = next(self._request_ids)
        payload = _pack(
            {
                "request_id": request_id,
                "method": method,
                "params": params,
            }
        )

        with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as sock:
            sock.connect(self.socket_path)
            sock.sendall(struct.pack(">I", len(payload)))
            sock.sendall(payload)

            frame_len = struct.unpack(">I", _recv_exact(sock, 4))[0]
            response = _unpack(_recv_exact(sock, frame_len))

        if response.get("request_id") != request_id:
            raise RpcError(
                "invalid_response",
                f"response request_id {response.get('request_id')!r} did not match {request_id}",
            )

        error = response.get("error")
        if error is not None:
            raise RpcError(
                error.get("code", "request_failed"),
                error.get("message", "request failed"),
                retryable=bool(error.get("retryable", False)),
                details=error.get("details"),
            )

        return response.get("result")


class ThreadLifecycleClient:
    def __init__(self, socket_path: str):
        self._rpc = DaemonRpcClient(socket_path)

    @classmethod
    def from_request(cls, request: Dict[str, Any]) -> "ThreadLifecycleClient":
        runtime = request.get("runtime") or {}
        socket_path = runtime.get("socket_path")
        if not socket_path:
            raise RpcError("invalid_request", "runtime.socket_path is required")
        return cls(socket_path)

    def attach_process(
        self,
        thread_id: str,
        pid: int,
        pgid: int,
        metadata: Dict[str, Any] | None = None,
    ) -> Any:
        return self._rpc.request(
            "threads.attach_process",
            {
                "thread_id": thread_id,
                "pid": pid,
                "pgid": pgid,
                "metadata": metadata,
            },
        )

    def mark_running(self, thread_id: str) -> Any:
        return self._rpc.request(
            "threads.mark_running",
            {
                "thread_id": thread_id,
            },
        )

    def finalize_thread(
        self,
        thread_id: str,
        status: str,
        *,
        outcome_code: str | None = None,
        result: Dict[str, Any] | None = None,
        error: Dict[str, Any] | None = None,
        metadata: Dict[str, Any] | None = None,
        artifacts: list[Dict[str, Any]] | None = None,
        final_cost: Dict[str, Any] | None = None,
        summary_json: Dict[str, Any] | None = None,
    ) -> Any:
        return self._rpc.request(
            "threads.finalize",
            {
                "thread_id": thread_id,
                "status": status,
                "outcome_code": outcome_code,
                "result": result,
                "error": error,
                "metadata": metadata,
                "artifacts": artifacts or [],
                "final_cost": final_cost,
                "summary_json": summary_json,
            },
        )

    def get_thread(self, thread_id: str) -> Any:
        return self._rpc.request(
            "threads.get",
            {
                "thread_id": thread_id,
            },
        )

    def list_threads(self, limit: int = 20) -> Any:
        return self._rpc.request(
            "threads.list",
            {
                "limit": limit,
            },
        )

    def list_children(self, thread_id: str) -> Any:
        return self._rpc.request(
            "threads.children",
            {
                "thread_id": thread_id,
            },
        )

    def get_chain(self, thread_id: str) -> Any:
        return self._rpc.request(
            "threads.chain",
            {
                "thread_id": thread_id,
            },
        )

    def request_continuation(
        self,
        thread_id: str,
        reason: str | None = None,
    ) -> Any:
        return self._rpc.request(
            "threads.request_continuation",
            {
                "thread_id": thread_id,
                "reason": reason,
            },
        )

    def send_command(
        self,
        thread_id: str,
        command_type: str,
        params: Dict[str, Any] | None = None,
    ) -> Any:
        return self._rpc.request(
            "commands.submit",
            {
                "thread_id": thread_id,
                "command_type": command_type,
                "params": params,
            },
        )

    def append_event(
        self,
        thread_id: str,
        event_type: str,
        storage_class: str,
        payload: Dict[str, Any] | None = None,
    ) -> Any:
        return self._rpc.request(
            "events.append",
            {
                "thread_id": thread_id,
                "event": {
                    "event_type": event_type,
                    "storage_class": storage_class,
                    "payload": payload or {},
                },
            },
        )

    def append_events(self, thread_id: str, events: list[Dict[str, Any]]) -> Any:
        return self._rpc.request(
            "events.append_batch",
            {"thread_id": thread_id, "events": events},
        )

    def replay_events(
        self,
        *,
        chain_root_id: str | None = None,
        thread_id: str | None = None,
        after_chain_seq: int | None = None,
        limit: int = 200,
    ) -> Any:
        return self._rpc.request(
            "events.replay",
            {
                "chain_root_id": chain_root_id,
                "thread_id": thread_id,
                "after_chain_seq": after_chain_seq,
                "limit": limit,
            },
        )

    def claim_commands(self, thread_id: str, timeout_ms: int | None = None) -> Any:
        return self._rpc.request(
            "commands.claim",
            {"thread_id": thread_id, "timeout_ms": timeout_ms},
        )

    def complete_command(
        self, command_id: int, status: str, result: Dict[str, Any] | None = None
    ) -> Any:
        return self._rpc.request(
            "commands.complete",
            {
                "command_id": command_id,
                "status": status,
                "result": result,
            },
        )

    def reserve_budget(
        self,
        thread_id: str,
        budget_parent_id: str,
        reserved_spend: float,
        metadata: Dict[str, Any] | None = None,
    ) -> Any:
        return self._rpc.request(
            "budgets.reserve",
            {
                "thread_id": thread_id,
                "budget_parent_id": budget_parent_id,
                "reserved_spend": reserved_spend,
                "metadata": metadata,
            },
        )

    def report_budget(
        self,
        thread_id: str,
        actual_spend: float,
        metadata: Dict[str, Any] | None = None,
    ) -> Any:
        return self._rpc.request(
            "budgets.report",
            {
                "thread_id": thread_id,
                "actual_spend": actual_spend,
                "metadata": metadata,
            },
        )

    def release_budget(
        self,
        thread_id: str,
        status: str,
        metadata: Dict[str, Any] | None = None,
    ) -> Any:
        return self._rpc.request(
            "budgets.release",
            {
                "thread_id": thread_id,
                "status": status,
                "metadata": metadata,
            },
        )

    def get_budget(self, thread_id: str) -> Any:
        return self._rpc.request("budgets.get", {"thread_id": thread_id})

    def publish_artifact(
        self,
        thread_id: str,
        artifact_type: str,
        uri: str,
        *,
        content_hash: str | None = None,
        metadata: Dict[str, Any] | None = None,
    ) -> Any:
        return self._rpc.request(
            "artifacts.publish",
            {
                "thread_id": thread_id,
                "artifact_type": artifact_type,
                "uri": uri,
                "content_hash": content_hash,
                "metadata": metadata,
            },
        )


def resolve_daemon_socket_path(explicit: str | None = None) -> str | None:
    if explicit:
        return explicit

    socket_path = _SOCKET_PATH.get()
    if socket_path:
        return socket_path

    env_socket_path = os.environ.get("RYEOSD_SOCKET_PATH")
    if env_socket_path:
        return env_socket_path

    runtime_dir = os.environ.get("XDG_RUNTIME_DIR")
    if runtime_dir:
        return str(Path(runtime_dir) / "ryeosd.sock")

    uid = os.geteuid() if hasattr(os, "geteuid") else 0
    return str(Path(tempfile.gettempdir()) / f"ryeosd-{uid}" / "ryeosd.sock")


@contextmanager
def daemon_runtime_context(
    *,
    socket_path: str,
    thread_id: str | None = None,
    chain_root_id: str | None = None,
):
    tokens: list[tuple[ContextVar[str | None], Token[str | None]]] = [
        (_SOCKET_PATH, _SOCKET_PATH.set(socket_path)),
        (_THREAD_ID, _THREAD_ID.set(thread_id)),
        (_CHAIN_ROOT_ID, _CHAIN_ROOT_ID.set(chain_root_id)),
    ]
    try:
        yield
    finally:
        for var, token in reversed(tokens):
            var.reset(token)


def get_daemon_runtime_context() -> Dict[str, str | None]:
    return {
        "socket_path": _SOCKET_PATH.get(),
        "thread_id": _THREAD_ID.get(),
        "chain_root_id": _CHAIN_ROOT_ID.get(),
    }


def require_daemon_runtime_context(
    *,
    thread_id: str | None = None,
) -> tuple[ThreadLifecycleClient, str, Dict[str, str | None]]:
    context = get_daemon_runtime_context()
    socket_path = context.get("socket_path")
    if not socket_path:
        raise RpcError(
            "missing_runtime_context",
            "daemon runtime context is required for event emission",
        )

    resolved_thread_id = thread_id or context.get("thread_id")
    if not resolved_thread_id:
        raise RpcError(
            "missing_runtime_context",
            "daemon runtime thread_id is required for event emission",
        )

    return ThreadLifecycleClient(socket_path), resolved_thread_id, context


def _recv_exact(sock: socket.socket, size: int) -> bytes:
    chunks = bytearray()
    while len(chunks) < size:
        chunk = sock.recv(size - len(chunks))
        if not chunk:
            raise RpcError("connection_closed", "daemon closed the socket mid-frame")
        chunks.extend(chunk)
    return bytes(chunks)


def _pack(value: Any) -> bytes:
    if value is None:
        return b"\xc0"
    if value is False:
        return b"\xc2"
    if value is True:
        return b"\xc3"
    if isinstance(value, int):
        return _pack_int(value)
    if isinstance(value, float):
        return b"\xcb" + struct.pack(">d", value)
    if isinstance(value, str):
        encoded = value.encode("utf-8")
        length = len(encoded)
        if length <= 31:
            return bytes([0xA0 | length]) + encoded
        if length <= 0xFF:
            return b"\xd9" + struct.pack(">B", length) + encoded
        if length <= 0xFFFF:
            return b"\xda" + struct.pack(">H", length) + encoded
        return b"\xdb" + struct.pack(">I", length) + encoded
    if isinstance(value, (list, tuple)):
        length = len(value)
        header = _pack_array_header(length)
        return header + b"".join(_pack(item) for item in value)
    if isinstance(value, dict):
        length = len(value)
        header = _pack_map_header(length)
        parts = [header]
        for key, item in value.items():
            parts.append(_pack(key))
            parts.append(_pack(item))
        return b"".join(parts)
    raise TypeError(f"unsupported MessagePack type: {type(value)!r}")


def _pack_int(value: int) -> bytes:
    if 0 <= value <= 0x7F:
        return struct.pack(">B", value)
    if -32 <= value < 0:
        return struct.pack(">b", value)
    if 0 <= value <= 0xFF:
        return b"\xcc" + struct.pack(">B", value)
    if 0 <= value <= 0xFFFF:
        return b"\xcd" + struct.pack(">H", value)
    if 0 <= value <= 0xFFFFFFFF:
        return b"\xce" + struct.pack(">I", value)
    if 0 <= value <= 0xFFFFFFFFFFFFFFFF:
        return b"\xcf" + struct.pack(">Q", value)
    if -0x80 <= value < 0:
        return b"\xd0" + struct.pack(">b", value)
    if -0x8000 <= value < -0x80:
        return b"\xd1" + struct.pack(">h", value)
    if -0x80000000 <= value < -0x8000:
        return b"\xd2" + struct.pack(">i", value)
    if -0x8000000000000000 <= value < -0x80000000:
        return b"\xd3" + struct.pack(">q", value)
    raise OverflowError("integer out of MessagePack range")


def _pack_array_header(length: int) -> bytes:
    if length <= 15:
        return bytes([0x90 | length])
    if length <= 0xFFFF:
        return b"\xdc" + struct.pack(">H", length)
    return b"\xdd" + struct.pack(">I", length)


def _pack_map_header(length: int) -> bytes:
    if length <= 15:
        return bytes([0x80 | length])
    if length <= 0xFFFF:
        return b"\xde" + struct.pack(">H", length)
    return b"\xdf" + struct.pack(">I", length)


def _unpack(data: bytes) -> Any:
    value, offset = _unpack_from(memoryview(data), 0)
    if offset != len(data):
        raise RpcError("invalid_response", "extra trailing MessagePack bytes")
    return value


def _unpack_from(data: memoryview, offset: int) -> tuple[Any, int]:
    marker = data[offset]
    offset += 1

    if marker <= 0x7F:
        return marker, offset
    if marker >= 0xE0:
        return marker - 0x100, offset
    if 0xA0 <= marker <= 0xBF:
        length = marker & 0x1F
        return _read_str(data, offset, length)
    if 0x90 <= marker <= 0x9F:
        return _read_array(data, offset, marker & 0x0F)
    if 0x80 <= marker <= 0x8F:
        return _read_map(data, offset, marker & 0x0F)

    if marker == 0xC0:
        return None, offset
    if marker == 0xC2:
        return False, offset
    if marker == 0xC3:
        return True, offset
    if marker == 0xCB:
        return struct.unpack(">d", bytes(data[offset : offset + 8]))[0], offset + 8
    if marker == 0xCC:
        return data[offset], offset + 1
    if marker == 0xCD:
        return struct.unpack(">H", bytes(data[offset : offset + 2]))[0], offset + 2
    if marker == 0xCE:
        return struct.unpack(">I", bytes(data[offset : offset + 4]))[0], offset + 4
    if marker == 0xCF:
        return struct.unpack(">Q", bytes(data[offset : offset + 8]))[0], offset + 8
    if marker == 0xD0:
        return struct.unpack(">b", bytes(data[offset : offset + 1]))[0], offset + 1
    if marker == 0xD1:
        return struct.unpack(">h", bytes(data[offset : offset + 2]))[0], offset + 2
    if marker == 0xD2:
        return struct.unpack(">i", bytes(data[offset : offset + 4]))[0], offset + 4
    if marker == 0xD3:
        return struct.unpack(">q", bytes(data[offset : offset + 8]))[0], offset + 8
    if marker == 0xD9:
        length = data[offset]
        return _read_str(data, offset + 1, length)
    if marker == 0xDA:
        length = struct.unpack(">H", bytes(data[offset : offset + 2]))[0]
        return _read_str(data, offset + 2, length)
    if marker == 0xDB:
        length = struct.unpack(">I", bytes(data[offset : offset + 4]))[0]
        return _read_str(data, offset + 4, length)
    if marker == 0xDC:
        length = struct.unpack(">H", bytes(data[offset : offset + 2]))[0]
        return _read_array(data, offset + 2, length)
    if marker == 0xDD:
        length = struct.unpack(">I", bytes(data[offset : offset + 4]))[0]
        return _read_array(data, offset + 4, length)
    if marker == 0xDE:
        length = struct.unpack(">H", bytes(data[offset : offset + 2]))[0]
        return _read_map(data, offset + 2, length)
    if marker == 0xDF:
        length = struct.unpack(">I", bytes(data[offset : offset + 4]))[0]
        return _read_map(data, offset + 4, length)

    raise RpcError("invalid_response", f"unsupported MessagePack marker 0x{marker:02x}")


def _read_str(data: memoryview, offset: int, length: int) -> tuple[str, int]:
    end = offset + length
    return bytes(data[offset:end]).decode("utf-8"), end


def _read_array(data: memoryview, offset: int, length: int) -> tuple[list[Any], int]:
    items: list[Any] = []
    for _ in range(length):
        item, offset = _unpack_from(data, offset)
        items.append(item)
    return items, offset


def _read_map(data: memoryview, offset: int, length: int) -> tuple[dict[Any, Any], int]:
    items: dict[Any, Any] = {}
    for _ in range(length):
        key, offset = _unpack_from(data, offset)
        value, offset = _unpack_from(data, offset)
        items[key] = value
    return items, offset

"""Tiny RyeOS daemon UDS client for Python subprocess tools.

The daemon callback protocol is a 4-byte big-endian length prefix plus
MessagePack frames. This module intentionally implements only the
JSON-shaped MessagePack subset emitted/accepted by RyeOS runtime RPCs so
bundle tools do not need an external Python dependency.
"""

from __future__ import annotations

import os
import socket
import struct
from dataclasses import dataclass
from typing import Any


class RyeOSRuntimeError(RuntimeError):
    """Raised when a daemon-backed runtime callback fails."""

    def __init__(self, message: str, *, code: str | None = None, retryable: bool = False, details: Any = None):
        super().__init__(message)
        self.code = code
        self.retryable = retryable
        self.details = details


@dataclass(frozen=True)
class RuntimeContext:
    socket_path: str
    callback_token: str
    thread_auth_token: str
    thread_id: str

    @classmethod
    def from_env(cls) -> "RuntimeContext":
        missing = [
            name
            for name in (
                "RYEOSD_SOCKET_PATH",
                "RYEOSD_CALLBACK_TOKEN",
                "RYEOSD_THREAD_AUTH_TOKEN",
                "RYEOSD_THREAD_ID",
            )
            if not os.environ.get(name)
        ]
        if missing:
            raise RyeOSRuntimeError(
                "RyeOS daemon callback environment is missing: " + ", ".join(missing)
            )
        return cls(
            socket_path=os.environ["RYEOSD_SOCKET_PATH"],
            callback_token=os.environ["RYEOSD_CALLBACK_TOKEN"],
            thread_auth_token=os.environ["RYEOSD_THREAD_AUTH_TOKEN"],
            thread_id=os.environ["RYEOSD_THREAD_ID"],
        )


_REQUEST_ID = 0


def request(method: str, params: dict[str, Any], *, context: RuntimeContext | None = None) -> Any:
    """Send one runtime RPC request over the daemon callback UDS."""

    global _REQUEST_ID
    ctx = context or RuntimeContext.from_env()
    _REQUEST_ID += 1
    request_id = _REQUEST_ID
    body = dict(params)
    body["callback_token"] = ctx.callback_token
    body["thread_auth_token"] = ctx.thread_auth_token
    body["thread_id"] = ctx.thread_id
    frame = _pack({"request_id": request_id, "method": method, "params": body})

    try:
        with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as sock:
            sock.connect(ctx.socket_path)
            sock.sendall(struct.pack(">I", len(frame)) + frame)
            header = _recv_exact(sock, 4)
            length = struct.unpack(">I", header)[0]
            response = _unpack(_recv_exact(sock, length))
    except OSError as exc:
        raise RyeOSRuntimeError(f"daemon callback I/O failed: {exc}") from exc

    if not isinstance(response, dict):
        raise RyeOSRuntimeError("daemon response was not an RPC object")
    actual_id = response.get("request_id")
    if actual_id is not None and actual_id != request_id:
        raise RyeOSRuntimeError(f"response request_id {actual_id!r} did not match {request_id}")
    if response.get("error"):
        err = response["error"]
        message = err.get("message") or str(err)
        code = err.get("code")
        raise RyeOSRuntimeError(
            f"{code}: {message}" if code else message,
            code=code,
            retryable=bool(err.get("retryable")),
            details=err.get("details"),
        )
    return response.get("result")


def _recv_exact(sock: socket.socket, length: int) -> bytes:
    chunks: list[bytes] = []
    remaining = length
    while remaining:
        chunk = sock.recv(remaining)
        if not chunk:
            raise RyeOSRuntimeError("daemon closed the socket mid-frame")
        chunks.append(chunk)
        remaining -= len(chunk)
    return b"".join(chunks)


def _pack(value: Any) -> bytes:
    if value is None:
        return b"\xc0"
    if value is False:
        return b"\xc2"
    if value is True:
        return b"\xc3"
    if isinstance(value, int) and not isinstance(value, bool):
        if 0 <= value <= 0x7F:
            return bytes([value])
        if -32 <= value < 0:
            return bytes([0x100 + value])
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
        if -0x8000 <= value < 0:
            return b"\xd1" + struct.pack(">h", value)
        if -0x80000000 <= value < 0:
            return b"\xd2" + struct.pack(">i", value)
        return b"\xd3" + struct.pack(">q", value)
    if isinstance(value, float):
        return b"\xcb" + struct.pack(">d", value)
    if isinstance(value, str):
        raw = value.encode("utf-8")
        if len(raw) <= 31:
            return bytes([0xA0 | len(raw)]) + raw
        if len(raw) <= 0xFF:
            return b"\xd9" + struct.pack(">B", len(raw)) + raw
        if len(raw) <= 0xFFFF:
            return b"\xda" + struct.pack(">H", len(raw)) + raw
        return b"\xdb" + struct.pack(">I", len(raw)) + raw
    if isinstance(value, (list, tuple)):
        prefix = _pack_len(len(value), 0x90, b"\xdc", b"\xdd")
        return prefix + b"".join(_pack(v) for v in value)
    if isinstance(value, dict):
        prefix = _pack_len(len(value), 0x80, b"\xde", b"\xdf")
        parts = [prefix]
        for key, item in value.items():
            if not isinstance(key, str):
                raise TypeError(f"MessagePack map key must be str, got {type(key).__name__}")
            parts.append(_pack(key))
            parts.append(_pack(item))
        return b"".join(parts)
    raise TypeError(f"unsupported MessagePack value: {type(value).__name__}")


def _pack_len(length: int, fix_base: int, marker16: bytes, marker32: bytes) -> bytes:
    if length <= 15:
        return bytes([fix_base | length])
    if length <= 0xFFFF:
        return marker16 + struct.pack(">H", length)
    return marker32 + struct.pack(">I", length)


def _unpack(data: bytes) -> Any:
    value, offset = _unpack_at(data, 0)
    if offset != len(data):
        raise RyeOSRuntimeError("trailing bytes in MessagePack response")
    return value


def _unpack_at(data: bytes, offset: int) -> tuple[Any, int]:
    if offset >= len(data):
        raise RyeOSRuntimeError("unexpected end of MessagePack response")
    marker = data[offset]
    offset += 1
    if marker <= 0x7F:
        return marker, offset
    if marker >= 0xE0:
        return marker - 0x100, offset
    if 0xA0 <= marker <= 0xBF:
        return _read_str(data, offset, marker & 0x1F)
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
    if marker == 0xCC:
        return _unpack_num(data, offset, ">B")
    if marker == 0xCD:
        return _unpack_num(data, offset, ">H")
    if marker == 0xCE:
        return _unpack_num(data, offset, ">I")
    if marker == 0xCF:
        return _unpack_num(data, offset, ">Q")
    if marker == 0xD0:
        return _unpack_num(data, offset, ">b")
    if marker == 0xD1:
        return _unpack_num(data, offset, ">h")
    if marker == 0xD2:
        return _unpack_num(data, offset, ">i")
    if marker == 0xD3:
        return _unpack_num(data, offset, ">q")
    if marker == 0xCB:
        return _unpack_num(data, offset, ">d")
    if marker == 0xD9:
        length, offset = _unpack_num(data, offset, ">B")
        return _read_str(data, offset, length)
    if marker == 0xDA:
        length, offset = _unpack_num(data, offset, ">H")
        return _read_str(data, offset, length)
    if marker == 0xDB:
        length, offset = _unpack_num(data, offset, ">I")
        return _read_str(data, offset, length)
    if marker == 0xDC:
        length, offset = _unpack_num(data, offset, ">H")
        return _read_array(data, offset, length)
    if marker == 0xDD:
        length, offset = _unpack_num(data, offset, ">I")
        return _read_array(data, offset, length)
    if marker == 0xDE:
        length, offset = _unpack_num(data, offset, ">H")
        return _read_map(data, offset, length)
    if marker == 0xDF:
        length, offset = _unpack_num(data, offset, ">I")
        return _read_map(data, offset, length)
    raise RyeOSRuntimeError(f"unsupported MessagePack marker 0x{marker:02x}")


def _unpack_num(data: bytes, offset: int, fmt: str) -> tuple[Any, int]:
    size = struct.calcsize(fmt)
    end = offset + size
    if end > len(data):
        raise RyeOSRuntimeError("truncated MessagePack number")
    return struct.unpack(fmt, data[offset:end])[0], end


def _read_str(data: bytes, offset: int, length: int) -> tuple[str, int]:
    end = offset + length
    if end > len(data):
        raise RyeOSRuntimeError("truncated MessagePack string")
    return data[offset:end].decode("utf-8"), end


def _read_array(data: bytes, offset: int, length: int) -> tuple[list[Any], int]:
    result = []
    for _ in range(length):
        item, offset = _unpack_at(data, offset)
        result.append(item)
    return result, offset


def _read_map(data: bytes, offset: int, length: int) -> tuple[dict[str, Any], int]:
    result = {}
    for _ in range(length):
        key, offset = _unpack_at(data, offset)
        value, offset = _unpack_at(data, offset)
        if not isinstance(key, str):
            raise RyeOSRuntimeError("MessagePack response map key was not a string")
        result[key] = value
    return result, offset

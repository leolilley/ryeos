#!/usr/bin/env python3
"""RyeOS persistent-session adapter for the admitted local Qwen worker."""

from __future__ import annotations

import hashlib
import json
import math
import os
import queue
import re
import stat
import struct
import sys
import threading
from pathlib import Path
from typing import Any, Iterator


PROTOCOL = "ryeos.persistent-session"
VERSION = 1
MAX_FRAME_BYTES = 16 * 1024 * 1024
MAX_MESSAGES = 512
MAX_MESSAGE_BYTES = 8 * 1024 * 1024
MAX_TOOLS = 256
MAX_TOOL_BYTES = 4 * 1024 * 1024


def _is_within(path: Path, roots: tuple[Path, ...]) -> bool:
    return any(path == root or root in path.parents for root in roots)


def _verify_loaded_module_origins(*roots: Path) -> None:
    admitted = tuple(root.resolve(strict=True) for root in roots)
    for name, module in sorted(sys.modules.items()):
        origin = getattr(module, "__file__", None)
        if origin is None:
            continue
        resolved = Path(origin).resolve(strict=True)
        if not _is_within(resolved, admitted):
            raise RuntimeError(
                f"local worker module {name!r} escaped the admitted realization"
            )


def _prepare_environment() -> tuple[Path, Path, Path, Path]:
    session_fd = os.environ.get("RYEOS_SESSION_FD")
    workspace = Path.cwd().resolve()
    worker_root = Path(__file__).resolve(strict=True).parent
    if workspace not in worker_root.parents:
        raise RuntimeError("local worker source escaped the admitted workspace")
    scratch = workspace / "scratch"
    home = scratch / "home"
    cache = scratch / "cache"
    temporary = scratch / "tmp"
    for path in (home, cache, temporary):
        path.mkdir(parents=True, exist_ok=True)
    environment = {
        "HOME": str(home),
        "XDG_CACHE_HOME": str(cache),
        "TMPDIR": str(temporary),
        "PYTHONHOME": str(workspace / "runtime" / "python"),
        "PYTHONNOUSERSITE": "1",
        "PYTHONDONTWRITEBYTECODE": "1",
        "PYTHONHASHSEED": "0",
        "PYTHONSAFEPATH": "1",
        "PATH": "",
        "DEV": "CPU",
        "CACHELEVEL": "0",
        "CCACHE": "0",
        "LANG": "C",
        "LC_ALL": "C",
    }
    if session_fd is not None:
        environment["RYEOS_SESSION_FD"] = session_fd
    # The admitted worker contract is the whole environment. In particular,
    # tinygrad development switches such as REGEN and device selectors must
    # never arrive from the node and silently turn a sealed launch into host
    # discovery or a different backend.
    os.environ.clear()
    os.environ.update(environment)
    sys.path[:] = [
        str(worker_root),
        str(workspace / "tinygrad"),
        *[entry for entry in sys.path if entry and "site-packages" not in entry],
    ]
    runtime_root = (workspace / "runtime").resolve(strict=True)
    python_root = (runtime_root / "python").resolve(strict=True)
    if Path(sys.prefix).resolve(strict=True) != python_root:
        raise RuntimeError("local worker interpreter prefix is outside the admitted runtime")
    if (
        not sys.flags.safe_path
        or sys.flags.hash_randomization != 0
        or hash("ryeos-local-worker") != -7902905501708591707
    ):
        raise RuntimeError("local worker interpreter did not honor the admitted hash seed")
    for entry in sys.path:
        resolved = Path(entry).resolve(strict=False)
        if resolved != workspace and workspace not in resolved.parents:
            raise RuntimeError("local worker import path escaped the admitted workspace")
    tinygrad_root = (workspace / "tinygrad").resolve(strict=True)
    _verify_loaded_module_origins(runtime_root, worker_root)
    return workspace, runtime_root, worker_root, tinygrad_root


WORKSPACE, RUNTIME_ROOT, WORKER_ROOT, TINYGRAD_ROOT = _prepare_environment()

from compiler import install_admitted_compiler  # noqa: E402

install_admitted_compiler(Path.cwd())

from model import MAX_CONTEXT, MAX_OUTPUT_TOKENS, MODEL_ID, QwenModel  # noqa: E402
from tokenizer import QwenTokenizer, render_chat  # noqa: E402

_verify_loaded_module_origins(RUNTIME_ROOT, WORKER_ROOT, TINYGRAD_ROOT)


def _read_exact(channel: int, length: int) -> bytes:
    output = bytearray()
    while len(output) < length:
        try:
            chunk = os.read(channel, length - len(output))
        except InterruptedError:
            continue
        if not chunk:
            raise EOFError("persistent-session channel closed")
        output.extend(chunk)
    return bytes(output)


def _strict_object(pairs: list[tuple[str, Any]]) -> dict[str, Any]:
    result: dict[str, Any] = {}
    for key, value in pairs:
        if key in result:
            raise ValueError(f"duplicate JSON object member: {key}")
        result[key] = value
    return result


def _reject_json_constant(value: str) -> None:
    raise ValueError(f"non-finite JSON number is forbidden: {value}")


def _strict_json_loads(encoded: bytes | str) -> Any:
    return json.loads(
        encoded,
        object_pairs_hook=_strict_object,
        parse_constant=_reject_json_constant,
    )


def _read_frame(channel: int) -> dict[str, Any]:
    length = struct.unpack(">I", _read_exact(channel, 4))[0]
    if length == 0 or length > MAX_FRAME_BYTES:
        raise ValueError("persistent-session frame is outside its bound")
    frame = _strict_json_loads(_read_exact(channel, length))
    if not isinstance(frame, dict) or set(frame) != {
        "protocol",
        "version",
        "kind",
        "request_id",
        "body",
    }:
        raise ValueError("persistent-session frame shape is not canonical")
    if frame["protocol"] != PROTOCOL or frame["version"] != VERSION:
        raise ValueError("persistent-session frame identity mismatch")
    return frame


def _write_frame(
    channel: int,
    lock: threading.Lock,
    kind: str,
    request_id: str | None,
    body: Any,
) -> None:
    frame = {
        "protocol": PROTOCOL,
        "version": VERSION,
        "kind": kind,
        "request_id": request_id,
        "body": body,
    }
    encoded = json.dumps(
        frame,
        ensure_ascii=False,
        separators=(",", ":"),
        allow_nan=False,
    ).encode("utf-8")
    if not encoded or len(encoded) > MAX_FRAME_BYTES:
        raise ValueError("persistent-session response exceeds its frame bound")
    framed = struct.pack(">I", len(encoded)) + encoded
    with lock:
        written = 0
        while written < len(framed):
            try:
                count = os.write(channel, framed[written:])
            except InterruptedError:
                continue
            if count == 0:
                raise EOFError("persistent-session channel closed while writing")
            written += count


class RequestInbox:
    def __init__(self, channel: int):
        self.channel = channel
        self.requests: queue.Queue[tuple[str, dict[str, Any], threading.Event] | BaseException] = queue.Queue(maxsize=1)
        self._current_lock = threading.Lock()
        self._current: tuple[str, threading.Event] | None = None

    def clear_current(self, request_id: str) -> None:
        with self._current_lock:
            if self._current is None or self._current[0] != request_id:
                raise RuntimeError("persistent worker current request changed")
            self._current = None

    def run(self) -> None:
        try:
            while True:
                frame = _read_frame(self.channel)
                kind, request_id, body = frame["kind"], frame["request_id"], frame["body"]
                if kind == "request":
                    if not isinstance(request_id, str) or not request_id or not isinstance(body, dict):
                        raise ValueError("persistent-session request frame is malformed")
                    cancelled = threading.Event()
                    with self._current_lock:
                        if self._current is not None or not self.requests.empty():
                            raise ValueError("persistent-session process received concurrent requests")
                        self._current = (request_id, cancelled)
                    self.requests.put((request_id, body, cancelled))
                elif kind == "cancel":
                    if body is not None or not isinstance(request_id, str):
                        raise ValueError("persistent-session cancel frame is malformed")
                    with self._current_lock:
                        if self._current is None or self._current[0] != request_id:
                            raise ValueError("persistent-session cancellation has no matching request")
                        self._current[1].set()
                else:
                    raise ValueError("persistent-session worker received an invalid frame kind")
        except BaseException as error:
            self.requests.put(error)


class OutputRouter:
    def __init__(self):
        self.buffer = ""
        self.mode = "undecided"

    def _split(self, tag: str, final: bool) -> tuple[str, bool]:
        if tag in self.buffer:
            before, self.buffer = self.buffer.split(tag, 1)
            return before, True
        hold = 0
        if not final:
            hold = max(
                (
                    size
                    for size in range(1, min(len(self.buffer), len(tag)) + 1)
                    if tag.startswith(self.buffer[-size:])
                ),
                default=0,
            )
        emitted = self.buffer[: len(self.buffer) - hold]
        self.buffer = self.buffer[len(self.buffer) - hold :]
        return emitted, False

    def route(self, text: str, final: bool = False) -> Iterator[tuple[str, str]]:
        self.buffer += text
        if self.mode == "undecided":
            if not final and len(self.buffer) < len("<think>") and "<think>".startswith(self.buffer):
                return
            if self.buffer.startswith("<think>"):
                self.mode = "reasoning"
                self.buffer = self.buffer[len("<think>") :]
            else:
                self.mode = "content"
        if self.mode == "reasoning":
            emitted, found = self._split("</think>", final)
            if emitted:
                yield "reasoning", emitted
            if not found:
                return
            self.mode = "content"
        if self.mode == "tool":
            return
        emitted, found = self._split("<tool_call>", final)
        if emitted:
            yield "content", emitted
        if found:
            self.mode = "tool"
            self.buffer = "<tool_call>" + self.buffer


def _validate_request(outer: dict[str, Any]) -> tuple[dict[str, Any], int, float, int]:
    if set(outer) != {"request_body", "request_body_sha256", "requested_output_ceiling"}:
        raise ValueError("local worker envelope shape is not canonical")
    request_body, body_digest, ceiling = (
        outer["request_body"],
        outer["request_body_sha256"],
        outer["requested_output_ceiling"],
    )
    if (
        not isinstance(request_body, str)
        or not isinstance(body_digest, str)
        or len(body_digest) != 64
        or any(character not in "0123456789abcdef" for character in body_digest)
        or not isinstance(ceiling, int)
        or isinstance(ceiling, bool)
        or ceiling <= 0
    ):
        raise ValueError("local worker envelope is malformed")
    if hashlib.sha256(request_body.encode("utf-8")).hexdigest() != body_digest:
        raise ValueError("local worker request bytes contradict their admitted digest")
    request = _strict_json_loads(request_body)
    if not isinstance(request, dict):
        raise ValueError("local Qwen request body is not an object")
    allowed = {
        "model",
        "messages",
        "tools",
        "stream",
        "stream_options",
        "max_tokens",
        "temperature",
        "seed",
        "enable_thinking",
    }
    unknown = sorted(set(request) - allowed)
    if unknown:
        raise ValueError(f"local Qwen request contains unsupported fields: {unknown}")
    if request.get("model") != MODEL_ID or request.get("stream") is not True:
        raise ValueError("local Qwen request names the wrong model or is not streaming")
    if request.get("stream_options") not in (None, {"include_usage": True}):
        raise ValueError("local Qwen stream options changed")
    if not isinstance(request.get("enable_thinking", True), bool):
        raise ValueError("local Qwen thinking mode must be a boolean")
    messages = request.get("messages")
    if not isinstance(messages, list) or not 0 < len(messages) <= MAX_MESSAGES:
        raise ValueError("local Qwen message count is outside its bound")
    message_bytes = len(json.dumps(messages, ensure_ascii=False, separators=(",", ":")).encode("utf-8"))
    if message_bytes > MAX_MESSAGE_BYTES:
        raise ValueError("local Qwen messages exceed their byte bound")
    tools = request.get("tools", [])
    if not isinstance(tools, list) or len(tools) > MAX_TOOLS or any(not isinstance(tool, dict) for tool in tools):
        raise ValueError("local Qwen tools are outside their bound")
    if len(json.dumps(tools, ensure_ascii=False, separators=(",", ":")).encode("utf-8")) > MAX_TOOL_BYTES:
        raise ValueError("local Qwen tools exceed their byte bound")
    output_limit = request.get("max_tokens", ceiling)
    if (
        not isinstance(output_limit, int)
        or isinstance(output_limit, bool)
        or output_limit <= 0
        or output_limit > ceiling
        or output_limit > MAX_OUTPUT_TOKENS
    ):
        raise ValueError("local Qwen output limit exceeds its admitted ceiling")
    temperature = request.get("temperature", 0.0)
    if (
        not isinstance(temperature, (int, float))
        or isinstance(temperature, bool)
        or not math.isfinite(float(temperature))
        or not 0.0 <= float(temperature) <= 2.0
    ):
        raise ValueError("local Qwen temperature is malformed")
    temperature = float(temperature)
    seed_value = request.get("seed")
    if temperature > 0.0 and seed_value is None:
        raise ValueError("sampled local Qwen requests require an explicit seed")
    seed = 0 if seed_value is None else seed_value
    if not isinstance(seed, int) or isinstance(seed, bool) or seed < 0 or seed > (1 << 63) - 1:
        raise ValueError("local Qwen seed is outside its bound")
    return request, output_limit, temperature, seed


def _parse_tools(raw: str, request_id: str) -> tuple[list[dict[str, Any]], str]:
    calls: list[dict[str, Any]] = []
    consumed: list[tuple[int, int]] = []
    for match in re.finditer(r"<tool_call>\s*(.*?)\s*</tool_call>", raw, re.DOTALL):
        try:
            value = json.loads(match.group(1))
        except json.JSONDecodeError:
            continue
        if (
            not isinstance(value, dict)
            or set(value) != {"name", "arguments"}
            or not isinstance(value["name"], str)
            or not value["name"]
            or not isinstance(value["arguments"], dict)
        ):
            continue
        canonical = json.dumps(value, ensure_ascii=False, sort_keys=True, separators=(",", ":"))
        call_id = "call_" + hashlib.sha256(
            f"{request_id}\0{len(calls)}\0{canonical}".encode("utf-8")
        ).hexdigest()[:24]
        calls.append({"id": call_id, "name": value["name"], "arguments": value["arguments"]})
        consumed.append(match.span())
    if not consumed:
        return [], raw
    remainder: list[str] = []
    position = 0
    for begin, end in consumed:
        remainder.append(raw[position:begin])
        position = end
    remainder.append(raw[position:])
    return calls, "".join(remainder)


class Worker:
    def __init__(self, *, load_model: bool = False):
        self.model_root = Path("model")
        self.tokenizer = QwenTokenizer(self.model_root) if load_model else None
        self.model = QwenModel(self.model_root) if load_model else None

    def _execute_in_process(
        self,
        request_id: str,
        outer: dict[str, Any],
        cancelled: threading.Event,
        emit: Any,
    ) -> dict[str, Any]:
        if self.tokenizer is None or self.model is None:
            raise RuntimeError("local Qwen model state is not resident")
        request, output_limit, temperature, seed = _validate_request(outer)
        rendered = render_chat(
            request["messages"],
            request.get("tools") or [],
            request.get("enable_thinking", True),
        )
        prompt_tokens = self.tokenizer.encode(rendered)
        if len(prompt_tokens) >= MAX_CONTEXT or len(prompt_tokens) + output_limit > MAX_CONTEXT:
            raise ValueError("local Qwen prompt plus output exceeds the admitted context")
        decoder = self.tokenizer.stream_decoder()
        router = OutputRouter()
        content_parts: list[str] = []
        reasoning_parts: list[str] = []
        output_tokens = 0
        finish_reason = "length"

        if cancelled.is_set():
            raise RuntimeError("local Qwen request was cancelled")
        for token_id in self.model.generate(prompt_tokens, output_limit, temperature, seed):
            if cancelled.is_set():
                raise RuntimeError("local Qwen request was cancelled")
            output_tokens += 1
            if token_id in self.tokenizer.end_tokens:
                finish_reason = "stop"
                break
            piece = decoder(token_id)
            for field, delta in router.route(piece):
                if not delta:
                    continue
                if field == "reasoning":
                    reasoning_parts.append(delta)
                    emit({"kind": "reasoning_delta", "text": delta})
                else:
                    content_parts.append(delta)
                    emit({"kind": "text_delta", "text": delta})
        tail = decoder(None)
        for field, delta in router.route(tail, final=True):
            if not delta:
                continue
            if field == "reasoning":
                reasoning_parts.append(delta)
                emit({"kind": "reasoning_delta", "text": delta})
            else:
                content_parts.append(delta)
                emit({"kind": "text_delta", "text": delta})

        tool_calls, unparsed = _parse_tools(router.buffer, request_id)
        if unparsed:
            content_parts.append(unparsed)
            emit({"kind": "text_delta", "text": unparsed})
        for index, call in enumerate(tool_calls):
            emit(
                {
                    "kind": "tool_use",
                    "id": call["id"],
                    "name": call["name"],
                    "stream_key": f"tool-{index}",
                    "arguments": call["arguments"],
                }
            )
        if tool_calls and finish_reason == "stop":
            finish_reason = "tool_calls"
        content = "".join(content_parts)
        reasoning = "".join(reasoning_parts)
        return {
            "answer": {
                "message": {
                    "role": "assistant",
                    "content": content if content else None,
                    "tool_calls": tool_calls if tool_calls else None,
                    "tool_call_id": None,
                    "reasoning_content": reasoning if reasoning else None,
                },
                "finish_reason": finish_reason,
            },
            "usage": {
                "input_tokens": len(prompt_tokens),
                "output_tokens": output_tokens,
                "reasoning_tokens": None,
            },
            "response_id": "local-" + outer["request_body_sha256"][:24],
        }

    def execute(
        self,
        request_id: str,
        outer: dict[str, Any],
        cancelled: threading.Event,
        emit: Any,
    ) -> dict[str, Any]:
        """Execute with resident immutable weights and fresh per-call state."""
        return self._execute_in_process(request_id, outer, cancelled, emit)


def main() -> int:
    channel_text = os.environ.get("RYEOS_SESSION_FD")
    if channel_text is None:
        raise RuntimeError("RYEOS_SESSION_FD is absent")
    if not channel_text.isascii() or not channel_text.isdecimal():
        raise RuntimeError("RYEOS_SESSION_FD is not canonical")
    channel_fd = int(channel_text)
    # Enforced isolation relocates the daemon-owned duplex channel onto stdin
    # because the pinned Bubblewrap backend preserves stdio but exposes no
    # arbitrary-FD mapping primitive. Disabled isolation retains the verified
    # source descriptor above stderr. Stdout/stderr are never channel authority.
    if channel_fd in (1, 2):
        raise RuntimeError("RYEOS_SESSION_FD overlaps output standard I/O")
    # The daemon creates and owns the AF_UNIX/SOCK_STREAM pair, then admits
    # exactly this inherited descriptor. The worker checks that it received a
    # socket without issuing socket-family introspection syscalls that a
    # hermetic worker sandbox need not grant.
    if not stat.S_ISSOCK(os.fstat(channel_fd).st_mode):
        raise RuntimeError("persistent-session descriptor is not a socket")
    channel = channel_fd
    write_lock = threading.Lock()
    worker = Worker(load_model=True)
    inbox = RequestInbox(channel)
    reader = threading.Thread(target=inbox.run, name="ryeos-session-reader", daemon=True)
    reader.start()
    _write_frame(channel, write_lock, "ready", None, None)
    while True:
        item = inbox.requests.get()
        if isinstance(item, BaseException):
            raise item
        request_id, body, cancelled = item
        try:
            result = worker.execute(
                request_id,
                body,
                cancelled,
                lambda delta: _write_frame(channel, write_lock, "delta", request_id, delta),
            )
            if cancelled.is_set():
                raise RuntimeError("local Qwen request was cancelled")
            _write_frame(channel, write_lock, "final", request_id, result)
        except BaseException as error:
            _write_frame(
                channel,
                write_lock,
                "error",
                request_id,
                {"message": str(error)[:2048]},
            )
        finally:
            inbox.clear_current(request_id)


if __name__ == "__main__":
    raise SystemExit(main())

"""Exact, dependency-free tokenizer and admitted Qwen chat rendering."""

from __future__ import annotations

import codecs
import hashlib
import itertools
import json
import re
import unicodedata
from pathlib import Path
from typing import Any, Callable


EXPECTED_SPLIT_PATTERN = (
    r"(?i:'s|'t|'re|'ve|'m|'ll|'d)|[^\r\n\p{L}\p{N}]?\p{L}+|\p{N}| "
    r"?[^\s\p{L}\p{N}]+[\r\n]*|\s*[\r\n]+|\s+(?!\S)|\s+"
)
EXPECTED_CHAT_TEMPLATE_SHA256 = "a55ee1b1660128b7098723e0abcd92caa0788061051c62d51cbe87d9cf1974d8"
EXPECTED_GENERATION_CONFIG = {
    "bos_token_id": 151_643,
    "do_sample": True,
    "eos_token_id": [151_645, 151_643],
    "pad_token_id": 151_643,
    "temperature": 0.6,
    "top_k": 20,
    "top_p": 0.95,
    "transformers_version": "4.51.0",
}


def _category_ranges(prefix: str) -> str:
    # Unicode's current L/N/Z categories end below this boundary. Compacting
    # adjacent codepoints keeps the stdlib-regex form bounded and deterministic.
    points = enumerate(
        codepoint
        for codepoint in range(0x323B0)
        if unicodedata.category(chr(codepoint)).startswith(prefix)
    )
    runs = [list(group) for _, group in itertools.groupby(points, lambda pair: pair[1] - pair[0])]
    return "".join(
        re.escape(chr(run[0][1]))
        + (f"-{re.escape(chr(run[-1][1]))}" if len(run) > 1 else "")
        for run in runs
    )


class QwenTokenizer:
    def __init__(self, model_root: Path):
        tokenizer_config = json.loads(
            (model_root / "tokenizer_config.json").read_text(encoding="utf-8")
        )
        template = tokenizer_config.get("chat_template")
        if (
            not isinstance(template, str)
            or hashlib.sha256(template.encode("utf-8")).hexdigest()
            != EXPECTED_CHAT_TEMPLATE_SHA256
        ):
            raise ValueError("Qwen chat template changed")
        generation_config = json.loads(
            (model_root / "generation_config.json").read_text(encoding="utf-8")
        )
        if generation_config != EXPECTED_GENERATION_CONFIG:
            raise ValueError("Qwen generation configuration changed")
        raw = json.loads((model_root / "tokenizer.json").read_text(encoding="utf-8"))
        if raw.get("normalizer") != {"type": "NFC"}:
            raise ValueError("Qwen tokenizer normalization behavior changed")
        model = raw.get("model")
        if not isinstance(model, dict) or model.get("type") != "BPE":
            raise ValueError("Qwen tokenizer is not the admitted BPE contract")
        if model.get("dropout") is not None or model.get("byte_fallback") is not False:
            raise ValueError("Qwen tokenizer BPE behavior changed")
        self._vocab: dict[str, int] = model["vocab"]
        if len(self._vocab) != 151_643 or len(set(self._vocab.values())) != len(self._vocab):
            raise ValueError("Qwen tokenizer vocabulary changed")
        merges = model["merges"]
        if len(merges) != 151_387:
            raise ValueError("Qwen tokenizer merge table changed")
        self._merge_ranks: dict[tuple[str, str], int] = {}
        for rank, merge in enumerate(merges):
            if (
                not isinstance(merge, list)
                or len(merge) != 2
                or not all(isinstance(part, str) and part for part in merge)
            ):
                raise ValueError("Qwen tokenizer has a malformed merge")
            left, right = merge
            pair = (left, right)
            if pair in self._merge_ranks:
                raise ValueError("Qwen tokenizer repeats a merge")
            self._merge_ranks[pair] = rank

        pre = raw.get("pre_tokenizer", {})
        pretokenizers = pre.get("pretokenizers") if pre.get("type") == "Sequence" else None
        try:
            split = pretokenizers[0]
            byte_level = pretokenizers[1]
        except (TypeError, IndexError):
            raise ValueError("Qwen tokenizer pre-tokenizer shape changed") from None
        if split != {
            "type": "Split",
            "pattern": {"Regex": EXPECTED_SPLIT_PATTERN},
            "behavior": "Isolated",
            "invert": False,
        } or byte_level != {
            "type": "ByteLevel",
            "add_prefix_space": False,
            "trim_offsets": False,
            "use_regex": False,
        }:
            raise ValueError("Qwen tokenizer pre-tokenizer behavior changed")

        added = raw.get("added_tokens")
        if not isinstance(added, list):
            raise ValueError("Qwen tokenizer added-token table is absent")
        self.added_tokens: dict[str, int] = {}
        self.special_tokens: dict[str, int] = {}
        for token in added:
            if (
                not isinstance(token, dict)
                or not isinstance(token.get("special"), bool)
                or any(token.get(flag) is not False for flag in ("single_word", "lstrip", "rstrip", "normalized"))
            ):
                raise ValueError("Qwen tokenizer added-token behavior changed")
            content, token_id = token.get("content"), token.get("id")
            if not isinstance(content, str) or not isinstance(token_id, int):
                raise ValueError("Qwen tokenizer added token is malformed")
            self.added_tokens[content] = token_id
            if token["special"]:
                self.special_tokens[content] = token_id
        if self.special_tokens.get("<|endoftext|>") != 151_643:
            raise ValueError("Qwen end-of-text token changed")
        if self.special_tokens.get("<|im_start|>") != 151_644:
            raise ValueError("Qwen message-start token changed")
        if self.special_tokens.get("<|im_end|>") != 151_645:
            raise ValueError("Qwen message-end token changed")

        base_bytes = [*range(33, 127), *range(161, 173), *range(174, 256)]
        self._byte_encoder = {byte: chr(byte) for byte in base_bytes}
        self._byte_encoder.update(
            {
                byte: chr(256 + index)
                for index, byte in enumerate(byte for byte in range(256) if byte not in base_bytes)
            }
        )
        self._byte_decoder = {character: byte for byte, character in self._byte_encoder.items()}
        self._id_to_bytes: dict[int, bytes] = {}
        for token, token_id in self._vocab.items():
            try:
                self._id_to_bytes[token_id] = bytes(self._byte_decoder[character] for character in token)
            except KeyError as error:
                raise ValueError(f"Qwen byte-level token contains an unknown symbol: {error}") from error
        self._id_to_bytes.update(
            {token_id: token.encode("utf-8") for token, token_id in self.added_tokens.items()}
        )

        whitespace = r"\t\n\x0b\x0c\r\x85" + _category_ranges("Z")
        numbers, letters = _category_ranges("N"), _category_ranges("L")
        self._split_words = re.compile(
            "(?i:'s|'t|'re|'ve|'m|'ll|'d)|"
            + f"[^\\r\\n{letters}{numbers}]?[{letters}]+|[{numbers}]|"
            + f" ?[^{whitespace}{letters}{numbers}]+[\\r\\n]*|"
            + f"[{whitespace}]*[\\r\\n]+|[{whitespace}]+(?![^{whitespace}])|[{whitespace}]+"
        )
        special_pattern = "|".join(
            re.escape(token) for token in sorted(self.added_tokens, key=len, reverse=True)
        )
        self._split_special = re.compile(special_pattern)
        self._bpe_cache: dict[str, tuple[int, ...]] = {}

    @property
    def end_tokens(self) -> frozenset[int]:
        return frozenset((151_643, 151_645))

    def _bpe(self, piece: str) -> tuple[int, ...]:
        cached = self._bpe_cache.get(piece)
        if cached is not None:
            return cached
        word = tuple(piece)
        while len(word) > 1:
            pairs = {(word[index], word[index + 1]) for index in range(len(word) - 1)}
            pair = min(pairs, key=lambda candidate: self._merge_ranks.get(candidate, 1 << 60))
            if pair not in self._merge_ranks:
                break
            merged: list[str] = []
            index = 0
            while index < len(word):
                if index + 1 < len(word) and (word[index], word[index + 1]) == pair:
                    merged.append(word[index] + word[index + 1])
                    index += 2
                else:
                    merged.append(word[index])
                    index += 1
            word = tuple(merged)
        try:
            result = tuple(self._vocab[token] for token in word)
        except KeyError as error:
            raise ValueError(f"Qwen BPE produced an unknown token: {error}") from error
        if len(self._bpe_cache) < 65_536:
            self._bpe_cache[piece] = result
        return result

    def _encode_text(self, text: str) -> list[int]:
        text = unicodedata.normalize("NFC", text)
        output: list[int] = []
        for match in self._split_words.finditer(text):
            encoded = "".join(self._byte_encoder[byte] for byte in match.group(0).encode("utf-8"))
            output.extend(self._bpe(encoded))
        return output

    def encode(self, text: str) -> list[int]:
        output: list[int] = []
        position = 0
        for match in self._split_special.finditer(text):
            output.extend(self._encode_text(text[position : match.start()]))
            output.append(self.added_tokens[match.group(0)])
            position = match.end()
        output.extend(self._encode_text(text[position:]))
        return output

    def decode(self, token_ids: list[int]) -> str:
        try:
            return b"".join(self._id_to_bytes[token_id] for token_id in token_ids).decode(
                "utf-8", errors="replace"
            )
        except KeyError as error:
            raise ValueError(f"Qwen response contains an unknown token id: {error}") from error

    def stream_decoder(self) -> Callable[[int | None], str]:
        decoder = codecs.getincrementaldecoder("utf-8")("replace")

        def decode(token_id: int | None = None) -> str:
            if token_id is None:
                return decoder.decode(b"", final=True)
            try:
                return decoder.decode(self._id_to_bytes[token_id])
            except KeyError as error:
                raise ValueError(f"Qwen response contains an unknown token id: {error}") from error

        return decode


def _content(message: dict[str, Any]) -> str:
    content = message.get("content")
    if content is None:
        return ""
    if not isinstance(content, str):
        raise ValueError("local Qwen accepts only string message content")
    return content


def _tool_call_payload(call: dict[str, Any]) -> tuple[str, Any]:
    function = call.get("function", call)
    if not isinstance(function, dict) or not isinstance(function.get("name"), str):
        raise ValueError("Qwen chat history contains a malformed tool call")
    arguments = function.get("arguments", {})
    if isinstance(arguments, str):
        arguments = json.loads(arguments)
    return function["name"], arguments


def render_chat(
    messages: list[dict[str, Any]],
    tools: list[dict[str, Any]],
    enable_thinking: bool = True,
) -> str:
    if not messages:
        raise ValueError("local Qwen request has no messages")
    if any(not isinstance(message, dict) for message in messages):
        raise ValueError("local Qwen messages must be objects")
    output = ""
    first_is_system = messages[0].get("role") == "system"
    if tools:
        output += "<|im_start|>system\n"
        if first_is_system:
            output += _content(messages[0]) + "\n\n"
        output += (
            "# Tools\n\nYou may call one or more functions to assist with the user query.\n\n"
            "You are provided with function signatures within <tools></tools> XML tags:\n<tools>"
        )
        for tool in tools:
            output += "\n" + json.dumps(tool, ensure_ascii=False, separators=(",", ":"))
        output += (
            "\n</tools>\n\nFor each function call, return a json object with function name and "
            "arguments within <tool_call></tool_call> XML tags:\n<tool_call>\n"
            '{"name": <function-name>, "arguments": <args-json-object>}\n'
            "</tool_call><|im_end|>\n"
        )
    elif first_is_system:
        output += "<|im_start|>system\n" + _content(messages[0]) + "<|im_end|>\n"

    last_query_index = len(messages) - 1
    for index in range(len(messages) - 1, -1, -1):
        message = messages[index]
        content = _content(message)
        if (
            message.get("role") == "user"
            and not (content.startswith("<tool_response>") and content.endswith("</tool_response>"))
        ):
            last_query_index = index
            break

    index = 0
    while index < len(messages):
        message = messages[index]
        role, content = message.get("role"), _content(message)
        if role == "system" and index == 0:
            index += 1
            continue
        if role in ("user", "system"):
            output += f"<|im_start|>{role}\n{content}<|im_end|>\n"
        elif role == "assistant":
            reasoning = message.get("reasoning_content")
            if reasoning is not None and not isinstance(reasoning, str):
                raise ValueError("assistant reasoning content must be a string")
            reasoning = reasoning or ""
            if not reasoning and "</think>" in content:
                before, content = content.split("</think>", 1)
                reasoning = before.rsplit("<think>", 1)[-1].lstrip("\n").rstrip("\n")
                content = content.lstrip("\n")
            output += "<|im_start|>assistant\n"
            if index > last_query_index and (index == len(messages) - 1 or reasoning):
                output += f"<think>\n{reasoning.strip(chr(10))}\n</think>\n\n{content.lstrip(chr(10))}"
            else:
                output += content
            for call_index, call in enumerate(message.get("tool_calls") or []):
                name, arguments = _tool_call_payload(call)
                if content or call_index:
                    output += "\n"
                output += (
                    '<tool_call>\n{"name": '
                    + json.dumps(name, ensure_ascii=False)
                    + ', "arguments": '
                    + json.dumps(arguments, ensure_ascii=False, separators=(",", ":"))
                    + "}\n</tool_call>"
                )
            output += "<|im_end|>\n"
        elif role == "tool":
            if index == 0 or messages[index - 1].get("role") != "tool":
                output += "<|im_start|>user"
            output += f"\n<tool_response>\n{content}\n</tool_response>"
            if index + 1 == len(messages) or messages[index + 1].get("role") != "tool":
                output += "<|im_end|>\n"
        else:
            raise ValueError(f"local Qwen does not support message role {role!r}")
        index += 1
    output += "<|im_start|>assistant\n"
    if not enable_thinking:
        output += "<think>\n\n</think>\n\n"
    return output

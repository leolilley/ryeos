"""Qwen3-0.6B binding over the admitted tinygrad realization."""

from __future__ import annotations

import ctypes
import json
import mmap
from pathlib import Path
from typing import Iterator

from tinygrad import Tensor, UOp, dtypes, nn
from tinygrad.llm.model import Transformer, TransformerConfig


MODEL_ID = "qwen3-0.6b"
MAX_CONTEXT = 2048
MAX_OUTPUT_TOKENS = 256

_SAFE_DTYPES = {
    "BOOL": dtypes.bool,
    "I8": dtypes.int8,
    "U8": dtypes.uint8,
    "I16": dtypes.int16,
    "U16": dtypes.uint16,
    "I32": dtypes.int,
    "U32": dtypes.uint,
    "I64": dtypes.int64,
    "U64": dtypes.uint64,
    "F16": dtypes.float16,
    "BF16": dtypes.bfloat16,
    "F32": dtypes.float32,
    "F64": dtypes.float64,
}


class ReadOnlySafeTensors:
    """Map a sealed safetensors file without requiring write access.

    tinygrad's path loader uses its writable DISK device. RyeOS realizations
    are intentionally read-only, so this adapter exposes a private mmap as
    external CPU buffers and keeps that mapping alive for the model lifetime.
    """

    def __init__(self, path: Path):
        source = path.open("rb")
        try:
            self._mapping = mmap.mmap(source.fileno(), 0, access=mmap.ACCESS_COPY)
        finally:
            source.close()
        if len(self._mapping) < 8:
            raise ValueError("safetensors object is truncated")
        header_length = int.from_bytes(self._mapping[:8], "little")
        if header_length == 0 or header_length > 16 * 1024 * 1024:
            raise ValueError("safetensors header is outside the admitted bound")
        data_start = 8 + header_length
        if data_start > len(self._mapping):
            raise ValueError("safetensors header exceeds the object")
        header = json.loads(self._mapping[8:data_start])
        if not isinstance(header, dict):
            raise ValueError("safetensors header is not a mapping")
        base = ctypes.addressof(ctypes.c_ubyte.from_buffer(self._mapping))
        self.tensors: dict[str, Tensor] = {}
        for name, metadata in header.items():
            if name == "__metadata__":
                continue
            if not isinstance(name, str) or not isinstance(metadata, dict):
                raise ValueError("safetensors metadata is malformed")
            shape, offsets, dtype_name = (
                metadata.get("shape"),
                metadata.get("data_offsets"),
                metadata.get("dtype"),
            )
            if (
                not isinstance(shape, list)
                or not shape
                or any(not isinstance(size, int) or size <= 0 for size in shape)
                or not isinstance(offsets, list)
                or len(offsets) != 2
                or any(not isinstance(offset, int) for offset in offsets)
                or dtype_name not in _SAFE_DTYPES
            ):
                raise ValueError(f"safetensors entry {name!r} is malformed")
            begin, end = offsets
            if begin < 0 or end <= begin or data_start + end > len(self._mapping):
                raise ValueError(f"safetensors entry {name!r} is out of bounds")
            dtype = _SAFE_DTYPES[dtype_name]
            elements = 1
            for size in shape:
                elements *= size
            if elements * dtype.itemsize != end - begin:
                raise ValueError(f"safetensors entry {name!r} has an incoherent byte length")
            self.tensors[name] = Tensor.from_blob(
                base + data_start + begin,
                tuple(shape),
                dtype=dtype,
                device="CPU",
            )


def _require_exact_model_config(model_root: Path) -> dict:
    config = json.loads((model_root / "config.json").read_text(encoding="utf-8"))
    expected = {
        "architectures": ["Qwen3ForCausalLM"],
        "attention_bias": False,
        "attention_dropout": 0.0,
        "bos_token_id": 151643,
        "eos_token_id": 151645,
        "head_dim": 128,
        "hidden_act": "silu",
        "hidden_size": 1024,
        "intermediate_size": 3072,
        "max_position_embeddings": 40960,
        "model_type": "qwen3",
        "num_attention_heads": 16,
        "num_hidden_layers": 28,
        "num_key_value_heads": 8,
        "rms_norm_eps": 1e-6,
        "rope_scaling": None,
        "rope_theta": 1_000_000,
        "sliding_window": None,
        "tie_word_embeddings": True,
        "torch_dtype": "bfloat16",
        "use_cache": True,
        "use_sliding_window": False,
        "vocab_size": 151936,
    }
    for key, value in expected.items():
        if config.get(key) != value:
            raise ValueError(f"Qwen model config field {key!r} changed")
    return config


def _map_weight_name(name: str) -> str:
    if name == "model.embed_tokens.weight":
        return "token_embd.weight"
    if name == "model.norm.weight":
        return "output_norm.weight"
    if name == "lm_head.weight":
        return "output.weight"
    prefix = "model.layers."
    if not name.startswith(prefix):
        raise ValueError(f"Qwen weight {name!r} has no admitted mapping")
    remainder = name[len(prefix) :]
    layer_text, separator, suffix = remainder.partition(".")
    if not separator or not layer_text.isdigit() or int(layer_text) >= 28:
        raise ValueError(f"Qwen weight {name!r} has an invalid layer")
    mapped_suffixes = {
        "input_layernorm.weight": "attn_norm.weight",
        "post_attention_layernorm.weight": "ffn_norm.weight",
        "self_attn.q_proj.weight": "attn_q.weight",
        "self_attn.k_proj.weight": "attn_k.weight",
        "self_attn.v_proj.weight": "attn_v.weight",
        "self_attn.o_proj.weight": "attn_output.weight",
        "self_attn.q_norm.weight": "attn_q_norm.weight",
        "self_attn.k_norm.weight": "attn_k_norm.weight",
        "mlp.gate_proj.weight": "ffn_gate.weight",
        "mlp.up_proj.weight": "ffn_up.weight",
        "mlp.down_proj.weight": "ffn_down.weight",
    }
    try:
        mapped = mapped_suffixes[suffix]
    except KeyError as error:
        raise ValueError(f"Qwen weight {name!r} has no admitted mapping") from error
    return f"blk.{int(layer_text)}.{mapped}"


class QwenModel:
    def __init__(self, model_root: Path):
        config = _require_exact_model_config(model_root)
        self._mapped = ReadOnlySafeTensors(model_root / "model.safetensors")
        model_config = TransformerConfig(
            num_blocks=config["num_hidden_layers"],
            dim=config["hidden_size"],
            hidden_dim=config["intermediate_size"],
            n_heads=config["num_attention_heads"],
            n_kv_heads=config["num_key_value_heads"],
            norm_eps=config["rms_norm_eps"],
            vocab_size=config["vocab_size"],
            head_dim=config["head_dim"],
            rope_theta=config["rope_theta"],
            rope_dim=config["head_dim"],
            v_head_dim=config["head_dim"],
            max_context=MAX_CONTEXT,
            qk_norm=config["head_dim"],
        )
        self._model_config = model_config
        self._model = Transformer(model_config)
        state: dict[str, Tensor] = {}
        for source_name, tensor in self._mapped.tensors.items():
            target_name = _map_weight_name(source_name)
            if target_name in state:
                raise ValueError(f"Qwen weights collide at {target_name!r}")
            state[target_name] = tensor
        self._state = state
        nn.state.load_state_dict(self._model, state, strict=True, verbose=False, realize=False)

    def generate(
        self,
        prompt_tokens: list[int],
        output_limit: int,
        temperature: float,
        seed: int,
    ) -> Iterator[int]:
        if not prompt_tokens or len(prompt_tokens) >= MAX_CONTEXT:
            raise ValueError("Qwen prompt is empty or exceeds the admitted context")
        if output_limit <= 0 or output_limit > MAX_OUTPUT_TOKENS:
            raise ValueError("Qwen output limit is outside the admitted bound")
        if len(prompt_tokens) + output_limit > MAX_CONTEXT:
            raise ValueError("Qwen prompt plus output exceeds the admitted context")
        if not 0.0 <= temperature <= 2.0:
            raise ValueError("Qwen temperature is outside the admitted range")
        if seed < 0 or seed > (1 << 63) - 1:
            raise ValueError("Qwen seed is outside the admitted range")
        # Weights and their read-only mmap stay resident, while every call gets
        # a fresh Transformer state container. This creates new KV buffers,
        # prefix metadata, and TinyJit instances without re-reading or copying
        # the 0.6B parameters. No prior request's generation state is reachable
        # by the next call. Mutable compiler caches are disabled by the worker
        # environment rather than being trusted as semantics-free state.
        request_model = Transformer(self._model_config)
        nn.state.load_state_dict(
            request_model,
            self._state,
            strict=True,
            verbose=False,
            realize=False,
        )
        # Transformer construction allocates throwaway initialized parameters.
        # Reset only after those have been replaced so the authored seed governs
        # sampling itself, independent of tinygrad's parameter initialization.
        Tensor.manual_seed(seed)
        # Drive the admitted Transformer execution contract explicitly.  The
        # upstream convenience generator owns a reusable prefix cache and a
        # padded dynamic-slice path; neither is part of this worker's
        # per-request semantics.  Explicit prefill followed by one-token
        # rollouts leaves only this fresh model's KV buffers, TinyJit instances,
        # and freshly seeded sampler mutable.
        next_input = list(prompt_tokens)
        start_pos = 0
        variable_start_pos = UOp.variable("start_pos", 0, MAX_CONTEXT - 1)
        sample_temperature = Tensor([temperature])
        for _ in range(output_limit):
            next_token = int(
                request_model(
                    Tensor([next_input], dtype="int32"),
                    variable_start_pos.bind(start_pos),
                    sample_temperature,
                )
                .realize()
                .item()
            )
            start_pos += len(next_input)
            next_input = [next_token]
            yield next_token

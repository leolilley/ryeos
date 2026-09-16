"""Closed Qwen model and safetensors contracts for local inference.

This module deliberately has no tinygrad import. Its complete artifact
preflight runs before Transformer construction or selected-device contact.
"""

from __future__ import annotations

from dataclasses import dataclass
import hashlib
import json
from pathlib import Path, PurePosixPath
import re
from typing import Any


MAX_SAFETENSORS_HEADER_BYTES = 16 * 1024 * 1024
MAX_MODEL_TENSORS = 512
_SHARD_NAME = re.compile(r"model-[0-9]{5}-of-[0-9]{5}\.safetensors")
_DTYPE_BYTES = {"BF16": 2}


def _strict_object(pairs: list[tuple[str, Any]]) -> dict[str, Any]:
    value: dict[str, Any] = {}
    for key, member in pairs:
        if key in value:
            raise ValueError(f"duplicate JSON object member: {key}")
        value[key] = member
    return value


def _reject_json_constant(value: str) -> None:
    raise ValueError(f"non-finite JSON number is forbidden: {value}")


def strict_json_file(path: Path, *, maximum_bytes: int) -> Any:
    if path.is_symlink() or not path.is_file():
        raise ValueError(f"{path.name} is not an ordinary admitted file")
    size = path.stat().st_size
    if size <= 0 or size > maximum_bytes:
        raise ValueError(f"{path.name} is outside its admitted byte bound")
    encoded = path.read_bytes()
    if len(encoded) != size:
        raise ValueError(f"{path.name} is outside its admitted byte bound")
    try:
        return json.loads(
            encoded,
            object_pairs_hook=_strict_object,
            parse_constant=_reject_json_constant,
        )
    except (UnicodeDecodeError, json.JSONDecodeError) as error:
        raise ValueError(f"{path.name} is not canonical JSON") from error


def canonical_json(value: Any) -> str:
    """Preserve JSON scalar types while comparing exact semantic objects."""
    return json.dumps(
        value,
        ensure_ascii=False,
        sort_keys=True,
        separators=(",", ":"),
        allow_nan=False,
    )


@dataclass(frozen=True)
class TensorContract:
    shape: tuple[int, ...]
    dtype: str = "BF16"

    @property
    def byte_length(self) -> int:
        elements = 1
        for size in self.shape:
            elements *= size
        return elements * _DTYPE_BYTES[self.dtype]


@dataclass(frozen=True)
class QwenModelContract:
    model_id: str
    upstream_repository: str
    upstream_revision: str
    config: dict[str, Any]
    context_ceiling: int
    output_ceiling: int
    tensor_bytes: int
    shard_names: tuple[str, ...]
    has_lm_head: bool
    weight_map_sha256: str | None
    sampler_id: str

    @property
    def tensors(self) -> dict[str, TensorContract]:
        hidden = self.config["hidden_size"]
        intermediate = self.config["intermediate_size"]
        heads = self.config["num_attention_heads"]
        kv_heads = self.config["num_key_value_heads"]
        head_dim = self.config["head_dim"]
        vocab = self.config["vocab_size"]
        tensors = {
            "model.embed_tokens.weight": TensorContract((vocab, hidden)),
            "model.norm.weight": TensorContract((hidden,)),
        }
        if self.has_lm_head:
            tensors["lm_head.weight"] = TensorContract((vocab, hidden))
        for layer in range(self.config["num_hidden_layers"]):
            prefix = f"model.layers.{layer}."
            tensors.update(
                {
                    prefix + "input_layernorm.weight": TensorContract((hidden,)),
                    prefix + "post_attention_layernorm.weight": TensorContract((hidden,)),
                    prefix + "self_attn.q_proj.weight": TensorContract(
                        (heads * head_dim, hidden)
                    ),
                    prefix + "self_attn.k_proj.weight": TensorContract(
                        (kv_heads * head_dim, hidden)
                    ),
                    prefix + "self_attn.v_proj.weight": TensorContract(
                        (kv_heads * head_dim, hidden)
                    ),
                    prefix + "self_attn.o_proj.weight": TensorContract(
                        (hidden, heads * head_dim)
                    ),
                    prefix + "self_attn.q_norm.weight": TensorContract((head_dim,)),
                    prefix + "self_attn.k_norm.weight": TensorContract((head_dim,)),
                    prefix + "mlp.gate_proj.weight": TensorContract(
                        (intermediate, hidden)
                    ),
                    prefix + "mlp.up_proj.weight": TensorContract(
                        (intermediate, hidden)
                    ),
                    prefix + "mlp.down_proj.weight": TensorContract(
                        (hidden, intermediate)
                    ),
                }
            )
        if len(tensors) > MAX_MODEL_TENSORS:
            raise ValueError("Qwen model tensor contract exceeds its admitted bound")
        return tensors


def _config(
    *, hidden_size: int, intermediate_size: int, heads: int, layers: int
) -> dict[str, Any]:
    return {
        "architectures": ["Qwen3ForCausalLM"],
        "attention_bias": False,
        "attention_dropout": 0.0,
        "bos_token_id": 151643,
        "eos_token_id": 151645,
        "head_dim": 128,
        "hidden_act": "silu",
        "hidden_size": hidden_size,
        "initializer_range": 0.02,
        "intermediate_size": intermediate_size,
        "max_position_embeddings": 40960,
        "max_window_layers": layers,
        "model_type": "qwen3",
        "num_attention_heads": heads,
        "num_hidden_layers": layers,
        "num_key_value_heads": 8,
        "rms_norm_eps": 1e-6,
        "rope_scaling": None,
        "rope_theta": 1_000_000,
        "sliding_window": None,
        "tie_word_embeddings": True,
        "torch_dtype": "bfloat16",
        "transformers_version": "4.51.0",
        "use_cache": True,
        "use_sliding_window": False,
        "vocab_size": 151936,
    }


QWEN3_0_6B = QwenModelContract(
    model_id="qwen3-0.6b",
    upstream_repository="Qwen/Qwen3-0.6B",
    upstream_revision="c1899de289a04d12100db370d81485cdf75e47ca",
    config=_config(hidden_size=1024, intermediate_size=3072, heads=16, layers=28),
    context_ceiling=2048,
    output_ceiling=256,
    tensor_bytes=1_503_264_768,
    shard_names=("model.safetensors",),
    has_lm_head=True,
    weight_map_sha256=None,
    sampler_id="tinygrad-full-vocabulary-gumbel-max-v1",
)

QWEN3_4B = QwenModelContract(
    model_id="qwen3-4b",
    upstream_repository="Qwen/Qwen3-4B",
    upstream_revision="3101254bbe4169895668a0e7653c3fd1f313576e",
    config=_config(hidden_size=2560, intermediate_size=9728, heads=32, layers=36),
    context_ceiling=32768,
    output_ceiling=2048,
    tensor_bytes=8_044_936_192,
    shard_names=tuple(f"model-{index:05}-of-00003.safetensors" for index in range(1, 4)),
    has_lm_head=False,
    weight_map_sha256="9cb9c16d3bc213510f48d3aca6726c9ef875b0ed5c7e2a952a4a4f61c30e0cc8",
    sampler_id="tinygrad-full-vocabulary-gumbel-max-v1",
)

MODEL_CONTRACTS = {contract.model_id: contract for contract in (QWEN3_0_6B, QWEN3_4B)}


def identify_model_contract(model_root: Path) -> QwenModelContract:
    config = strict_json_file(model_root / "config.json", maximum_bytes=64 * 1024)
    if not isinstance(config, dict):
        raise ValueError("Qwen model configuration is not an object")
    encoded = canonical_json(config)
    for contract in MODEL_CONTRACTS.values():
        expected = canonical_json(contract.config)
        if encoded == expected:
            return contract
    raise ValueError("Qwen model configuration is not an admitted exact contract")


def _canonical_shard_name(value: Any) -> str:
    if not isinstance(value, str) or not _SHARD_NAME.fullmatch(value):
        raise ValueError("safetensors index contains a non-canonical shard name")
    path = PurePosixPath(value)
    if path.name != value or path.is_absolute() or ".." in path.parts:
        raise ValueError("safetensors index shard escaped the model root")
    return value


def load_weight_map(
    model_root: Path, contract: QwenModelContract
) -> dict[str, str]:
    expected_tensors = contract.tensors
    if contract.shard_names == ("model.safetensors",):
        if (model_root / "model.safetensors.index.json").exists():
            raise ValueError("single-file Qwen model has an unexpected shard index")
        return {name: "model.safetensors" for name in expected_tensors}

    index = strict_json_file(
        model_root / "model.safetensors.index.json", maximum_bytes=2 * 1024 * 1024
    )
    if not isinstance(index, dict) or set(index) != {"metadata", "weight_map"}:
        raise ValueError("safetensors index shape is not canonical")
    metadata = index["metadata"]
    if (
        not isinstance(metadata, dict)
        or set(metadata) != {"total_size"}
        or type(metadata["total_size"]) is not int
        or metadata["total_size"] != contract.tensor_bytes
    ):
        raise ValueError("safetensors index total size changed")
    weight_map = index["weight_map"]
    if not isinstance(weight_map, dict) or set(weight_map) != set(expected_tensors):
        raise ValueError("safetensors index tensor set changed")
    canonical = {
        name: _canonical_shard_name(shard) for name, shard in weight_map.items()
    }
    if set(canonical.values()) != set(contract.shard_names):
        raise ValueError("safetensors index shard set changed")
    canonical_digest = hashlib.sha256(
        canonical_json(canonical).encode("utf-8")
    ).hexdigest()
    if canonical_digest != contract.weight_map_sha256:
        raise ValueError("safetensors index tensor-to-shard assignment changed")
    return canonical


def validate_safetensors_header(
    *,
    shard_name: str,
    header: Any,
    data_bytes: int,
    expected: dict[str, TensorContract],
) -> dict[str, tuple[tuple[int, ...], str, int, int]]:
    if not isinstance(header, dict) or len(header) > len(expected) + 1:
        raise ValueError(f"{shard_name} safetensors header is outside its bound")
    metadata = header.get("__metadata__")
    if metadata is not None and (
        not isinstance(metadata, dict)
        or any(not isinstance(key, str) or not isinstance(value, str) for key, value in metadata.items())
    ):
        raise ValueError(f"{shard_name} safetensors metadata is malformed")
    entries = {name: value for name, value in header.items() if name != "__metadata__"}
    if set(entries) != set(expected):
        raise ValueError(f"{shard_name} safetensors tensor set contradicts its index")

    validated: dict[str, tuple[tuple[int, ...], str, int, int]] = {}
    ranges: list[tuple[int, int, str]] = []
    for name, tensor in entries.items():
        if not isinstance(tensor, dict) or set(tensor) != {"dtype", "shape", "data_offsets"}:
            raise ValueError(f"safetensors entry {name!r} is malformed")
        dtype, shape, offsets = tensor["dtype"], tensor["shape"], tensor["data_offsets"]
        wanted = expected[name]
        if (
            not isinstance(dtype, str)
            or not isinstance(shape, list)
            or any(
                not isinstance(size, int) or isinstance(size, bool) or size <= 0
                for size in shape
            )
            or dtype != wanted.dtype
            or shape != list(wanted.shape)
        ):
            raise ValueError(f"safetensors entry {name!r} changed shape or dtype")
        if (
            not isinstance(offsets, list)
            or len(offsets) != 2
            or any(not isinstance(offset, int) or isinstance(offset, bool) for offset in offsets)
        ):
            raise ValueError(f"safetensors entry {name!r} has malformed offsets")
        begin, end = offsets
        if begin < 0 or end <= begin or end > data_bytes or end - begin != wanted.byte_length:
            raise ValueError(f"safetensors entry {name!r} has incoherent bounds")
        ranges.append((begin, end, name))
        validated[name] = (wanted.shape, wanted.dtype, begin, end)

    ranges.sort()
    cursor = 0
    for begin, end, name in ranges:
        if begin != cursor:
            reason = "overlaps" if begin < cursor else "contains unindexed bytes"
            raise ValueError(f"{shard_name} {reason} before tensor {name!r}")
        cursor = end
    if cursor != data_bytes:
        raise ValueError(f"{shard_name} contains trailing unindexed bytes")
    return validated

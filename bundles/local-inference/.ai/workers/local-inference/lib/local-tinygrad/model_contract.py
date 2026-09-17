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
    chat_template_sha256: str
    generation_config: dict[str, Any]

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


MODEL_PROFILE_SCHEMA = "ryeos.local_inference.qwen_profile.v1"
MAX_MODEL_PROFILES = 8
_MODEL_ID = re.compile(r"[a-z0-9][a-z0-9.-]{0,63}")
_REPOSITORY = re.compile(
    r"[A-Za-z0-9](?:[A-Za-z0-9._-]{0,62}[A-Za-z0-9])?/"
    r"[A-Za-z0-9](?:[A-Za-z0-9._-]{0,62}[A-Za-z0-9])?"
)
_HEX_40 = re.compile(r"[0-9a-f]{40}")
_HEX_64 = re.compile(r"[0-9a-f]{64}")
_CONFIG_KEYS = {
    "architectures",
    "attention_bias",
    "attention_dropout",
    "bos_token_id",
    "eos_token_id",
    "head_dim",
    "hidden_act",
    "hidden_size",
    "initializer_range",
    "intermediate_size",
    "max_position_embeddings",
    "max_window_layers",
    "model_type",
    "num_attention_heads",
    "num_hidden_layers",
    "num_key_value_heads",
    "rms_norm_eps",
    "rope_scaling",
    "rope_theta",
    "sliding_window",
    "tie_word_embeddings",
    "torch_dtype",
    "transformers_version",
    "use_cache",
    "use_sliding_window",
    "vocab_size",
}


def _positive_int(value: Any, field: str, maximum: int) -> int:
    if type(value) is not int or not 0 < value <= maximum:
        raise ValueError(f"Qwen profile {field} is outside its admitted bound")
    return value


def _validate_config(value: Any) -> dict[str, Any]:
    if not isinstance(value, dict) or set(value) != _CONFIG_KEYS:
        raise ValueError("Qwen profile configuration shape is not canonical")
    if value["architectures"] != ["Qwen3ForCausalLM"]:
        raise ValueError("Qwen profile architecture is unsupported")
    exact_values = {
        "attention_bias": False,
        "bos_token_id": 151643,
        "eos_token_id": 151645,
        "head_dim": 128,
        "hidden_act": "silu",
        "max_position_embeddings": 40960,
        "model_type": "qwen3",
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
    for field, expected in exact_values.items():
        if type(value[field]) is not type(expected) or value[field] != expected:
            raise ValueError(f"Qwen profile configuration field {field!r} changed")
    for field, maximum in {
        "hidden_size": 32768,
        "intermediate_size": 131072,
        "max_window_layers": 256,
        "num_attention_heads": 256,
        "num_hidden_layers": 256,
        "num_key_value_heads": 256,
    }.items():
        _positive_int(value[field], field, maximum)
    for field, expected in {"attention_dropout": 0.0, "initializer_range": 0.02}.items():
        if type(value[field]) is not float or value[field] != expected:
            raise ValueError(f"Qwen profile configuration field {field!r} changed")
    if value["num_attention_heads"] % value["num_key_value_heads"] != 0:
        raise ValueError("Qwen profile attention grouping is incoherent")
    if value["max_window_layers"] != value["num_hidden_layers"]:
        raise ValueError("Qwen profile window layer count is incoherent")
    return value


def load_model_profile(path: Path) -> QwenModelContract:
    value = strict_json_file(path, maximum_bytes=128 * 1024)
    expected_keys = {
        "schema",
        "model_id",
        "upstream_repository",
        "upstream_revision",
        "config",
        "context_ceiling",
        "output_ceiling",
        "tensor_bytes",
        "shard_names",
        "has_lm_head",
        "weight_map_sha256",
        "sampler_id",
        "chat_template_sha256",
        "generation_config",
    }
    if not isinstance(value, dict) or set(value) != expected_keys:
        raise ValueError("Qwen profile shape is not canonical")
    model_id = value["model_id"]
    if (
        value["schema"] != MODEL_PROFILE_SCHEMA
        or not isinstance(model_id, str)
        or not _MODEL_ID.fullmatch(model_id)
        or path.name != f"{model_id}.json"
    ):
        raise ValueError("Qwen profile identity is not canonical")
    if not isinstance(value["upstream_repository"], str) or not _REPOSITORY.fullmatch(
        value["upstream_repository"]
    ):
        raise ValueError("Qwen profile repository is not canonical")
    if not isinstance(value["upstream_revision"], str) or not _HEX_40.fullmatch(
        value["upstream_revision"]
    ):
        raise ValueError("Qwen profile revision is not canonical")
    config = _validate_config(value["config"])
    context_ceiling = _positive_int(value["context_ceiling"], "context_ceiling", 1_000_000)
    output_ceiling = _positive_int(value["output_ceiling"], "output_ceiling", context_ceiling)
    if context_ceiling > config["max_position_embeddings"]:
        raise ValueError("Qwen profile context exceeds the model configuration")
    tensor_bytes = _positive_int(value["tensor_bytes"], "tensor_bytes", 1 << 50)
    shard_names = value["shard_names"]
    if (
        not isinstance(shard_names, list)
        or not 1 <= len(shard_names) <= 32
    ):
        raise ValueError("Qwen profile shard set is not canonical")
    if type(value["has_lm_head"]) is not bool:
        raise ValueError("Qwen profile lm-head declaration is malformed")
    weight_map_sha256 = value["weight_map_sha256"]
    if weight_map_sha256 is not None and (
        not isinstance(weight_map_sha256, str) or not _HEX_64.fullmatch(weight_map_sha256)
    ):
        raise ValueError("Qwen profile weight-map digest is malformed")
    single_file = shard_names == ["model.safetensors"]
    expected_shards = [
        f"model-{index:05}-of-{len(shard_names):05}.safetensors"
        for index in range(1, len(shard_names) + 1)
    ]
    if not single_file and shard_names != expected_shards:
        raise ValueError("Qwen profile shard set is not canonical")
    if single_file != (weight_map_sha256 is None):
        raise ValueError("Qwen profile shard index contract is incoherent")
    if value["sampler_id"] != "tinygrad-full-vocabulary-gumbel-max-v1":
        raise ValueError("Qwen profile sampler is unsupported")
    if not isinstance(value["chat_template_sha256"], str) or not _HEX_64.fullmatch(
        value["chat_template_sha256"]
    ):
        raise ValueError("Qwen profile chat-template digest is malformed")
    generation_config = value["generation_config"]
    if not isinstance(generation_config, dict) or set(generation_config) != {
        "bos_token_id",
        "do_sample",
        "eos_token_id",
        "pad_token_id",
        "temperature",
        "top_k",
        "top_p",
        "transformers_version",
    }:
        raise ValueError("Qwen profile generation configuration is malformed")
    token_ids = generation_config["eos_token_id"]
    if (
        type(generation_config["bos_token_id"]) is not int
        or generation_config["bos_token_id"] != config["bos_token_id"]
        or type(generation_config["do_sample"]) is not bool
        or not isinstance(token_ids, list)
        or not token_ids
        or any(type(token_id) is not int or not 0 <= token_id < config["vocab_size"] for token_id in token_ids)
        or len(set(token_ids)) != len(token_ids)
        or config["eos_token_id"] not in token_ids
        or type(generation_config["pad_token_id"]) is not int
        or not 0 <= generation_config["pad_token_id"] < config["vocab_size"]
        or type(generation_config["temperature"]) is not float
        or not 0.0 < generation_config["temperature"] <= 2.0
        or type(generation_config["top_k"]) is not int
        or not 0 < generation_config["top_k"] <= config["vocab_size"]
        or type(generation_config["top_p"]) is not float
        or not 0.0 < generation_config["top_p"] <= 1.0
        or generation_config["transformers_version"] != config["transformers_version"]
    ):
        raise ValueError("Qwen profile generation configuration is malformed")
    contract = QwenModelContract(
        model_id=model_id,
        upstream_repository=value["upstream_repository"],
        upstream_revision=value["upstream_revision"],
        config=config,
        context_ceiling=context_ceiling,
        output_ceiling=output_ceiling,
        tensor_bytes=tensor_bytes,
        shard_names=tuple(shard_names),
        has_lm_head=value["has_lm_head"],
        weight_map_sha256=weight_map_sha256,
        sampler_id=value["sampler_id"],
        chat_template_sha256=value["chat_template_sha256"],
        generation_config=generation_config,
    )
    if sum(tensor.byte_length for tensor in contract.tensors.values()) != tensor_bytes:
        raise ValueError("Qwen profile tensor total is incoherent")
    return contract


def load_model_profiles(
    root: Path | None = None,
) -> dict[str, QwenModelContract]:
    profile_root = root or Path(__file__).resolve(strict=True).parent / "model-profiles"
    if profile_root.is_symlink() or not profile_root.is_dir():
        raise ValueError("Qwen profile root is not an admitted directory")
    entries = sorted(profile_root.iterdir(), key=lambda path: path.name)
    if not 1 <= len(entries) <= MAX_MODEL_PROFILES:
        raise ValueError("Qwen profile set is outside its admitted bound")
    contracts: dict[str, QwenModelContract] = {}
    for path in entries:
        contract = load_model_profile(path)
        if contract.model_id in contracts:
            raise ValueError("Qwen profile set repeats a model identity")
        contracts[contract.model_id] = contract
    return contracts


def load_named_model_profile(model_id: str) -> QwenModelContract:
    if not isinstance(model_id, str) or not _MODEL_ID.fullmatch(model_id):
        raise ValueError("Qwen profile identity is not canonical")
    root = Path(__file__).resolve(strict=True).parent / "model-profiles"
    return load_model_profile(root / f"{model_id}.json")


def identify_model_contract(
    model_root: Path, contract: QwenModelContract
) -> QwenModelContract:
    config = strict_json_file(model_root / "config.json", maximum_bytes=64 * 1024)
    if not isinstance(config, dict):
        raise ValueError("Qwen model configuration is not an object")
    if canonical_json(config) != canonical_json(contract.config):
        raise ValueError("Qwen model configuration is not the selected exact contract")
    return contract


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

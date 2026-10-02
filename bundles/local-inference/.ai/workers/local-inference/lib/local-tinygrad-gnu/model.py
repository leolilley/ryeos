"""Exact Qwen3 bindings over an admitted tinygrad realization."""

from __future__ import annotations

import ctypes
import json
import mmap
from pathlib import Path
from typing import Iterator

from tinygrad import Device, Tensor, UOp, dtypes, nn
from tinygrad.llm.model import Transformer, TransformerConfig

from model_contract import (
    MAX_SAFETENSORS_HEADER_BYTES,
    QwenModelContract,
    identify_model_contract,
    load_named_model_profile,
    load_weight_map,
    validate_safetensors_header,
)


_SAFE_DTYPES = {"BF16": dtypes.bfloat16}


def _strict_object(pairs: list[tuple[str, object]]) -> dict[str, object]:
    value: dict[str, object] = {}
    for key, member in pairs:
        if key in value:
            raise ValueError(f"duplicate JSON object member: {key}")
        value[key] = member
    return value


def _reject_json_constant(value: str) -> None:
    raise ValueError(f"non-finite JSON number is forbidden: {value}")


class ReadOnlySafeTensors:
    """Map one already-indexed safetensors shard without write authority."""

    def __init__(self, path: Path, expected: dict):
        if path.is_symlink() or not path.is_file():
            raise ValueError(f"{path.name} is not an ordinary admitted file")
        source = path.open("rb")
        try:
            self._mapping = mmap.mmap(source.fileno(), 0, access=mmap.ACCESS_COPY)
        finally:
            source.close()
        if len(self._mapping) < 8:
            raise ValueError(f"{path.name} safetensors object is truncated")
        header_length = int.from_bytes(self._mapping[:8], "little")
        if header_length == 0 or header_length > MAX_SAFETENSORS_HEADER_BYTES:
            raise ValueError(f"{path.name} safetensors header is outside the admitted bound")
        data_start = 8 + header_length
        if data_start > len(self._mapping):
            raise ValueError(f"{path.name} safetensors header exceeds the object")
        try:
            header = json.loads(
                self._mapping[8:data_start],
                object_pairs_hook=_strict_object,
                parse_constant=_reject_json_constant,
            )
        except (UnicodeDecodeError, json.JSONDecodeError) as error:
            raise ValueError(f"{path.name} safetensors header is not canonical JSON") from error
        validated = validate_safetensors_header(
            shard_name=path.name,
            header=header,
            data_bytes=len(self._mapping) - data_start,
            expected=expected,
        )
        self._data_start = data_start
        self._validated = validated
        self.tensors: dict[str, Tensor] = {}

    def materialize(self) -> None:
        if self.tensors:
            raise RuntimeError("safetensors shard was materialized more than once")
        base = ctypes.addressof(ctypes.c_ubyte.from_buffer(self._mapping))
        for name, (shape, dtype_name, begin, _end) in self._validated.items():
            self.tensors[name] = Tensor.from_blob(
                base + self._data_start + begin,
                shape,
                dtype=_SAFE_DTYPES[dtype_name],
                device="CPU",
            )


class ReadOnlyQwenWeights:
    """Validate and retain every exact shard mapping for one model lifetime."""

    def __init__(self, model_root: Path, contract: QwenModelContract):
        weight_map = load_weight_map(model_root, contract)
        _require_exact_shard_files(model_root, contract.shard_names)
        expected = contract.tensors
        self._shards = _validate_then_materialize_shards(
            model_root, contract, weight_map, ReadOnlySafeTensors
        )
        self.tensors: dict[str, Tensor] = {}
        for shard in self._shards:
            for name, tensor in shard.tensors.items():
                if name in self.tensors:
                    raise ValueError(f"Qwen tensor {name!r} occurs in more than one shard")
                self.tensors[name] = tensor
        if set(self.tensors) != set(expected):
            raise ValueError("Qwen mapped tensor set is incomplete")
        if sum(tensor.byte_length for tensor in expected.values()) != contract.tensor_bytes:
            raise ValueError("Qwen tensor contract contradicts its exact total size")


def _require_exact_shard_files(model_root: Path, shard_names: tuple[str, ...]) -> None:
    paths = list(model_root.glob("*.safetensors"))
    if {path.name for path in paths} != set(shard_names):
        raise ValueError("Qwen model safetensors shard set changed")
    if any(path.is_symlink() or not path.is_file() for path in paths):
        raise ValueError("Qwen model shard is not an ordinary admitted file")


def _validate_then_materialize_shards(
    model_root: Path,
    contract: QwenModelContract,
    weight_map: dict[str, str],
    shard_factory: object,
) -> list[ReadOnlySafeTensors]:
    """Keep the pure-validation pass ahead of all tensor construction."""
    expected = contract.tensors
    shards: list[ReadOnlySafeTensors] = []
    for shard_name in contract.shard_names:
        names = {name for name, selected in weight_map.items() if selected == shard_name}
        if not names:
            raise ValueError(f"Qwen model shard {shard_name!r} has no indexed tensors")
        shards.append(
            shard_factory(
                model_root / shard_name,
                {name: expected[name] for name in names},
            )
        )
    for shard in shards:
        shard.materialize()
    return shards


def _map_weight_name(name: str, contract: QwenModelContract) -> str:
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
    if (
        not separator
        or not layer_text.isdigit()
        or int(layer_text) >= contract.config["num_hidden_layers"]
    ):
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
    def __init__(self, model_root: Path, profile_id: str):
        self.contract = identify_model_contract(
            model_root, load_named_model_profile(profile_id)
        )
        # Weight preflight finishes before Transformer construction can
        # initialize the selected tinygrad backend or contact that device.
        self._mapped = ReadOnlyQwenWeights(model_root, self.contract)
        config = self.contract.config
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
            max_context=self.contract.context_ceiling,
            qk_norm=config["head_dim"],
        )
        self._model_config = model_config
        self._model = Transformer(model_config)
        state: dict[str, Tensor] = {}
        for source_name, tensor in self._mapped.tensors.items():
            target_name = _map_weight_name(source_name, self.contract)
            if target_name in state:
                raise ValueError(f"Qwen weights collide at {target_name!r}")
            state[target_name] = tensor.to(Device.DEFAULT)
        if not self.contract.has_lm_head:
            state["output.weight"] = state["token_embd.weight"]
        self._state = state
        nn.state.load_state_dict(self._model, state, strict=True, verbose=False, realize=False)

    def _fresh_model(self) -> Transformer:
        model = Transformer(self._model_config)
        nn.state.load_state_dict(
            model, self._state, strict=True, verbose=False, realize=False
        )
        return model

    @property
    def model_id(self) -> str:
        return self.contract.model_id

    @property
    def context_ceiling(self) -> int:
        return self.contract.context_ceiling

    @property
    def output_ceiling(self) -> int:
        return self.contract.output_ceiling

    def full_prefix_logits(self, prompt_tokens: list[int]) -> Tensor:
        """Project exact next-token logits without cache or sampling.

        This is the numeric-conformance surface for an admitted model. It uses a
        fresh instance of the same pinned tinygrad Transformer and the same
        admitted state as generation, then performs the upstream forward path
        through the output projection. Keeping this projection here avoids an
        authoring-time model fork while making no activation or qualification
        decision itself.
        """
        if not prompt_tokens or len(prompt_tokens) >= self.context_ceiling:
            raise ValueError("Qwen prompt is empty or exceeds the admitted context")
        model = self._fresh_model()
        tokens = Tensor([prompt_tokens], dtype="int32")
        hidden = model.token_embd(tokens).float()
        for block in model.blk:
            hidden = block(hidden, 0)
        return model.output(model.output_norm(hidden))[:, -1, :].realize()

    def generate(
        self,
        prompt_tokens: list[int],
        output_limit: int,
        temperature: float,
        seed: int,
    ) -> Iterator[int]:
        if not prompt_tokens or len(prompt_tokens) >= self.context_ceiling:
            raise ValueError("Qwen prompt is empty or exceeds the admitted context")
        if output_limit <= 0 or output_limit > self.output_ceiling:
            raise ValueError("Qwen output limit is outside the admitted bound")
        if len(prompt_tokens) + output_limit > self.context_ceiling:
            raise ValueError("Qwen prompt plus output exceeds the admitted context")
        if not 0.0 <= temperature <= 2.0:
            raise ValueError("Qwen temperature is outside the admitted range")
        if seed < 0 or seed > (1 << 63) - 1:
            raise ValueError("Qwen seed is outside the admitted range")
        request_model = self._fresh_model()
        Tensor.manual_seed(seed)
        next_input = list(prompt_tokens)
        start_pos = 0
        variable_start_pos = UOp.variable("start_pos", 0, self.context_ceiling - 1)
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

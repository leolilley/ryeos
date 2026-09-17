#!/usr/bin/env python3
"""Focused tests for exact local-inference model source authoring."""

from __future__ import annotations

import copy
import contextlib
import hashlib
import io
import importlib.util
import json
from pathlib import Path
import stat
import sys
import tempfile
import unittest
from unittest import mock


sys.dont_write_bytecode = True

HERE = Path(__file__).resolve().parent
BUNDLE = HERE.parent.parent
SCRIPT = BUNDLE / "authoring/author_model.py"
CONTRACT = BUNDLE / "authoring/contracts/qwen3-4b-bf16-model-source-v1.json"
SPEC = importlib.util.spec_from_file_location("local_inference_model_author", SCRIPT)
assert SPEC is not None and SPEC.loader is not None
AUTHOR = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(AUTHOR)
WORKER_SOURCE = BUNDLE / ".ai/workers/local-inference/lib/local-tinygrad"
sys.path.insert(0, str(WORKER_SOURCE))
from model_contract import load_model_profiles  # noqa: E402


QWEN3_4B = load_model_profiles()["qwen3-4b"]


class ModelSourceAuthoringTests(unittest.TestCase):
    def test_reviewed_qwen3_4b_contract_is_exact(self) -> None:
        contract, encoded = AUTHOR.load_contract(CONTRACT)
        self.assertEqual(contract["repository"], "Qwen/Qwen3-4B")
        self.assertEqual(
            contract["revision"], "3101254bbe4169895668a0e7653c3fd1f313576e"
        )
        self.assertEqual(contract["license"]["spdx"], "Apache-2.0")
        self.assertEqual(contract["tensor_payload_bytes"], 8_044_936_192)
        self.assertEqual(contract["selected_file_bytes"], 8_056_463_813)
        self.assertEqual(
            [item["path"] for item in contract["files"] if item["role"] == "weight_shard"],
            [
                "model-00001-of-00003.safetensors",
                "model-00002-of-00003.safetensors",
                "model-00003-of-00003.safetensors",
            ],
        )
        self.assertEqual(json.loads(encoded), contract)
        self.assertEqual(contract["repository"], QWEN3_4B.upstream_repository)
        self.assertEqual(contract["revision"], QWEN3_4B.upstream_revision)
        self.assertEqual(contract["tensor_payload_bytes"], QWEN3_4B.tensor_bytes)
        self.assertEqual(
            tuple(
                item["path"]
                for item in contract["files"]
                if item["role"] == "weight_shard"
            ),
            QWEN3_4B.shard_names,
        )

    def test_contract_rejects_type_confusion_and_incoherent_totals(self) -> None:
        original = json.loads(CONTRACT.read_text(encoding="utf-8"))
        cases = []
        wrong_total = copy.deepcopy(original)
        wrong_total["selected_file_bytes"] += 1
        cases.append((wrong_total, "selected_file_bytes"))
        bool_size = copy.deepcopy(original)
        bool_size["files"][0]["bytes"] = True
        cases.append((bool_size, "byte count"))
        escaped = copy.deepcopy(original)
        escaped["files"][0]["path"] = "../config.json"
        cases.append((escaped, "canonical root file"))
        escaped_repository = copy.deepcopy(original)
        escaped_repository["repository"] = "../Model"
        escaped_repository["source"] = "https://huggingface.co/../Model"
        cases.append((escaped_repository, "repository is not canonical"))
        duplicate = copy.deepcopy(original)
        duplicate["files"][1]["path"] = duplicate["files"][0]["path"]
        cases.append((duplicate, "duplicate model source path"))
        impossible_tensors = copy.deepcopy(original)
        impossible_tensors["tensor_payload_bytes"] = sum(
            item["bytes"]
            for item in impossible_tensors["files"]
            if item["role"] == "weight_shard"
        )
        cases.append((impossible_tensors, "tensor payload"))
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            for index, (value, reason) in enumerate(cases):
                path = root / f"contract-{index}.json"
                path.write_text(json.dumps(value), encoding="utf-8")
                with self.subTest(reason=reason):
                    with self.assertRaisesRegex(ValueError, reason):
                        AUTHOR.load_contract(path)

    def test_offline_source_authoring_is_atomic_and_exact(self) -> None:
        files = [
            ("config.json", "model_config", b"{}"),
            ("generation_config.json", "generation_config", b"{}\n"),
            ("model.safetensors.index.json", "weight_index", b'{"weight_map":{}}'),
            ("model-00001-of-00001.safetensors", "weight_shard", b"weights"),
            ("tokenizer.json", "tokenizer", b'{"model":{}}'),
            ("tokenizer_config.json", "tokenizer_config", b'{"chat_template":"x"}'),
            ("README.md", "license_evidence", b"license: apache-2.0\n"),
        ]
        contract = {
            "schema": AUTHOR.SCHEMA,
            "model_id": "fixture",
            "repository": "Owner/Model",
            "revision": "a" * 40,
            "source": "https://huggingface.co/Owner/Model",
            "license": {"spdx": "Apache-2.0", "evidence_path": "README.md"},
            "tensor_payload_bytes": len(b"weights") - 1,
            "selected_file_bytes": sum(len(content) for _, _, content in files),
            "files": [
                {
                    "path": name,
                    "role": role,
                    "bytes": len(content),
                    "sha256": hashlib.sha256(content).hexdigest(),
                }
                for name, role, content in files
            ],
        }
        encoded = (json.dumps(contract, indent=2) + "\n").encode()
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            source = root / "source"
            source.mkdir()
            for name, _, content in files:
                (source / name).write_bytes(content)
            output = root / "output"
            cache = root / "cache"
            AUTHOR.author_tree(
                output=output,
                cache=None,
                source=source,
                offline=False,
                contract=contract,
                contract_bytes=encoded,
            )
            AUTHOR.verify_tree(output, contract, encoded)
            self.assertEqual((output / AUTHOR.CONTRACT_NAME).read_bytes(), encoded)
            self.assertEqual(stat.S_IMODE(output.stat().st_mode), 0o755)
            self.assertTrue(
                all(
                    stat.S_IMODE((output / name).stat().st_mode) == 0o644
                    for name, _, _ in files
                )
            )
            contract_path = root / "contract.json"
            contract_path.write_bytes(encoded)
            cli_output = root / "cli-output"
            with mock.patch.object(
                sys,
                "argv",
                [
                    str(SCRIPT),
                    "--contract",
                    str(contract_path),
                    "--source",
                    str(source),
                    "--output",
                    str(cli_output),
                ],
            ):
                self.assertEqual(AUTHOR.main(), 0)
            AUTHOR.verify_tree(cli_output, contract, encoded)
            self.assertFalse(cache.exists())
            (output / "config.json").chmod(0o755)
            with self.assertRaisesRegex(ValueError, "mode differs"):
                AUTHOR.verify_tree(output, contract, encoded)
            (output / "config.json").chmod(0o644)
            with self.assertRaisesRegex(ValueError, "refusing existing output"):
                AUTHOR.author_tree(
                    output=output,
                    cache=cache,
                    source=source,
                    offline=False,
                    contract=contract,
                    contract_bytes=encoded,
                )
            (output / "config.json").write_bytes(b"moved")
            with self.assertRaisesRegex(ValueError, "byte count differs"):
                AUTHOR.verify_tree(output, contract, encoded)

    def test_failed_payload_verification_never_publishes_completion_marker(self) -> None:
        files = [
            ("config.json", "model_config", b"{}"),
            ("generation_config.json", "generation_config", b"{}\n"),
            ("model.safetensors.index.json", "weight_index", b'{"weight_map":{}}'),
            ("model.safetensors", "weight_shard", b"weights"),
            ("tokenizer.json", "tokenizer", b'{"model":{}}'),
            ("tokenizer_config.json", "tokenizer_config", b'{"chat_template":"x"}'),
            ("README.md", "license_evidence", b"license: apache-2.0\n"),
        ]
        contract = {
            "files": [
                {
                    "path": name,
                    "role": role,
                    "bytes": len(content),
                    "sha256": hashlib.sha256(content).hexdigest(),
                }
                for name, role, content in files
            ]
        }
        encoded = b"reviewed contract\n"
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            source = root / "source"
            source.mkdir()
            for name, _, content in files:
                (source / name).write_bytes(content)
            output = root / "output"
            with mock.patch.object(
                AUTHOR,
                "verify_payload_tree",
                side_effect=ValueError("injected post-copy refusal"),
            ), self.assertRaisesRegex(ValueError, "injected post-copy refusal"):
                AUTHOR.author_tree(
                    output=output,
                    cache=None,
                    source=source,
                    offline=True,
                    contract=contract,
                    contract_bytes=encoded,
                )
            self.assertEqual(stat.S_IMODE(output.stat().st_mode), 0o700)
            self.assertFalse((output / AUTHOR.CONTRACT_NAME).exists())

    def test_offline_cache_refuses_absence_and_wrong_bytes(self) -> None:
        contract = {
            "source": "https://huggingface.co/Owner/Model",
            "revision": "a" * 40,
        }
        item = {
            "path": "config.json",
            "bytes": 2,
            "sha256": hashlib.sha256(b"{}").hexdigest(),
        }
        with tempfile.TemporaryDirectory() as directory:
            cache = Path(directory)
            with self.assertRaisesRegex(ValueError, "offline model authoring is missing"):
                AUTHOR.obtain(cache, contract, item, True)
            cached = cache / f"{item['sha256']}-{item['path']}"
            cached.write_bytes(b"xx")
            with self.assertRaisesRegex(ValueError, "digest differs"):
                AUTHOR.obtain(cache, contract, item, True)

    def test_online_download_is_bounded_cleans_up_and_reuses_cache(self) -> None:
        exact = b"{}"
        contract = {
            "source": "https://huggingface.co/Owner/Model",
            "revision": "a" * 40,
        }
        item = {
            "path": "config.json",
            "bytes": len(exact),
            "sha256": hashlib.sha256(exact).hexdigest(),
        }
        with tempfile.TemporaryDirectory() as directory:
            cache = Path(directory)
            with mock.patch.object(
                AUTHOR.urllib.request, "urlopen", return_value=io.BytesIO(exact)
            ) as fetch:
                cached = AUTHOR.obtain(cache, contract, item, False)
            self.assertEqual(cached.read_bytes(), exact)
            fetch.assert_called_once()
            self.assertEqual(
                fetch.call_args.kwargs["timeout"],
                AUTHOR.NETWORK_INACTIVITY_TIMEOUT_SECONDS,
            )
            with mock.patch.object(
                AUTHOR.urllib.request,
                "urlopen",
                side_effect=AssertionError("cache reuse contacted the network"),
            ):
                self.assertEqual(AUTHOR.obtain(cache, contract, item, False), cached)

        cases = {
            "size differs": b"{",
            "oversized": b"{}x",
            "digest differs": b"[]",
        }
        for reason, response in cases.items():
            with self.subTest(reason=reason), tempfile.TemporaryDirectory() as directory:
                cache = Path(directory)
                expected = "size differs" if reason == "oversized" else reason
                with mock.patch.object(
                    AUTHOR.urllib.request,
                    "urlopen",
                    return_value=io.BytesIO(response),
                ):
                    with self.assertRaisesRegex(ValueError, expected):
                        AUTHOR.obtain(cache, contract, item, False)
                self.assertEqual(list(cache.iterdir()), [])

        with tempfile.TemporaryDirectory() as directory:
            cache = Path(directory)
            with mock.patch.object(
                AUTHOR.urllib.request,
                "urlopen",
                side_effect=TimeoutError("injected network inactivity"),
            ):
                with self.assertRaisesRegex(TimeoutError, "network inactivity"):
                    AUTHOR.obtain(cache, contract, item, False)
            self.assertEqual(list(cache.iterdir()), [])

    def test_concurrent_cache_winner_is_validated_and_temp_is_removed(self) -> None:
        exact = b"{}"
        contract = {
            "source": "https://huggingface.co/Owner/Model",
            "revision": "a" * 40,
        }
        item = {
            "path": "config.json",
            "bytes": len(exact),
            "sha256": hashlib.sha256(exact).hexdigest(),
        }
        with tempfile.TemporaryDirectory() as directory:
            cache = Path(directory)

            def publish_winner(_source: Path, destination: Path, **_kwargs: object) -> None:
                Path(destination).write_bytes(exact)
                raise FileExistsError

            with mock.patch.object(
                AUTHOR.urllib.request, "urlopen", return_value=io.BytesIO(exact)
            ), mock.patch.object(AUTHOR.os, "link", side_effect=publish_winner):
                cached = AUTHOR.obtain(cache, contract, item, False)
            self.assertEqual(cached.read_bytes(), exact)
            self.assertEqual([path.name for path in cache.iterdir()], [cached.name])

    def test_cli_refuses_symlinked_verification_root(self) -> None:
        contract, encoded = AUTHOR.load_contract(CONTRACT)
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            target = root / "target"
            target.mkdir(mode=0o755)
            link = root / "link"
            link.symlink_to(target, target_is_directory=True)
            errors = io.StringIO()
            with mock.patch.object(
                sys,
                "argv",
                [
                    str(SCRIPT),
                    "--contract",
                    str(CONTRACT),
                    "--verify-tree",
                    str(link),
                ],
            ), contextlib.redirect_stderr(errors), self.assertRaises(SystemExit) as refusal:
                AUTHOR.main()
            self.assertEqual(refusal.exception.code, 2)
            self.assertIn("not an ordinary directory", errors.getvalue())
            self.assertTrue(encoded)


if __name__ == "__main__":
    unittest.main()

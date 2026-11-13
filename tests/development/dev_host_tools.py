#!/usr/bin/env python3
"""Focused tests for the externally invoked development host tools.

Every external program (cargo, ryeosd, ryeos, codex) is a fake written here;
nothing builds, installs, starts a node or reaches a provider.
"""

import importlib.util
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import textwrap
import unittest


ROOT = Path(__file__).resolve().parents[2]
TOOLS = ROOT / ".ai/tools/ryeos/development"
CONFIGS = ROOT / ".ai/config/development/ryeos"


def load(name, relative):
    spec = importlib.util.spec_from_file_location(name, TOOLS / relative)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


DEV_HOST = load("dev_host_under_test", "dev-host/lib/dev_host.py")
GUARDED = load("guarded_cargo", "guarded-cargo/lib/guarded_cargo.py")
FEATURES = load("featured_build_check", "featured-build-check/lib/featured_build_check.py")
SMOKE = load("bundle_set_init_smoke", "bundle-set-init-smoke/lib/bundle_set_init_smoke.py")
DRIFT = load("dev_node_drift_check", "dev-node-drift-check/lib/dev_node_drift_check.py")
HANDSHAKE = load("codex_worker_handshake_check",
                 "codex-worker-handshake-check/lib/codex_worker_handshake_check.py")
SHAPE = load("render_provider_shape_check",
             "render-provider-shape-check/lib/render_provider_shape_check.py")


def executable(path: Path, body: str) -> Path:
    path.write_text(f"#!{sys.executable}\n" + textwrap.dedent(body))
    path.chmod(0o755)
    return path


def signed_body(path: Path) -> dict:
    raw = path.read_bytes()
    return json.loads(raw.split(b"\n", 1)[1] if raw.startswith(b"# ryeos:signed:") else raw)


def has_module(name):
    return importlib.util.find_spec(name) is not None


class Scratch(unittest.TestCase):
    def setUp(self):
        self.tmp = Path(tempfile.mkdtemp(prefix="dev-host-tools-test-"))

    def tearDown(self):
        shutil.rmtree(self.tmp, ignore_errors=True)

    def git_worktree(self) -> Path:
        tree = self.tmp / "worktree"
        (tree / "crates/a/src").mkdir(parents=True)
        (tree / "bundles/core/.ai/config").mkdir(parents=True)
        (tree / "Cargo.toml").write_text("[workspace]\n")
        (tree / "crates/a/src/lib.rs").write_text("pub fn a() {}\n")
        (tree / "bundles/core/.ai/config/x.yaml").write_text("x: 1\n")
        for command in (["init", "-q"], ["add", "."],
                        ["-c", "user.name=t", "-c", "user.email=t@t", "commit", "-qm", "base"]):
            subprocess.run(["git", *command], cwd=tree, check=True, capture_output=True)
        return tree


class SharedConfig(unittest.TestCase):
    def test_committed_configs_load(self):
        GUARDED.load(None, ROOT)
        DEV_HOST.load_dev_node(None, ROOT)
        config = DEV_HOST.load_config(CONFIGS / "render-provider-shape.yaml", SHAPE.NAME, SHAPE.SCHEMA,
                                      SHAPE.KEYS)
        self.assertIsNone(config["owner_id"])

    def test_expansion(self):
        env = {"WORKTREE": "/w"}
        self.assertEqual(DEV_HOST.expand("${UNSET_FOR_TEST:-${WORKTREE}/t}", env), "/w/t")
        self.assertEqual(DEV_HOST.expand("${WORKTREE}/x", env), "/w/x")
        with self.assertRaises(DEV_HOST.ToolError):
            DEV_HOST.expand("${UNSET_FOR_TEST}/x", env)
        with self.assertRaises(DEV_HOST.ToolError):
            DEV_HOST.expand_path("relative/${WORKTREE}", env)

    def test_config_refuses_unknown_and_missing_keys(self):
        config = signed_body(CONFIGS / "guarded-cargo.yaml")
        path = Path(tempfile.mkdtemp()) / "c.yaml"
        try:
            path.write_text(json.dumps({**config, "surprise": 1}))
            with self.assertRaises(GUARDED.ToolError):
                GUARDED.load(path, ROOT)
            del config["disk"]
            path.write_text(json.dumps(config))
            with self.assertRaises(GUARDED.ToolError):
                GUARDED.load(path, ROOT)
        finally:
            shutil.rmtree(path.parent)


class GuardedCargo(Scratch):
    def site_config(self, **changes) -> Path:
        config = signed_body(CONFIGS / "guarded-cargo.yaml")
        config.update(
            lock_path=str(self.tmp / "lock/cargo.lock"),
            target_dir=str(self.tmp / "shared-target"),
            tmpdir=str(self.tmp / "build-tmp"),
            cargo=str(executable(self.tmp / "fake-cargo", """
                import os, sys
                print("fake cargo", " ".join(sys.argv[1:]), os.environ["CARGO_TARGET_DIR"],
                      os.environ["CARGO_INCREMENTAL"], os.environ["TMPDIR"])
                sys.exit(int(os.environ.get("FAKE_CARGO_EXIT", "0")))
            """)),
            poll_seconds={"live_step": 0.05, "unguarded_cargo": 0.05, "watchdog": 0.05},
            wait_for_unguarded_cargo=False,
        )
        config.update(changes)
        path = self.tmp / "guarded-cargo.yaml"
        path.write_text(json.dumps(config))
        return path

    def run_tool(self, *args, env=None):
        return subprocess.run([sys.executable, str(TOOLS / "guarded-cargo/lib/guarded_cargo.py"), *args],
                              capture_output=True, text=True, env={**os.environ, **(env or {})})

    def test_runs_cargo_touches_sources_and_prunes(self):
        tree = self.git_worktree()
        source = tree / "crates/a/src/lib.rs"
        os.utime(source, (1, 1))
        deps = self.tmp / "shared-target/debug/deps"
        deps.mkdir(parents=True)
        (self.tmp / "shared-target/debug/incremental/x").mkdir(parents=True)
        test_binary = deps / "engine-0123456789abcdef"
        test_binary.write_text("")
        test_binary.chmod(0o755)
        kept = deps / "libserde-0123456789abcdef.rlib"
        kept.write_text("")
        stale = deps / "libryeos_engine-0123456789abcdef.rlib"
        stale.write_text("")
        os.utime(stale, (1, 1))
        log = self.tmp / "cargo.log"
        done = self.run_tool("--config-file", str(self.site_config()), "--worktree", str(tree),
                             "--log", str(log), "--", "test", "-p", "a")
        self.assertEqual(done.returncode, 0, done.stderr)
        text = log.read_text()
        self.assertIn(f"fake cargo test -p a {self.tmp / 'shared-target'} 0 {self.tmp / 'build-tmp'}", text)
        self.assertIn("cargo exit=0", text)
        self.assertGreater(source.stat().st_mtime, 1)
        self.assertEqual((self.tmp / "shared-target/.last-source-tree").read_text(), str(tree))
        self.assertFalse((self.tmp / "shared-target/debug/incremental").exists())
        self.assertFalse(test_binary.exists())
        self.assertFalse(stale.exists())
        self.assertTrue(kept.exists())
        # Same worktree again: no second touch.
        os.utime(source, (1, 1))
        done = self.run_tool("--config-file", str(self.site_config()), "--worktree", str(tree),
                             "--", "build", env={"FAKE_CARGO_EXIT": "7"})
        self.assertEqual(done.returncode, 7)
        self.assertEqual(source.stat().st_mtime, 1)

    def test_refuses_below_disk_floor_without_running_cargo(self):
        tree = self.git_worktree()
        config = self.site_config(disk={"paths": [str(self.tmp)], "refuse_below_mib": 2**40,
                                        "kill_below_mib": 0})
        log = self.tmp / "cargo.log"
        done = self.run_tool("--config-file", str(config), "--worktree", str(tree), "--log", str(log),
                             "--", "build")
        self.assertEqual(done.returncode, GUARDED.EXIT_DISK_REFUSED)
        self.assertIn("refusing: disk below", log.read_text())
        self.assertNotIn("fake cargo", log.read_text())

    def test_watchdog_kills_the_process_group(self):
        tree = self.git_worktree()
        sleeper = executable(self.tmp / "slow-cargo", """
            import subprocess, time
            subprocess.Popen(["sleep", "30"])
            time.sleep(30)
        """)
        settings = GUARDED.load(self.site_config(cargo=str(sleeper)), tree)
        readings = iter([5000, 5000, 10])
        say = GUARDED.Reporter(None)
        code = GUARDED.run_cargo(settings, [], None, say, available=lambda _: next(readings, 10))
        self.assertEqual(code, GUARDED.EXIT_WATCHDOG)

    def test_live_step_and_cargo_detection(self):
        proc = self.tmp / "proc"
        for pid, comm, cmdline in ((101, "bash", b"bash\0/site/steps/adopt.sh\0--now"),
                                   (102, "cargo", b"cargo\0build"), (103, "zsh", b"zsh")):
            (proc / str(pid)).mkdir(parents=True)
            (proc / str(pid) / "comm").write_text(comm + "\n")
            (proc / str(pid) / "cmdline").write_bytes(cmdline + b"\0")
        tree = self.git_worktree()
        settings = GUARDED.load(self.site_config(live_step_patterns=[r"^bash /site/steps/(adopt|chain)\.sh"]),
                                tree)
        self.assertEqual(GUARDED.running_live_steps(settings, proc), ["101 bash /site/steps/adopt.sh --now"])
        self.assertEqual(GUARDED.running_cargo(proc), ["102 cargo"])

    def test_keep_newest_test_executables(self):
        tree = self.git_worktree()
        deps = self.tmp / "shared-target/debug/deps"
        deps.mkdir(parents=True)
        old, new = deps / "suite-0000000000000000", deps / "suite-1111111111111111"
        for index, path in enumerate((old, new)):
            path.write_text("")
            path.chmod(0o755)
            os.utime(path, (100 + index, 100 + index))
        prune = signed_body(CONFIGS / "guarded-cargo.yaml")["prune"]
        settings = GUARDED.load(self.site_config(prune={**prune, "test_executables": "keep_newest"}), tree)
        GUARDED.prune(settings, GUARDED.Reporter(None))
        self.assertFalse(old.exists())
        self.assertTrue(new.exists())


def fake_ryeosd(path: Path, features, revision="0" * 40) -> Path:
    return executable(path, f"""
        import json, sys
        assert sys.argv[1:] == ["build-info", "--json"], sys.argv
        print(json.dumps({{"version": "0.6.0", "revision": "{revision}", "build_date": "x",
                          "profile": "release", "compiled_features": {list(features)!r}}}))
    """)


@unittest.skipUnless(has_module("cryptography"), "bundle-features.py needs python3-cryptography")
class FeaturedBuildCheck(Scratch):
    def test_names_missing_features(self):
        needed = DEV_HOST.daemon_features_for(ROOT, set_name="development")
        self.assertIn("scheduling", needed)
        daemon = fake_ryeosd(self.tmp / "ryeosd", [])
        result = FEATURES.check(ROOT, daemon, node_profile="development")
        self.assertFalse(result["ok"])
        self.assertEqual(result["missing"], sorted(needed))
        self.assertEqual(FEATURES.main(["--ryeosd", str(daemon), "--bundle-set", "development"]), 1)

    def test_passes_with_the_set_selection(self):
        needed = DEV_HOST.daemon_features_for(ROOT, set_name="development")
        daemon = fake_ryeosd(self.tmp / "ryeosd", sorted(needed) + ["latency-profiling"])
        self.assertEqual(FEATURES.main(["--ryeosd", str(daemon), "--node-profile", "development"]), 0)
        self.assertEqual(FEATURES.main(["--ryeosd", str(fake_ryeosd(self.tmp / "bare", [])),
                                        "--bundle-set", "core"]), 0)

    def test_refuses_daemon_without_build_info_json(self):
        daemon = executable(self.tmp / "old-ryeosd", "print('version: 0.5')\n")
        self.assertEqual(FEATURES.main(["--ryeosd", str(daemon), "--bundle-set", "core"]), 2)


class InitSmoke(Scratch):
    def fake_ryeos(self, bundles) -> Path:
        return executable(self.tmp / "ryeos", f"""
            import json, sys
            args = sys.argv[1:]
            assert args[0] == "init" and "--node-profile" in args, args
            print(json.dumps({{"bundles_installed": {list(bundles)!r}}}))
        """)

    def test_passes_when_the_profile_set_admits(self):
        expected = DEV_HOST.bundle_set_names(ROOT, DEV_HOST.profile_bundle_set(ROOT, "development"))
        result = SMOKE.smoke(ROOT, self.fake_ryeos(expected), ROOT / "bundles", "development", "local",
                             ROOT / ".dev-keys/PUBLISHER_DEV_TRUST.toml", self.tmp, 30, 30, False, False)
        self.assertTrue(result["ok"], result)
        self.assertFalse(Path(result["app_root"]).parent.exists())

    def test_fails_naming_missing_bundles(self):
        expected = DEV_HOST.bundle_set_names(ROOT, DEV_HOST.profile_bundle_set(ROOT, "development"))
        result = SMOKE.smoke(ROOT, self.fake_ryeos(expected[1:]), ROOT / "bundles", "development", "local",
                             ROOT / ".dev-keys/PUBLISHER_DEV_TRUST.toml", self.tmp, 30, 30, False, True)
        self.assertFalse(result["ok"])
        self.assertEqual(result["missing"], [expected[0]])
        self.assertTrue(Path(result["kept"]).is_dir())

    def test_reports_init_refusal(self):
        ryeos = executable(self.tmp / "ryeos", "import sys\nsys.stderr.write('kind schema refused')\nsys.exit(3)\n")
        result = SMOKE.smoke(ROOT, ryeos, ROOT / "bundles", "core", "local",
                             ROOT / ".dev-keys/PUBLISHER_DEV_TRUST.toml", self.tmp, 30, 30, False, False)
        self.assertFalse(result["ok"])
        self.assertIn("kind schema refused", result["reason"])


class DriftCheck(Scratch):
    def node(self, tree: Path, revision: str) -> Path:
        bin_dir, target = self.tmp / "bin", self.tmp / "target/release"
        bin_dir.mkdir()
        target.mkdir(parents=True)
        fake_ryeosd(bin_dir / "ryeosd", [], revision)
        shutil.copy(bin_dir / "ryeosd", target / "ryeosd")
        (bin_dir / "ryeos").write_text("old cli")
        (target / "ryeos").write_text("new cli")
        app = self.tmp / "app/.ai/bundles"
        shutil.copytree(tree / "bundles/core", app / "core")
        (app / "core/.ai/objects").mkdir()
        (app / "core/.ai/objects/ab").write_text("cas")
        config = signed_body(CONFIGS / "dev-node.yaml")
        config.update(installed_bin_dir=str(bin_dir), app_root=str(self.tmp / "app"),
                      target_dir=str(self.tmp / "target"))
        path = self.tmp / "dev-node.yaml"
        path.write_text(json.dumps(config))
        return path

    def test_reports_binary_bundle_and_source_drift(self):
        tree = self.git_worktree()
        head = subprocess.run(["git", "rev-parse", "HEAD"], cwd=tree, capture_output=True,
                              text=True, check=True).stdout.strip()
        config = self.node(tree, head)
        node = DEV_HOST.load_dev_node(config, tree)
        rows = {(r["kind"], r["name"]): r for r in
                DRIFT.host_binary_rows(node) + DRIFT.daemon_source_rows(node)
                + DRIFT.bundle_rows(node, node.app_root, None)}
        self.assertTrue(rows[("host binary", "ryeosd")]["ok"])
        self.assertFalse(rows[("host binary", "ryeos")]["ok"])
        self.assertTrue(rows[("daemon source", "ryeosd")]["ok"], rows[("daemon source", "ryeosd")])
        self.assertTrue(rows[("bundle", "core")]["ok"], rows[("bundle", "core")])
        # Change a source and a bundle file in the worktree.
        (tree / "crates/a/src/lib.rs").write_text("pub fn b() {}\n")
        (tree / "bundles/core/.ai/config/x.yaml").write_text("x: 2\n")
        self.assertEqual(DRIFT.main(["--worktree", str(tree), "--config-file", str(config)]), 1)
        rows = {(r["kind"], r["name"]): r for r in
                DRIFT.daemon_source_rows(node) + DRIFT.bundle_rows(node, node.app_root, ["core", "graph"])}
        self.assertIn("crates/a/src/lib.rs", rows[("daemon source", "ryeosd")]["detail"])
        self.assertIn(".ai/config/x.yaml", rows[("bundle", "core")]["detail"])
        self.assertFalse(rows[("bundle", "graph")]["ok"])

    def test_unknown_revision_is_a_note(self):
        tree = self.git_worktree()
        node = DEV_HOST.load_dev_node(self.node(tree, "f" * 40), tree)
        (row,) = DRIFT.daemon_source_rows(node)
        self.assertIsNone(row["ok"])


@unittest.skipUnless(has_module("jsonschema"), "the handshake check needs jsonschema")
class CodexHandshake(Scratch):
    def profile(self) -> Path:
        hosted = self.tmp / "hosted"
        (hosted / "schema").mkdir(parents=True)
        (hosted / "baseline.toml").write_text("model = 'x'\n")
        (hosted / "schema/Init.json").write_text(json.dumps(
            {"type": "object", "required": ["userAgent"], "properties": {"userAgent": {"type": "string"}}}))
        (hosted / "schema/Thread.json").write_text(json.dumps({"type": "object", "required": ["thread"]}))
        (hosted / "p.json").write_text(json.dumps({
            "baseline_config": "baseline.toml", "baseline_destination": "config.toml",
            "workload_home_env": "CODEX_HOME", "workload_args": ["app-server"],
            "initialization": [
                {"method": "initialize", "params": {}, "response_schema": "schema/Init.json"},
                {"notification": "initialized", "params": {}},
                {"method": "config/read", "params": {}}],
            "routes": [{"method": "thread/start", "fixed_params": {"x": 1}, "response_schema": "schema/Thread.json",
                        "response_predicates": [{"pointer": "/response/result/approvalPolicy",
                                                 "equals": "never"}]}]}))
        return hosted

    def fake_codex(self, approval: str) -> Path:
        return executable(self.tmp / "codex", f"""
            import json, os, sys
            assert sys.argv[1:] == ["app-server"] and os.environ["PATH"] == ""
            home = os.environ["CODEX_HOME"]
            assert os.path.exists(os.path.join(home, "config.toml"))
            assert "ryeos-external-candidate" in open(os.path.join(home, "environments.toml")).read()
            for line in sys.stdin:
                message = json.loads(line)
                if "id" not in message:
                    continue
                result = {{"initialize": {{"userAgent": "codex/1"}}, "config/read": {{}},
                          "thread/start": {{"thread": {{}}, "approvalPolicy": "{approval}"}}}}[message["method"]]
                print(json.dumps({{"id": message["id"], "result": result}}), flush=True)
        """)

    def test_passes_signed_gates(self):
        self.assertEqual(HANDSHAKE.main(["--codex", str(self.fake_codex("never")),
                                         "--profile-dir", str(self.profile()), "--profile", "p.json"]), 0)

    def test_fails_a_predicate(self):
        results = HANDSHAKE.run(self.fake_codex("on-request"), self.profile(), "p.json", 10)
        failed = [message for ok, message in results if not ok]
        self.assertEqual(len(failed), 1)
        self.assertIn("approvalPolicy", failed[0])

    def test_the_repository_profile_has_the_expected_shape(self):
        profile = json.loads((ROOT / HANDSHAKE.PROFILE_DIR / "external-authoring.profile.json").read_text())
        hosted = ROOT / HANDSHAKE.PROFILE_DIR
        self.assertTrue((hosted / profile["baseline_config"]).is_file())
        route = next(r for r in profile["routes"] if r["method"] == "thread/start")
        for name in [route["response_schema"]] + [s["response_schema"] for s in profile["initialization"]
                                                  if s.get("response_schema")]:
            self.assertTrue((hosted / name).is_file(), name)


class RenderProviderShape(unittest.TestCase):
    SOURCE = (ROOT / "crates/host-adapters/render-sandbox-adapter/src/main.rs").read_text()
    FIXTURES = ROOT / "crates/host-adapters/render-sandbox-adapter/fixtures"

    def test_reads_the_adapter_shape(self):
        shapes = SHAPE.adapter_shapes(self.SOURCE)
        sandbox = shapes["RenderSandbox"]
        self.assertTrue(sandbox["closed"])
        self.assertEqual({k for k, f in sandbox["fields"].items() if not f["required"]}, {"name", "terminatedAt"})
        self.assertEqual(shapes["RenderSandboxPlan"]["enum"], {"starter", "standard", "pro"})
        self.assertIn("terminated", shapes["RenderSandboxStatus"]["enum"])
        self.assertEqual(shapes["RenderNetworkPolicyType"]["enum"], {"allow-all", "allow-list", "deny-all"})

    def test_adapter_fixtures_fit(self):
        for name in ("terminated-response.json", "create-response.json", "create-response-allow-list.json"):
            response = json.loads((self.FIXTURES / name).read_text())
            self.assertEqual(SHAPE.check_response(response, self.SOURCE, "RenderSandbox"), [], name)

    def test_drift_is_named(self):
        base = json.loads((self.FIXTURES / "terminated-response.json").read_text())
        changed = {**base, "projectId": "p", "plan": "enterprise", "networkPolicy": {"type": "open"}}
        del changed["sandboxGroupId"]
        problems = "\n".join(SHAPE.check_response(changed, self.SOURCE, "RenderSandbox"))
        for expected in ("projectId", "sandboxGroupId", "enterprise", "networkPolicy.type"):
            self.assertIn(expected, problems)


if __name__ == "__main__":
    unittest.main()

#!/usr/bin/env python3
import importlib.util
import json
import tempfile
from pathlib import Path
import unittest

SPEC = importlib.util.spec_from_file_location("probe", Path(__file__).with_name("modal-e0m-probe.py"))
PROBE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(PROBE)


class FakeSandbox:
    def __init__(self, registry):
        self.object_id = f"sb-{len(registry)}"
        self.tags = {}
        self.terminated = False
        registry.append(self)

    def set_tags(self, tags):
        self.tags = dict(tags)

    def terminate(self):
        self.terminated = True


class FakeModal:
    __version__ = "0.0-fake"

    def __init__(self, leak=False):
        self.registry = []
        self.leak = leak
        outer = self

        class Sandbox:
            @staticmethod
            def create(*args, **kwargs):
                return FakeSandbox(outer.registry)

            @staticmethod
            def list(tags=None):
                return [s for s in outer.registry if (not s.terminated or outer.leak) and s.tags == tags]

        class App:
            @staticmethod
            def lookup(name, create_if_missing=False):
                return object()

        class Image:
            @staticmethod
            def debian_slim():
                return object()

        self.Sandbox, self.App, self.Image = Sandbox, App, Image


def mk_step(name, fn):
    return {"name": name, "description": "", "gpu": False, "fn": fn}


class ProbeTests(unittest.TestCase):
    def setUp(self):
        self.saved_steps = PROBE.STEPS[:]
        self.saved_loader = PROBE.load_modal
        self.saved_sleep = PROBE.time.sleep

    def tearDown(self):
        PROBE.STEPS[:] = self.saved_steps
        PROBE.load_modal = self.saved_loader
        PROBE.time.sleep = self.saved_sleep

    def run_main(self, argv):
        with tempfile.TemporaryDirectory() as d:
            rc = PROBE.main(["--out", d] + argv)
            path = Path(d) / PROBE.EVIDENCE_NAME
            return rc, json.loads(path.read_text())

    def test_plan_excludes_gpu_by_default(self):
        names = [s["name"] for s in PROBE.plan_for(False, [])]
        self.assertNotIn("gpu_t4", names)
        self.assertIn("gpu_t4", [s["name"] for s in PROBE.plan_for(True, [])])
        for required in ("egress_tls", "dns_residual", "websocket_echo", "lost_stream", "name_uniqueness",
                         "block_allow_rejection", "timeout_exit", "terminate_observe", "file_write", "guest_facts"):
            self.assertIn(required, names)

    def test_only_filters_and_rejects_unknown(self):
        self.assertEqual([s["name"] for s in PROBE.plan_for(False, ["file_write"])], ["file_write"])
        with self.assertRaises(SystemExit):
            PROBE.plan_for(False, ["nope"])

    def test_step_raising_still_records(self):
        def boom(ctx):
            raise ValueError("bad")
        rec = PROBE.run_step("boom", boom, None)
        self.assertEqual(set(rec), {"name", "ok", "evidence", "error", "seconds"})
        self.assertFalse(rec["ok"])
        self.assertTrue(rec["error"].startswith("ValueError"))

    def test_exit_code(self):
        good = PROBE.run_step("a", lambda c: {}, None)
        bad = PROBE.run_step("b", lambda c: 1 / 0, None)
        self.assertEqual(PROBE.exit_code([good]), 0)
        self.assertEqual(PROBE.exit_code([good, bad]), 1)

    def test_dry_run_writes_plan_without_loading_modal(self):
        def forbidden():
            raise AssertionError("modal must not load in dry run")
        PROBE.load_modal = forbidden
        rc, ev = self.run_main(["--dry-run"])
        self.assertEqual(rc, 0)
        self.assertTrue(ev["dry_run"])
        self.assertEqual(ev["steps"], [])
        self.assertFalse(ev["ok"])
        self.assertEqual(ev["app"], "ryeos-e0m")
        self.assertTrue(ev["plan"])

    def test_run_continues_after_failure_and_cleans_up(self):
        fake = FakeModal()
        PROBE.load_modal = lambda: fake
        PROBE.STEPS[:] = [
            mk_step("mk", lambda c: {"id": c.sandbox().object_id}),
            mk_step("bad", lambda c: 1 / 0),
            mk_step("after", lambda c: {}),
        ]
        rc, ev = self.run_main([])
        self.assertEqual(rc, 1)
        self.assertEqual([s["name"] for s in ev["steps"]], ["mk", "bad", "after", "cleanup"])
        self.assertEqual([s["ok"] for s in ev["steps"]], [True, False, True, True])
        self.assertTrue(all(s.terminated for s in fake.registry))
        self.assertEqual(fake.registry[0].tags[PROBE.TAG_KEY], ev["run_id"])

    def test_cleanup_fails_when_sandboxes_remain(self):
        fake = FakeModal(leak=True)
        ctx = PROBE.Ctx(fake, "app", "run1", False)
        ctx.sandbox()
        PROBE.time.sleep = lambda s: None
        rec = PROBE.run_step("cleanup", PROBE.cleanup_step, ctx)
        self.assertFalse(rec["ok"])
        self.assertIn("remain", rec["error"])

    def test_keep_skips_cleanup(self):
        fake = FakeModal()
        PROBE.load_modal = lambda: fake
        PROBE.STEPS[:] = [mk_step("mk", lambda c: {"id": c.sandbox().object_id})]
        rc, ev = self.run_main(["--keep"])
        self.assertEqual(rc, 0)
        self.assertEqual([s["name"] for s in ev["steps"]], ["mk"])
        self.assertFalse(fake.registry[0].terminated)

    def test_interrupt_still_cleans_up_and_writes_evidence(self):
        fake = FakeModal()
        PROBE.load_modal = lambda: fake

        def ctrl_c(c):
            c.sandbox()
            raise KeyboardInterrupt
        PROBE.STEPS[:] = [mk_step("ci", ctrl_c)]
        rc, ev = self.run_main([])
        self.assertEqual(rc, 1)
        self.assertEqual([s["name"] for s in ev["steps"]], ["interrupted", "cleanup"])
        self.assertTrue(ev["steps"][1]["ok"])
        self.assertTrue(fake.registry[0].terminated)

    def test_set_tags_failure_is_still_terminated(self):
        fake = FakeModal()
        ctx = PROBE.Ctx(fake, "app", "run1", False)
        orig = FakeSandbox.set_tags

        def bad(self, tags):
            raise RuntimeError("tag failed")
        FakeSandbox.set_tags = bad
        try:
            with self.assertRaises(RuntimeError):
                ctx.sandbox()
        finally:
            FakeSandbox.set_tags = orig
        rec = PROBE.run_step("cleanup", PROBE.cleanup_step, ctx)
        self.assertTrue(fake.registry[0].terminated)
        self.assertTrue(rec["ok"])

    def test_sandbox_helper_sets_lifetime(self):
        seen = {}
        fake = FakeModal()
        orig = fake.Sandbox.create
        fake.Sandbox.create = staticmethod(lambda *a, **k: seen.update(k) or orig(*a, **k))
        PROBE.Ctx(fake, "app", "r", False).sandbox()
        self.assertEqual(seen["timeout"], PROBE.DEFAULT_LIFETIME)

    def test_in_guest_programs_compile(self):
        compile(PROBE.WS_SERVER, "ws_server", "exec")
        compile(PROBE.EGRESS_CODE, "egress", "exec")


if __name__ == "__main__":
    unittest.main()

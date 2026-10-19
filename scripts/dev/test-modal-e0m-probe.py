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


class FakeStream:
    def __init__(self, data=b""):
        self.data = data
        self.written = bytearray()

    def read(self):
        return self.data

    def write(self, b):
        self.written += b

    def write_eof(self):
        pass

    def drain(self):
        pass

    def __iter__(self):
        return iter(self.data.splitlines(True))


class FakeProc:
    def __init__(self, out=b"", err=b"", code=0):
        self.stdin, self.stdout, self.stderr = FakeStream(), FakeStream(out), FakeStream(err)
        self.code = code

    def wait(self):
        return self.code


class ExecSandbox(FakeSandbox):
    """Sandbox whose exec serves `stat`, `sha256sum` and slot appends from an in-memory file."""

    def __init__(self, registry):
        super().__init__(registry)
        self.file = bytearray()
        self.calls = []

    def exec(self, *argv, timeout=None):
        self.calls.append(argv)
        if argv[0] == "sh" and argv[1] == "-c" and "cat >>" in argv[2]:
            p = FakeProc()
            orig = p.stdin.write
            p.stdin.write = lambda b: self.file.extend(b) or orig(b)
            return p
        if argv[0] == "stat":
            return FakeProc(str(len(self.file)))
        if argv[0] == "sha256sum":
            return FakeProc(PROBE.sha256_hex(bytes(self.file)) + "  " + argv[1])
        if argv[0] == "rm":
            self.file.clear()
        if argv[0] == "df":
            return FakeProc("Filesystem 1 2 3 4% /ryeos")
        return FakeProc("")


def mk_step(name, fn):
    return {"name": name, "description": "", "gpu": False, "group": "e0m", "fn": fn}


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

    def test_launcher_group_selection(self):
        launcher = ["exec_append_binary", "staged_large_write", "tag_limits", "account_switch",
                    "glibc_and_namespaces", "tls_dial_by_ip", "long_exec_stream"]
        self.assertEqual([s["name"] for s in PROBE.plan_for(False, [], "launcher")], launcher)
        default = [s["name"] for s in PROBE.plan_for(False, [])]
        self.assertEqual(len(default), 12)
        self.assertFalse(set(launcher) & set(default))
        everything = [s["name"] for s in PROBE.plan_for(False, [], "all")]
        self.assertEqual(everything, default + launcher)
        self.assertEqual([s["name"] for s in PROBE.plan_for(False, ["tag_limits"])], ["tag_limits"])
        with self.assertRaises(SystemExit):
            PROBE.plan_for(False, [], "bogus")

    def test_group_flag_dry_run(self):
        rc, ev = self.run_main(["--dry-run", "--group", "launcher"])
        self.assertEqual(ev["group"], "launcher")
        self.assertEqual({s["group"] for s in ev["plan"]}, {"launcher"})
        with self.assertRaises(SystemExit):
            PROBE.parse_args(["--group", "bogus"])

    def test_chunk_plan(self):
        self.assertEqual(PROBE.chunk_plan(10, 4), [(0, 4), (4, 4), (8, 2)])
        self.assertEqual(PROBE.chunk_plan(0, 4), [])
        self.assertEqual(len(PROBE.chunk_plan(PROBE.LARGE_TOTAL, PROBE.LARGE_CHUNK)), 32)
        with self.assertRaises(ValueError):
            PROBE.chunk_plan(10, 0)

    def test_deadline_with_fake_clock(self):
        now = [100.0]
        d = PROBE.Deadline(5, clock=lambda: now[0])
        self.assertFalse(d.expired())
        now[0] = 104.0
        self.assertEqual(d.remaining(), 1.0)
        now[0] = 105.0
        self.assertTrue(d.expired())

    def test_hash_and_rate_helpers(self):
        self.assertEqual(PROBE.sha256_hex(b""), "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855")
        self.assertEqual(PROBE.mb_per_s(PROBE.MIB * 10, 2), 5.0)
        self.assertIsNone(PROBE.mb_per_s(1, 0))

    def test_exec_run_pieces_stdin(self):
        sb = ExecSandbox([])
        sb.exec = lambda *a, timeout=None: sb.proc
        sb.proc = FakeProc(b"o", b"e", 3)
        data = b"x" * (PROBE.STDIN_PIECE + 5)
        self.assertEqual(PROBE.exec_run(sb, "cat", stdin=data), (3, b"o", b"e"))
        self.assertEqual(bytes(sb.proc.stdin.written), data)

    def test_attempt_records_errors(self):
        class Boom:
            def exec(self, *a, timeout=None):
                raise RuntimeError("no such binary")
        rec = PROBE.attempt(Boom(), "useradd")
        self.assertIn("no such binary", rec["error"])
        self.assertEqual(rec["argv"], ["useradd"])

    def test_scratch_terminates_on_error(self):
        fake = FakeModal()
        ctx = PROBE.Ctx(fake, "app", "r", False)
        with self.assertRaises(ValueError):
            with ctx.scratch() as sb:
                raise ValueError("x")
        self.assertTrue(sb.terminated)

    def test_staged_large_write_completes_and_hits_deadline(self):
        fake = FakeModal()
        fake.Sandbox.create = staticmethod(lambda *a, **k: ExecSandbox(fake.registry))
        saved = (PROBE.LARGE_TOTAL, PROBE.LARGE_CHUNK, PROBE.os.urandom, PROBE.Deadline.expired)
        PROBE.LARGE_TOTAL, PROBE.LARGE_CHUNK = 40, 16
        PROBE.os.urandom = lambda n: b"a" * n
        try:
            ev = PROBE.staged_large_write(PROBE.Ctx(fake, "app", "r", False))
            self.assertTrue(ev["completed"])
            self.assertEqual((ev["chunks_done"], ev["bytes_done"], ev["final_size"]), (3, 40, 40))
            self.assertIn("df_after", ev)
            PROBE.Deadline.expired = lambda self: True
            ev = PROBE.staged_large_write(PROBE.Ctx(fake, "app", "r", False))
            self.assertTrue(ev["deadline_hit"])
            self.assertFalse(ev["completed"])
            self.assertEqual(ev["bytes_done"], 0)
        finally:
            PROBE.LARGE_TOTAL, PROBE.LARGE_CHUNK, PROBE.os.urandom, PROBE.Deadline.expired = saved
        self.assertTrue(all(s.terminated for s in fake.registry))

    def test_staged_large_write_keeps_partial_evidence_on_error(self):
        fake = FakeModal()
        fake.Sandbox.create = staticmethod(lambda *a, **k: ExecSandbox(fake.registry))
        saved = (PROBE.LARGE_TOTAL, PROBE.LARGE_CHUNK, PROBE.os.urandom, PROBE.append_chunk)
        calls = []

        def flaky(sb, data, encoding="raw", timeout=300):
            if calls:
                raise RuntimeError("stream lost")
            calls.append(1)
            sb.file.extend(data)
        PROBE.LARGE_TOTAL, PROBE.LARGE_CHUNK = 40, 16
        PROBE.os.urandom = lambda n: b"a" * n
        PROBE.append_chunk = flaky
        try:
            ev = PROBE.staged_large_write(PROBE.Ctx(fake, "app", "r", False))
        finally:
            PROBE.LARGE_TOTAL, PROBE.LARGE_CHUNK, PROBE.os.urandom, PROBE.append_chunk = saved
        self.assertFalse(ev["completed"])
        self.assertEqual((ev["chunks_done"], ev["bytes_done"]), (1, 16))
        self.assertIn("RuntimeError: stream lost", ev["error"])
        self.assertTrue(fake.registry[0].terminated)

    def test_exec_append_binary_raw_match(self):
        fake = FakeModal()
        fake.Sandbox.create = staticmethod(lambda *a, **k: ExecSandbox(fake.registry))
        saved = PROBE.os.urandom
        PROBE.os.urandom = lambda n: b"b" * 64
        try:
            ev = PROBE.exec_append_binary(PROBE.Ctx(fake, "app", "r", False))
        finally:
            PROBE.os.urandom = saved
        self.assertEqual(ev["working_encoding"], "raw")
        self.assertTrue(ev["attempts"][0]["match"])
        self.assertTrue(fake.registry[0].terminated)

    def test_tag_limits_keeps_run_tag(self):
        fake = FakeModal()
        ctx = PROBE.Ctx(fake, "app", "run9", False)
        orig = FakeSandbox.set_tags

        def limited(self, tags):
            if len(tags) > 5:
                raise ValueError("too many tags")
            orig(self, tags)
        FakeSandbox.set_tags = limited
        try:
            ev = PROBE.tag_limits(ctx)
        finally:
            FakeSandbox.set_tags = orig
        self.assertTrue(all(r["accepted"] for r in ev["value_lengths"]))
        self.assertEqual([r["accepted"] for r in ev["counts"]], [True, True, True, False])
        self.assertIn("too many", ev["counts"][3]["error"])
        self.assertEqual(fake.registry[0].tags, {PROBE.TAG_KEY: "run9"})
        self.assertTrue(fake.registry[0].terminated)

    def test_off_list_address_differs(self):
        self.assertEqual(PROBE.pick_off_list("1.1.1.1"), "8.8.8.8")
        self.assertEqual(PROBE.pick_off_list("93.184.216.34"), "1.1.1.1")

    def test_launcher_guest_programs_compile(self):
        compile(PROBE.TLS_DIAL_CODE, "tls_dial", "exec")
        compile(PROBE.LONG_STREAM_CODE, "long_stream", "exec")


if __name__ == "__main__":
    unittest.main()

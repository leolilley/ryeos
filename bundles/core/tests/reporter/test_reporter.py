"""Pure-Python tests for the Core reporter revisions (stdlib only).

The doctor result below has the shape the doctor emits for the malformed
scheduled-doctor fixture: a failing manifest check naming bogus_field, a
passing verify, an import check that could not run, and the advisory
bundle-events check. It is a shape fixture, not retained live evidence.
"""

import copy
import importlib.util
import json
import pathlib
import unittest

TOOLS = (
    pathlib.Path(__file__).resolve().parents[2]
    / ".ai" / "tools" / "ryeos" / "core" / "reporter"
)


def load(filename):
    spec = importlib.util.spec_from_file_location(filename[:-3], TOOLS / filename)
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)
    return mod


R0, R1, R2 = load("report.py"), load("report_r1.py"), load("report_r2.py")

DOCTOR = {
    "source": "/fixture/scheduled-doctor",
    "ok": False,
    "checks": [
        {"name": "manifest", "status": "fail", "detail": {
            "error": "manifest.source.yaml is malformed: unknown field `bogus_field`"}},
        {"name": "verify", "status": "ok", "detail": {"warnings": []}},
        {"name": "import:tool:x/y", "status": "n/a", "detail": {
            "import_check": "unavailable",
            "import_check_reason": "offline engine unavailable: none"}},
        {"name": "bundle_events_coverage", "status": "unknown", "detail": {
            "declared_event_kinds": [], "note": "not determined statically"}},
    ],
}


def identity(result=DOCTOR, **over):
    ident = {
        "thread_id": "T-doctor-1",
        "tool_ref": "tool:ryeos/core/node/doctor",
        "result_sha256": R0.sha256_of(result),
    }
    ident.update(over)
    return ident


def params(**over):
    p = {"doctor_identity": identity(), "doctor_result": copy.deepcopy(DOCTOR)}
    p.update(over)
    return p


def status_of(report, name):
    return next(c["status"] for c in report["checks"] if c["name"] == name)


class R0Tests(unittest.TestCase):
    def test_canonical_stable_and_provenance(self):
        a = R0.execute(params(), "/p")
        b = R0.execute(params(), "/p")
        self.assertEqual(R0.canonical_json(a), R0.canonical_json(b))
        self.assertEqual(a["provenance"]["doctor_identity"], identity())
        self.assertEqual(a["provenance"]["doctor_source"], DOCTOR["source"])
        body = {k: v for k, v in a.items() if k != "report_sha256"}
        self.assertEqual(a["report_sha256"], R0.sha256_of(body))
        self.assertEqual(a["revision"], "r0")

    def test_check_order_independent(self):
        shuffled = copy.deepcopy(DOCTOR)
        shuffled["checks"].reverse()
        p = params(doctor_result=shuffled, doctor_identity=identity(shuffled))
        self.assertEqual(
            [c["name"] for c in R0.execute(p, "/p")["checks"]],
            sorted(c["name"] for c in DOCTOR["checks"]),
        )

    def test_ok_false_finding_preserved(self):
        r = R0.execute(params(), "/p")
        self.assertIs(r["doctor_ok"], False)
        self.assertEqual(status_of(r, "manifest"), "fail")
        self.assertIn("bogus_field", json.dumps(r))
        self.assertEqual(status_of(r, "bundle_events_coverage"), "unknown")
        self.assertEqual(status_of(r, "import:tool:x/y"), "n/a")

    def test_refusals_by_name(self):
        cases = [
            ("missing_doctor_identity", params(doctor_identity=None)),
            ("missing_doctor_identity_field",
             params(doctor_identity={k: v for k, v in identity().items() if k != "thread_id"})),
            ("missing_doctor_result", params(doctor_result=None)),
            ("doctor_tool_mismatch", params(doctor_identity=identity(tool_ref="tool:other"))),
            ("doctor_identity_mismatch",
             params(doctor_identity=identity(result_sha256="0" * 64))),
            ("malformed_doctor_result", params(doctor_result={"ok": "yes", "checks": []},
                                              doctor_identity=identity({"ok": "yes", "checks": []}))),
        ]
        for mod in (R0, R1, R2):
            for code, p in cases:
                if mod is R2:
                    p = dict(p, prior_report=R0.execute(params(), "/p"))
                with self.assertRaises(mod.ReportRefusal, msg=code) as cm:
                    mod.execute(p, "/p")
                self.assertEqual(cm.exception.code, code)

    def test_tampered_result_refused(self):
        p = params()
        p["doctor_result"]["ok"] = True
        with self.assertRaises(R0.ReportRefusal) as cm:
            R0.execute(p, "/p")
        self.assertEqual(cm.exception.code, "doctor_identity_mismatch")


class R1Tests(unittest.TestCase):
    def setUp(self):
        self.r = R1.execute(params(), "/p")

    def test_superset_of_r0(self):
        r0 = R0.execute(params(), "/p")
        for key in ("provenance", "doctor_ok", "status_counts"):
            self.assertEqual(self.r[key], r0[key])
        for a, b in zip(r0["checks"], self.r["checks"]):
            self.assertEqual((a["name"], a["status"], a["detail"]),
                             (b["name"], b["status"], b["detail"]))

    def test_classification_and_groups(self):
        self.assertEqual(self.r["groups"], {
            "finding": ["manifest"],
            "unavailable": ["import:tool:x/y"],
            "advisory": ["bundle_events_coverage"],
            "passed": ["verify"],
        })
        self.assertEqual(self.r["verdict"], "findings_present")
        self.assertIs(self.r["doctor_ok"], False)

    def test_actions_ordered_and_actionable(self):
        self.assertEqual([a["check"] for a in self.r["actions"]],
                         ["manifest", "import:tool:x/y", "bundle_events_coverage"])
        self.assertIn("bogus_field", self.r["actions"][0]["message"])

    def test_unavailable_and_advisory_never_become_success(self):
        clean = {"source": "s", "ok": True, "checks": [DOCTOR["checks"][1],
                                                         DOCTOR["checks"][2],
                                                         DOCTOR["checks"][3]]}
        r = R1.execute({"doctor_identity": identity(clean), "doctor_result": clean}, "/p")
        self.assertEqual(r["verdict"], "incomplete")
        self.assertEqual(status_of(r, "import:tool:x/y"), "n/a")
        self.assertEqual(status_of(r, "bundle_events_coverage"), "unknown")
        only_ok = {"source": "s", "ok": True, "checks": [DOCTOR["checks"][1]]}
        r = R1.execute({"doctor_identity": identity(only_ok), "doctor_result": only_ok}, "/p")
        self.assertEqual(r["verdict"], "clean")


class R2Tests(unittest.TestCase):
    def test_requires_prior(self):
        with self.assertRaises(R2.ReportRefusal) as cm:
            R2.execute(params(), "/p")
        self.assertEqual(cm.exception.code, "missing_prior_report")

    def test_malformed_and_tampered_prior(self):
        with self.assertRaises(R2.ReportRefusal) as cm:
            R2.execute(params(prior_report={"schema": "x"}), "/p")
        self.assertEqual(cm.exception.code, "malformed_prior_report")
        prior = R1.execute(params(), "/p")
        prior["checks"][0]["status"] = "ok"
        with self.assertRaises(R2.ReportRefusal) as cm:
            R2.execute(params(prior_report=prior), "/p")
        self.assertEqual(cm.exception.code, "prior_report_digest_mismatch")

    def test_identical_run_has_no_changes(self):
        prior = R1.execute(params(), "/p")
        cmp_ = R2.execute(params(prior_report=prior), "/p")["comparison"]
        self.assertFalse(cmp_["has_changes"])
        self.assertTrue(cmp_["same_doctor_result"])
        self.assertEqual(cmp_["unchanged_count"], 4)

    def test_changes_listed_against_r0_prior(self):
        prior = R0.execute(params(), "/p")
        newer = copy.deepcopy(DOCTOR)
        newer["ok"] = True
        newer["checks"][0] = {"name": "manifest", "status": "ok", "detail": {}}
        newer["checks"].pop(3)
        newer["checks"].append({"name": "extra", "status": "warning", "detail": None})
        p = {"doctor_identity": identity(newer, thread_id="T-doctor-2"),
             "doctor_result": newer, "prior_report": prior}
        r = R2.execute(p, "/p")
        c = r["comparison"]
        self.assertEqual(c["prior_revision"], "r0")
        self.assertEqual(c["added"], ["extra"])
        self.assertEqual(c["removed"], ["bundle_events_coverage"])
        self.assertEqual(c["status_changed"],
                         [{"check": "manifest", "from": "fail", "to": "ok"}])
        self.assertEqual([d["check"] for d in c["detail_changed"]], ["manifest"])
        self.assertTrue(c["doctor_ok_changed"])
        self.assertFalse(c["same_doctor_result"])
        self.assertEqual(c["prior_doctor_identity"], identity())
        self.assertEqual(r["revision"], "r2")
        self.assertEqual(r["reporter"], "tool:ryeos/core/reporter/report_r2")

    def test_preserved_finding_in_r2(self):
        r = R2.execute(params(prior_report=R0.execute(params(), "/p")), "/p")
        self.assertIs(r["doctor_ok"], False)
        self.assertEqual(status_of(r, "manifest"), "fail")


if __name__ == "__main__":
    unittest.main()

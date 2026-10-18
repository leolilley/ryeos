#!/usr/bin/env python3
import copy
import importlib.util
import json
import tempfile
from pathlib import Path
import unittest

SPEC = importlib.util.spec_from_file_location("prove", Path(__file__).with_name("prove-contained-oci-provider.py"))
PROVE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(PROVE)

OCI = "external_controller:oci"


def proof(provider=OCI, source="measured"):
    return {"source": source, "provider": provider, "basis": "probe"}


def passing():
    entry = {"name": "termination:scope", "providers": [OCI]}
    report = {
        "composition": {"profile": "contained-workflow"},
        "provided": {"quiescence:scope": proof(), "termination:scope": proof(), "recovery:scope": proof(), "limits:scope": proof(),
                     "storage:durable_root": proof("host")},
        "floor": {"requires": ["time", entry], "admitted": True, "unmet": {},
                  "met_by": {"termination:scope": {"name": "termination:scope", "source": "measured", "provider": OCI}}},
    }
    return {"ok": True, "checks": [{"name": "capabilities", "status": "ok", "detail": {"current": True, "report": report}}]}


def report_of(doctor):
    return doctor["checks"][0]["detail"]["report"]


class ProveTests(unittest.TestCase):
    def failures(self, doctor):
        return PROVE.verify(doctor)[0]

    def test_passing_report(self):
        self.assertEqual(self.failures(passing()), [])

    def test_termination_wrong_source_provider_or_absent(self):
        for change, text in [(lambda p: p.update(source="provider_attested"), "source is 'provider_attested'"),
                             (lambda p: p.update(provider="host"), "provider is 'host'")]:
            doctor = passing()
            change(report_of(doctor)["provided"]["termination:scope"])
            failures = self.failures(doctor)
            self.assertEqual(len(failures), 1)
            self.assertIn("termination:scope " + text, failures[0])
        doctor = passing()
        del report_of(doctor)["provided"]["termination:scope"]
        self.assertIn("not provided", self.failures(doctor)[0])

    def test_missing_non_termination_scope(self):
        for name in ["quiescence:scope", "recovery:scope", "limits:scope"]:
            doctor = passing()
            del report_of(doctor)["provided"][name]
            self.assertEqual(self.failures(doctor), [f"{name} is not provided"])

    def test_any_of_constrained_floor_label(self):
        doctor = passing()
        floor = report_of(doctor)["floor"]
        floor["requires"] = [{"any_of": ["a", "b"], "providers": [OCI, "host"]}]
        floor["met_by"] = {"any_of(a, b)": {"name": "a", "source": "measured", "provider": OCI}}
        self.assertEqual(self.failures(doctor), [])
        floor["met_by"]["any_of(a, b)"]["provider"] = "host"
        self.assertIn("any_of(a, b)", self.failures(doctor)[0])

    def test_stale_report_and_detail_without_report(self):
        doctor = passing()
        doctor["checks"][0]["detail"]["current"] = {"stale": "policy changed"}
        self.assertIn("not current", self.failures(doctor)[0])
        doctor = {"checks": [{"name": "capabilities", "detail": {"note": "no report"}}]}
        self.assertEqual(len(self.failures(doctor)), 1)

    def test_other_scope_provider_mismatch(self):
        doctor = passing()
        report_of(doctor)["provided"]["recovery:scope"]["provider"] = "host"
        self.assertEqual(len(self.failures(doctor)), 1)
        self.assertIn("recovery:scope", self.failures(doctor)[0])

    def test_constrained_floor_met_by_other_provider_or_missing(self):
        doctor = passing()
        report_of(doctor)["floor"]["met_by"]["termination:scope"]["provider"] = "host"
        self.assertIn("floor entry", self.failures(doctor)[0])
        doctor = passing()
        report_of(doctor)["floor"]["met_by"] = {}
        self.assertIn("no met_by", self.failures(doctor)[0])

    def test_profile_and_admission(self):
        doctor = passing()
        report_of(doctor)["composition"]["profile"] = "standard"
        self.assertIn("profile", self.failures(doctor)[0])
        doctor = passing()
        report_of(doctor)["floor"]["admitted"] = False
        self.assertIn("not admitted", self.failures(doctor)[0])

    def test_missing_capabilities_check(self):
        self.assertEqual(len(self.failures({"checks": []})), 1)

    def test_cli_writes_evidence_and_exits_nonzero_on_failure(self):
        with tempfile.TemporaryDirectory() as tmp:
            good = Path(tmp) / "good.json"
            good.write_text(json.dumps(passing()))
            bad_doctor = passing()
            report_of(bad_doctor)["provided"]["termination:scope"]["provider"] = "host"
            bad = Path(tmp) / "bad.json"
            bad.write_text(json.dumps(bad_doctor))
            self.assertEqual(PROVE.main(["--report", str(good), "--out", str(Path(tmp) / "a")]), 0)
            self.assertEqual(PROVE.main(["--report", str(bad), "--out", str(Path(tmp) / "b")]), 1)
            evidence = json.loads((Path(tmp) / "b" / "oci-provider-evidence.json").read_text())
            self.assertEqual(len(evidence["failures"]), 1)


if __name__ == "__main__":
    unittest.main()

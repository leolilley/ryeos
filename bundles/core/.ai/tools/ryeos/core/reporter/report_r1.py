# ryeos:signed:2026-10-15T13:54:43Z:90c2669b2d7033650fa1d65a2ff038ce56347e637017149a044582ad79978bed:s8nCxWXY1ACezayVCX+bmGQWuGxOfVQ1cQj+w7PfhHfUqKi1JIbSWBch8c3PywkfbeBSNrvnXU3yeVLqnGlGBA==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea
# ryeos-tool:
#   category: ryeos/core/reporter
#   version: "1.1.0"
#   executor_id: "tool:ryeos/core/runtimes/python/function"
#   execution_protocol: "protocol:ryeos/core/tool_callback"
#   description: "Canonical report over one retained doctor result, bound to that result's identity"
"""Reporter r1: r0 plus per-check classification and actionable grouping.

Input (params):
  doctor_identity  object, required. Names the retained doctor result:
                   thread_id, tool_ref (must be tool:ryeos/core/node/doctor)
                   and result_sha256 (sha256 of the canonical JSON of the
                   doctor result).
  doctor_result    object, required. The doctor report: source, ok, checks.

The identity is bound to the result by digest. A missing field, a wrong
tool, or a digest that does not match the supplied result is refused by name
(ReportRefusal.code); the reporter never guesses which doctor run it was given.

Classification never upgrades a status: failing checks are findings,
warnings stay warnings, unknown checks stay advisory, and checks that could
not run stay unavailable. The verdict is "clean" only when every check
passed; any finding gives "findings_present", and an otherwise-passing report
with advisory/unavailable checks is "incomplete", never success.

The doctor verdict is carried through unchanged: ok=false and every failing
check stay findings, advisory and unavailable checks keep their status.
"""

import hashlib
import json

REVISION = "r1"
REPORTER_REF = "tool:ryeos/core/reporter/report_r1"
DOCTOR_TOOL_REF = "tool:ryeos/core/node/doctor"
REPORT_SCHEMA = "ryeos.doctor_report.v1"
IDENTITY_FIELDS = ("thread_id", "tool_ref", "result_sha256")


class ReportRefusal(Exception):
    """Raised with a stable machine-readable code as the first token."""

    def __init__(self, code, detail=""):
        super().__init__(f"{code}: {detail}" if detail else code)
        self.code = code
        self.detail = detail


def canonical_json(value):
    return json.dumps(
        value, sort_keys=True, separators=(",", ":"), ensure_ascii=False
    )


def sha256_of(value):
    return hashlib.sha256(canonical_json(value).encode("utf-8")).hexdigest()


def _bind_doctor(params):
    identity = params.get("doctor_identity")
    if not isinstance(identity, dict) or not identity:
        raise ReportRefusal("missing_doctor_identity")
    for field in IDENTITY_FIELDS:
        value = identity.get(field)
        if not isinstance(value, str) or not value:
            raise ReportRefusal("missing_doctor_identity_field", field)
    result = params.get("doctor_result")
    if not isinstance(result, dict) or not result:
        raise ReportRefusal("missing_doctor_result")
    if identity["tool_ref"] != DOCTOR_TOOL_REF:
        raise ReportRefusal("doctor_tool_mismatch", identity["tool_ref"])
    if not isinstance(result.get("ok"), bool) or not isinstance(
        result.get("checks"), list
    ):
        raise ReportRefusal("malformed_doctor_result")
    for check in result["checks"]:
        if (
            not isinstance(check, dict)
            or not isinstance(check.get("name"), str)
            or not isinstance(check.get("status"), str)
        ):
            raise ReportRefusal("malformed_doctor_result", "check entry")
    actual = sha256_of(result)
    if actual != identity["result_sha256"]:
        raise ReportRefusal(
            "doctor_identity_mismatch",
            f"supplied result hashes to {actual}",
        )
    return identity, result


def _seal(body):
    sealed = dict(body)
    sealed["report_sha256"] = sha256_of(body)
    return json.loads(canonical_json(sealed))


def classify(check):
    status = check["status"]
    detail = check.get("detail")
    if status == "fail":
        return "finding"
    if status == "warning":
        return "warning"
    if status == "ok":
        return "passed"
    if status == "unknown":
        return "advisory"
    if status == "n/a":
        if isinstance(detail, dict) and detail.get("import_check") == "unavailable":
            return "unavailable"
        return "not_applicable"
    return "unrecognized"


def _message(detail):
    if not isinstance(detail, dict):
        return None
    for key in ("error", "import_check_reason", "note"):
        value = detail.get(key)
        if isinstance(value, str) and value:
            return value
    return None


GROUP_ORDER = (
    "finding",
    "warning",
    "unavailable",
    "advisory",
    "unrecognized",
    "not_applicable",
    "passed",
)
# Groups a person must act on or look at before trusting the report.
ACTIONABLE = ("finding", "warning", "unavailable", "advisory", "unrecognized")


def _classify_body(body):
    groups = {g: [] for g in GROUP_ORDER}
    actions = []
    for c in body["checks"]:
        c["classification"] = classify(c)
        groups[c["classification"]].append(c["name"])
        if c["classification"] in ACTIONABLE:
            action = {
                "check": c["name"],
                "classification": c["classification"],
                "status": c["status"],
                "message": _message(c.get("detail")),
            }
            remedy = c["detail"].get("remedy") if isinstance(c.get("detail"), dict) else None
            if isinstance(remedy, str) and remedy:
                action["remedy"] = remedy
            actions.append(action)
    actions.sort(
        key=lambda a: (GROUP_ORDER.index(a["classification"]), a["check"])
    )
    body["groups"] = {g: names for g, names in groups.items() if names}
    body["actions"] = actions
    if groups["finding"]:
        body["verdict"] = "findings_present"
    elif any(groups[g] for g in ACTIONABLE):
        body["verdict"] = "incomplete"
    else:
        body["verdict"] = "clean"
    return body


def _base_body(identity, result):
    checks = sorted(
        (
            {
                "name": c["name"],
                "status": c["status"],
                "detail": c.get("detail"),
            }
            for c in result["checks"]
        ),
        key=lambda c: c["name"],
    )
    counts = {}
    for c in checks:
        counts[c["status"]] = counts.get(c["status"], 0) + 1
    return {
        "schema": REPORT_SCHEMA,
        "revision": REVISION,
        "reporter": REPORTER_REF,
        "provenance": {
            "doctor_identity": {f: identity[f] for f in IDENTITY_FIELDS},
            "doctor_source": result.get("source"),
        },
        "doctor_ok": result["ok"],
        "status_counts": counts,
        "checks": checks,
    }


def build_report(params):
    identity, result = _bind_doctor(params)
    return _seal(_classify_body(_base_body(identity, result)))


def execute(params, project_path):
    return build_report(params)

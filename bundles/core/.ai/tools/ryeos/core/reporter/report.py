# ryeos:signed:2026-10-15T13:54:43Z:61260ee51ed3a95cd09b0293293d7433a975f92f08cd7e43b74cc0bbe18f29e6:hH+kIktQdXeQU7NoMUteT88GSupMqPJqjaP+pPZG7dDZh8IWahmilcyJRq75MYytmJw+1+kJtYybLTJa1D/WCw==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea
# ryeos-tool:
#   category: ryeos/core/reporter
#   version: "1.0.0"
#   executor_id: "tool:ryeos/core/runtimes/python/function"
#   execution_protocol: "protocol:ryeos/core/tool_callback"
#   description: "Canonical report over one retained doctor result, bound to that result's identity"
"""Reporter r0: a stable canonical report that retains doctor provenance.

Input (params):
  doctor_identity  object, required. Names the retained doctor result:
                   thread_id, tool_ref (must be tool:ryeos/core/node/doctor)
                   and result_sha256 (sha256 of the canonical JSON of the
                   doctor result).
  doctor_result    object, required. The doctor report: source, ok, checks.

The identity is bound to the result by digest. A missing field, a wrong
tool, or a digest that does not match the supplied result is refused by name
(ReportRefusal.code); the reporter never guesses which doctor run it was given.

The doctor verdict is carried through unchanged: ok=false and every failing
check stay findings, advisory and unavailable checks keep their status.
"""

import hashlib
import json

REVISION = "r0"
REPORTER_REF = "tool:ryeos/core/reporter/report"
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
    return _seal(_base_body(identity, result))


def execute(params, project_path):
    return build_report(params)

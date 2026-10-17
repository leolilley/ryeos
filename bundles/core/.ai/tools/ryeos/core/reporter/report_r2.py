# ryeos:signed:2026-10-15T13:54:43Z:1f3524e9c2cd4aea5e2a7e889b671ee78fc8bf34bfb79be0609ff2070d014364:RnJIduB6B/xwwFR30+DAPX7MQbWyF4kfKBVd98gf2IAN/0E6AA9Pxx+RUafBoa9yMt99Sxyn1hVdqRE7I4bpBw==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea
# ryeos-tool:
#   category: ryeos/core/reporter
#   version: "1.2.0"
#   executor_id: "tool:ryeos/core/runtimes/python/function"
#   execution_protocol: "protocol:ryeos/core/tool_callback"
#   description: "Canonical report over one retained doctor result, bound to that result's identity"
"""Reporter r2: r1 plus a comparison against an explicitly supplied prior report.

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

Additional input: prior_report, required, a report produced by any reporter
revision. Its own report_sha256 must match its content (prior_report_digest_mismatch
otherwise); a missing or malformed one is refused by name. The report lists
checks added, removed, whose status changed and whose detail changed since
the prior report. Nothing is read implicitly: no "latest" lookup.

The doctor verdict is carried through unchanged: ok=false and every failing
check stay findings, advisory and unavailable checks keep their status.
"""

import hashlib
import json

REVISION = "r2"
REPORTER_REF = "tool:ryeos/core/reporter/report_r2"
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


def _validate_prior(params):
    prior = params.get("prior_report")
    if not isinstance(prior, dict) or not prior:
        raise ReportRefusal("missing_prior_report")
    if prior.get("schema") != REPORT_SCHEMA or not isinstance(
        prior.get("checks"), list
    ):
        raise ReportRefusal("malformed_prior_report")
    for c in prior["checks"]:
        if (
            not isinstance(c, dict)
            or not isinstance(c.get("name"), str)
            or not isinstance(c.get("status"), str)
        ):
            raise ReportRefusal("malformed_prior_report", "check entry")
    claimed = prior.get("report_sha256")
    unsealed = {k: v for k, v in prior.items() if k != "report_sha256"}
    if not isinstance(claimed, str) or sha256_of(unsealed) != claimed:
        raise ReportRefusal("prior_report_digest_mismatch")
    return prior


def _compare(prior, body):
    before = {c["name"]: c for c in prior["checks"]}
    after = {c["name"]: c for c in body["checks"]}
    added = sorted(set(after) - set(before))
    removed = sorted(set(before) - set(after))
    status_changed = []
    detail_changed = []
    unchanged = 0
    for name in sorted(set(before) & set(after)):
        b, a = before[name], after[name]
        changed = False
        if b["status"] != a["status"]:
            status_changed.append(
                {"check": name, "from": b["status"], "to": a["status"]}
            )
            changed = True
        if sha256_of(b.get("detail")) != sha256_of(a.get("detail")):
            detail_changed.append(
                {
                    "check": name,
                    "from_sha256": sha256_of(b.get("detail")),
                    "to_sha256": sha256_of(a.get("detail")),
                }
            )
            changed = True
        if not changed:
            unchanged += 1
    prior_identity = (prior.get("provenance") or {}).get("doctor_identity")
    return {
        "prior_report_sha256": prior["report_sha256"],
        "prior_revision": prior.get("revision"),
        "prior_doctor_identity": prior_identity,
        "same_doctor_result": isinstance(prior_identity, dict)
        and prior_identity.get("result_sha256")
        == body["provenance"]["doctor_identity"]["result_sha256"],
        "doctor_ok_changed": prior.get("doctor_ok") != body["doctor_ok"],
        "added": added,
        "removed": removed,
        "status_changed": status_changed,
        "detail_changed": detail_changed,
        "unchanged_count": unchanged,
        "has_changes": bool(
            added or removed or status_changed or detail_changed
            or prior.get("doctor_ok") != body["doctor_ok"]
        ),
    }


def build_report(params):
    identity, result = _bind_doctor(params)
    prior = _validate_prior(params)
    body = _classify_body(_base_body(identity, result))
    body["comparison"] = _compare(prior, body)
    return _seal(body)


def execute(params, project_path):
    return build_report(params)

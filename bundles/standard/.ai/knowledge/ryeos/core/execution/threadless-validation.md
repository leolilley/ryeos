<!-- ryeos:signed:2026-09-07T09:01:08Z:47ddd06b5d4b6509cba7010b9e5d5a38b6feef95c44311833c8e21d61058b0e9:SquxhDq2NeRLCChxVx7sBGFJCqp28YFQEH9ffL/R8dFO4NQ2GBwwCZ0KbcHnDrnAk1yUX2CjUYAJaqKED663AQ==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
---
category: ryeos/core/execution
tags: [execution, validation, admission, snapshots]
version: "1.0.0"
description: Threadless executable preflight against live or pinned project authority.
---

# Threadless validation

`ryeos validate <canonical-ref>` uses ordinary executable admission without
starting the selected workload. It is distinct from signature-only inspection
and from an actual execution qualification.

Use `ryeos --project /absolute/project validate <canonical-ref> --current-head
--no-operator-vault` to inspect the existing immutable project HEAD and its
exact target-local content bindings. This does not capture current live edits.
The current HEAD must already exist under the caller's project authority.
Normal typed inputs and ref bindings still apply; control flags are not item
parameters.

The execution entry point retains the selected snapshot, project trust view,
request engine and root admission. For pinned validation it borrows the
read-only materialization of that exact generation, even when the requested
launch policy calls for CoW execution. There is no execution workspace to
reserve, attach or terminalize. The requested policy still participates in
admission; this inspection is not evidence that its isolation backend has
successfully launched a workload.

Validation can resolve source and populate leased immutable caches. It does
not invoke the workload handler, create an execution thread, acquire a worker
session, publish a terminal candidate or advance project HEAD. Managed launch
preparers may run their bounded preparation protocol; they are not the target
workload. Readiness fields must be read separately from `validated`: unresolved
credentials or missing content can leave admission not ready.

Validation is not an admission lease. Real execution rechecks current policy,
bindings, credentials and lifecycle authority. A previous successful response
cannot authorize a later launch after those facts change. Accepted asynchronous
validation is refused. Validation with a different target site is also refused:
local checks do not prove remote readiness.

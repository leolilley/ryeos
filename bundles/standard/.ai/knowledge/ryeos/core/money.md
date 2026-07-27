<!-- ryeos:signed:2026-07-27T01:32:30Z:ba8b560657d522792414780b2890c662d3a0fdfcaf100c1a4eb5e9760f5deaa4:zdHVVeZPgMDnSSbSuzMFvP11NsUE7pZN982DoDzATdd1g5rpzC1OtQYgG6sVMPXLNV8MG14leWYI4cTyz+zgAw==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
---
category: ryeos/core
tags: [money, accounting, cost, usd, settlement, wire-contract]
version: "1.0.0"
description: >
  The one money representation — exact fixed-point USD nanos as canonical
  decimal strings on every cost wire — and the entry, comparison, and display
  rules that follow from it.
---

# Money

RyeOS has exactly one money representation: **`UsdNanos`** — non-negative
fixed-point USD in integer nanos (`1 USD = 1_000_000_000` nanos), defined in
`ryeos-accounting::money`. Every cost-bearing wire and persisted record uses
it: `RuntimeCost.total_usd` (envelopes, stdout results, node receipts, graph
checkpoint accounting), `FinalCost.spend` (terminal settlement), the finalize
callback cost, and `ThreadUsage.spend_usd` (the signed thread snapshot and
`thread_usage` events). There is no float money type anywhere on a wire.

## Wire form

Money serializes as a **canonical decimal string** — `"0.03"`, `"12.5"`, `"0"`
— never a JSON number. Decode rejects JSON numbers outright, on every surface.
The reason is identity, not style: a lossy `f64` decoded from JSON is not
guaranteed to be the exact value that was encoded (serde's float parse can land
one ULP off), so two independently serialized copies of the *same* cost could
compare unequal. That representation drift once forced a tolerance band into
the graph rollup validator. Strings are byte-stable through any decode/encode
cycle — a follow splice copying a checkpoint through `serde_json::Value`
cannot perturb a cost — so **exact `==` is the correct comparison everywhere**,
with no epsilon and no tolerance.

Canonical strings carry no sign, exponent, or padding zero, and at most nine
fractional digits. More than nine digits is rejected, never silently rounded
(`MoneyError::ExcessScale`). Rendering is minimal (no trailing fractional
zeros); decode also accepts trailing zeros within the nine-digit scale
(`"0.50"`, `"0.0"`) — they are exact, and comparisons are on parsed nanos, not
bytes.

## Arithmetic

All arithmetic is checked integer-nano arithmetic: `checked_add`,
`checked_sub` (which refuses to go negative), and rate × quantity via
`rate_per_million_mul_units_round_up` computed in `i128`. Accumulation order
cannot change a rollup total, so an independently recomputed rollup compares
with plain equality — this is what lets checkpoint resume validate
`total == Σ node/hook costs` exactly.

## Entry boundary

Floats exist in exactly one place: where a **provider adapter** reports spend
as a parsed JSON number. That figure crosses into the exact domain once, where
it is first read into a cost, via `UsdNanos::quantize_reported_f64_round_up` —
the float's shortest round-trip decimal is quantized at nano precision,
rounding toward positive infinity (sub-nano residue rounds up, never
truncating spend). Rate-derived spend never touches a float at all: rate ×
tokens runs in checked `i128` nanos via `rate_per_million_mul_units_round_up`.
Provider-reported raw decimal *text* (including exponent forms like `2.25e-5`)
goes through `parse_reported_round_up` under the route's signed final-charge
scale contract. Internal code must never round-trip authority money through a
float.

## Display boundary

`display_usd_lossy()` is the one-way exit: a presentation float for UI event
payloads, hook-condition contexts, log lines, and the SQLite projection's
`spend_usd` REAL column (a rebuildable display cache — the exact figure lives
in the signed snapshot). Never parse a display value back into authority.

## Settlement cross-checks

Terminal settlement cross-checks the cost from independent carriers — the
signed authority snapshot, the managed envelope, and the runtime's stdout
result — with exact equality on `UsdNanos`. A mismatch is a real contradiction
and fails the settlement closed; there is no "close enough" for money.

## Contract drift

Money that arrives as a JSON number does not decode, on any surface — the
graph checkpoint contract (v3) pins this the same way it pins the expression
language, so a checkpoint carrying numeric money fails resume with
`restart_required` rather than being migrated.

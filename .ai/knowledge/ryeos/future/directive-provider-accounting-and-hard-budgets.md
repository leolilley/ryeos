# Directive provider accounting and hard budgets

## Status

Authoritative design and ownership note for the provider budget
reservation/reconciliation substrate. The core landed on the
`provider-budget-reservation` branch: fixed-point USD authority, sealed
per-launch financial authority, the daemon accounting ledger
(`accounting.sqlite3` + external financial anchor), the
reserve → issue → settle attempt lifecycle, terminal fencing, the strict
`provider_attempt_budget_transition_v1` audit event and its projection, and
the `service:threads/accounting/summary` surface. This document owns the
attempt ledger, the hard-spend guarantee, and the reconciliation state
machine; deferred extensions are listed at the end.

## The guarantee boundary

RyeOS provides a hard external-spend guarantee only where the resolved
accounting authority mechanically proves one of:

1. `ProviderEnforcedChargeCap` — the provider contract and the exact emitted
   request impose a server-enforced maximum total USD charge; or
2. `DerivedWorstCaseCharge` — a conservative maximum derived from the frozen
   request, bounded input/output units (context window and the provider-native
   output ceiling), and a signed tariff covering every applicable billable
   dimension.

An operator-signed number without one of those proofs is a declared admission
bound (`AdvisoryOnly`): usable for settled-mode reporting, ineligible for hard
spend, and rejected at launch when a finite hard `spend_usd` limit is
configured. A settled post-attempt threshold is never described as hard.

Authoritative money is `UsdNanos` fixed point (1 USD = 1e9 nanos), entering
only as canonical decimal strings or integer nanos. Presentation `f64` values
are one-way derived and never parse back into authority.

## Ownership map

- **This document** owns: budget accounts and scope identities
  (`execution_budget_id` / `directive_budget_id`), the reservation state
  machine (`ReservationDenied`, `Reserved`, `Issued`, `Reconciled`,
  `ReleasedUnissued`, `ChargedReservedMaximum`,
  `ReservationBoundViolated`), spend-bound certificates and tariff/cap
  contract content, the ledger + external anchor durability model, the
  transactional audit outbox, and hard-eligibility rules.
- `future/wandr-research-agent.md` consumes execution/directive budgets but
  does not redefine them.
- `future/scheduler-deferred-advanced-work.md` owns fire identity and
  recovery propagation; each fire gets a fresh execution budget account
  unless a future schedule quota explicitly creates a shared outer account.
- `future/distributed-substrate-deferred-advanced.md` records that paid
  descendants remain accounting-authority-node affine until distributed
  reservation consensus exists; remote paid fan-out is rejected before child
  admission.
- `future/hosted-node-trust-boundaries.md` and
  `future/ryeos-ui-local-project-registry-and-multitenancy.md` later add
  principal/organization account kinds; the account-kind enum stays closed
  until their admission authority exists.
- `future/node-operations.md` consumes the durable health/query surface
  (`service:threads/accounting/summary`, node-status accounting health) and
  defines deployment paging policy.
- `future/signed-envelope-v2-authenticated-metadata.md` owns authenticated
  metadata evolution; this document owns the `financial_authority` payload
  semantics carried through the launch contract.
- `future/data-driven-scope-profiles.md` may later select financial account
  sets but cannot bypass the daemon ledger.
- Tool-runtime authority documents remain the home of CPU/memory/process/
  storage quotas — those are not financial reservations.

## Token limits are a distinct guarantee

Cumulative `limits.tokens` remains a settled provider-usage threshold. A
genuinely hard total-token budget requires a trustworthy local tokenizer or a
provider-neutral preflight count contract; until a route has one, hard USD
ships independently wherever its spend certificate proves a charge cap. The
future local-tokenizer document should treat hard token-account reservations
as its primary acceptance consumer. The per-turn provider-native output limit
and the exact local stream-byte backstop remain separate controls.

## Deferred after the current landing

- principal billing/invoicing and organization-wide hosted quotas;
- cross-node distributed reservation consensus;
- provider-specific charge lookup unless expressed through a generic signed
  reconciliation contract;
- automatic live-price discovery as hard authority;
- request-specific reservation discounts below the sealed route maximum
  (requires a daemon-verifiable proof format);
- refund/adjustment policy for late provider credits;
- hard provider-native token budgets on routes without a trustworthy
  preflight bound;
- a general Prometheus/OpenTelemetry exporter over the durable summary
  surface.

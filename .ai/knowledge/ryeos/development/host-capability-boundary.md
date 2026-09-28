<!-- ryeos:signed:2026-09-22T00:31:25Z:74df10c383075a6abf639ddd2845d4b9825f27a7fa7192b3659aa97f5656dda9:lSm1Gak+hBQqaewKywCApWRG81Jmols5x3804I7F09UBo7nUBMEE3V/keykZdA6dBx73Y2NbQ0cO3UnWnlt7BQ==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
```yaml
category: ryeos/development
name: host-capability-boundary
title: Host Capability Ownership and Review
description: Complete Lillux host ownership, protocol boundaries, migration gates and regression review
entry_type: reference
version: "1.0.0"
```

# Host Capability Ownership and Review

This guide governs repository changes that touch the host. Read it with
[architecture](architecture.md) and the
[dependency constitution](dependency-constitution.md) before introducing or
migrating execution, filesystem, identity, time, waiting or communication code.
These are implementation requirements. Requirements below do not assert that
all existing callers have completed migration.

## Complete operation ownership

Lillux owns host capabilities and their platform implementations, including
preparation, resource acquisition, active observation, interruption, settlement
and release. RyeOS supplies admitted intent, composes these capabilities and
interprets their observations through its protocols and durable workflows.

For example, a helper that mutates a caller-owned `Command` to configure pipes
fixes those descriptor mechanics. It does not complete the process boundary
while the caller still spawns, retains `Child`, waits, signals and reaps. Review
the complete lifecycle, including failure paths and library-internal effects.
Portable standard-library APIs, diagnostic code and small implementations do
not change ownership.

| Concern | Owner |
|---|---|
| Select admitted executable/content, arguments, environment values, cwd, limits and semantic cancellation | RyeOS policy and protocol owner |
| Apply the launch request, environment clearing, cwd, stdio and descriptor inheritance; spawn and retain process authority | Lillux |
| Host pipes, bounded reads/writes, backpressure, child handles, process membership, signals, wait/reap and cleanup observations | Lillux |
| JSON-RPC framing, provider requests, HTTP messages, HTML processing and provider configuration | Feature/protocol owner |
| Host thread creation/join, clock sampling, sleeps and timed waits | Lillux; RyeOS selects work, budgets and expiry consequences |
| Socket creation/connection/listening, resolver integration and host transport timing | Lillux host integration; protocol owner selects endpoints and trust policy |
| Filesystem access, descriptor adoption, host identity and durability operations | Lillux; RyeOS owns schemas, reachability, transaction and recovery meaning |
| Duration values, path values, byte buffers, Arc and ordinary in-memory state | Consumer |

Standard I/O traits may describe a Lillux-supplied stream. A capability must keep
the intended lifetime and authority constraints; exporting a raw handle for the
caller to rebuild its own process supervisor does not establish this boundary.

Third-party protocol libraries require explicit review of hidden host effects:
connection establishment, DNS, clocks/timeouts, filesystem access, ambient
configuration, trust stores and credentials. Record how the integration meets
the host contract and any unresolved gap. Moving HTML, HTTP or provider semantics
into Lillux is not the remedy. A socket wrapper also proves nothing about
timeouts, credential access or cleanup beyond its implemented guarantees.

## Process contracts and evidence

Lillux is the host implementation owner. Buffered, interactive, attached and
inherited-stdio execution may require different typed states and capabilities.
Share appropriate mechanisms without forcing distinct authority contracts into
one request type or maintaining separate feature-owned process supervisors.

Before migrating a caller, preserve and test:

- Executable and filesystem authority lifelines, argv0, explicit environment,
  descriptor inheritance and owner-private creation policy.
- Process placement and enclosing ownership. An interactive child inside an
  existing scope must not acquire a new process group accidentally. Conversely,
  preserving its group membership must not give child cleanup permission to
  signal the shared outer group. Removing `setsid` from an owned-group runner
  alone is insufficient.
- Cooperative exact-child cancellation, escalation authority and full descendant
  cleanup as distinct contracts. The owner selecting a semantic cancellation
  policy remains above Lillux; the exact host action and observation stay inside.
- Active deadline, cancellation and output-limit enforcement during interaction,
  including blocked reads/writes. Waiting for `wait()` to begin supervision can
  deadlock an interactive exchange. Partial progress must not renew an absolute
  deadline. Streaming and cumulative capture limits must be explicit.
- Exactly one settlement owner through every partial-start and error path.
  Input closure, bounded output draining and observer shutdown are explicit.
  Drop behavior must have stated guarantees; attempted cleanup cannot be
  reported as established death.
- Bounded private diagnostics. RyeOS decides the audience and disclosure policy;
  provider stderr may contain credentials and cannot be forwarded into public
  responses by default. EOF on stdout does not prove final stderr has drained.

Keep these observations distinct: protocol success, stream EOF, task completion,
child exit, child reap, descendant settlement, durable command completion,
completion fence and filesystem writer exclusion. A capability may attest only
what it observes. In particular, a child exit or thread join cannot authorize
candidate freeze, release a durable uncertainty fence or prove all writers dead.

## History and existing incomplete boundaries

Workspace creation must retain its Lillux directory authority through the
original owner handoff; returning only a layout path discards that authority.
Fresh creation must not adopt an existing inode. A durable reservation records
intent, not permission to reopen a constructing directory as a new candidate.
After reservation, the retained guard preserves journal-owned state for explicit
cleanup. An available trusted/disabled workspace may lend exact backing-directory
access after journal authorization, but that descriptor is not an isolation view
or proof of process death. Disarmed, closing, borrowed and path-only owners must
refuse that transfer. Cold recovery instead derives a new incarnation from its
existing journal, identity and predecessor-settlement proof; it cannot pretend
the original process's descriptor survived restart.

Commit `1c19de1e0` added `configure_command_piped_stdio` to preserve fresh pipes
through exec after adopting a control channel vacated descriptor zero. The
structured-session caller retained `Command`, child handles and lifecycle
control. Preserve the descriptor fix and its regression test when completing
that ownership boundary. Existing use is not a new-code exemption.

The next migration must inventory the existing Lillux runner, attachment and
cooperative-child contracts and the structured-session caller together. Migrate
product consumers and executable qualification fixtures to the reviewed host
capability; retire obsolete raw-command adapters once their consumers move.
Do not copy the old split into another fixture or add an independent supervisor
solely to get one acceptance test past a failure.

Git history establishes what existed and why. It does not override the current
ownership contract. A historical search must state its scope; an absent symbol
does not establish that all equivalent capabilities were searched.

## Required review record and gates

For each changed host operation, record its caller, existing capability, proposed
host owner, protocol owner, placement, blocking behavior, cleanup guarantee and
decisive test. Include tracked changes, new files, moved code and affected
consumers. Search imports and aliases to find candidates, then trace the actual
operation and its lifecycle. Review complete dependencies for hidden host effects.

Product code, executable qualification fixtures, test harness setup and pure
computation have different roles. A fixture implementing product behavior follows
the same host contract. A harness may deliberately create invalid host state to
test refusal, but its purpose and affected operation must be explicit. Such code
cannot qualify a product path it bypasses, and `tests/` is not a blanket exemption.

Migration acceptance requires:

1. A reviewed operation/ownership map, including changes to placement or cleanup
   semantics. Record remaining violations instead of treating a partial migration
   as completed boundary compliance.
2. Focused tests for normal interaction, vacant standard descriptors, inherited
   descriptor aliasing, partial startup failure, blocked input/output, silent
   peers, partial progress, overflow, early EOF, stderr-drain races, cancellation
   and descendants retaining pipes or writer access.
3. Separate evidence for exact-child cancellation and enclosing-scope cleanup,
   with protocol/freeze tests proving those observations are not substituted.
4. Regression checks through the existing repository-validation framework for
   the migrated owner set. Include direct and aliased host calls and executable
   fixtures. Any exception names its operation and justification. Source scanning
   is a tripwire, not a proof of ownership or security.
5. Architecture and lifecycle review, plus qualification of the actual product
   call path. Compile success and historical test evidence cannot qualify a new
   process ownership model.

The current dependency-layer validator checks dependency closure and forbidden
owners. It does not enforce these host-operation requirements. Operation checks
must be implemented and tested as part of the migration before claiming that
regression prevention is in place. Update installed knowledge only for behavior
actually implemented and qualified.

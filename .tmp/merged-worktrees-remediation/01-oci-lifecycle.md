# OCI lifecycle implementation: F01–F04

Status: source implementation complete; focused tests pass; privileged installed qualification remains open. Parent plan: [README](README.md). Evidence: original review F01–F04.

## Implemented result — 2026-09-18

- Lillux now owns an exact process-root primitive which fences the process incarnation and follows only the controlled procfs root magic link; descendant traversal remains descriptor-relative and no-follow.
- Administrator validation and non-root controller opening are separate operations. The selected account comes from retained generation evidence; root does not bypass the controller-owner contract.
- The OCI bootstrap path adopts the exact already-prepared controller root instead of reusing native child-delegation assumptions.
- Retirement is bounded, writer-excluding, identity-checked and bottom-up. A generation journal makes partial deletion/retry idempotent.
- Intent-only crash recovery never mutates a same-named replacement when the original generation identity was not durably recorded. It quarantines/refuses that ambiguity; generation-bearing recovery remains exact.

Focused evidence is recorded in [07 Implementation evidence](07-implementation-evidence.md). The ordinary test host did not run the root credential-transition, writable delegated-cgroup or complete hook→bootstrap→workload→poststop journey, so those remain qualification gates rather than skipped successes.

## Scope and code ownership

Primary files: `crates/host-adapters/lillux-oci-hook/src/main.rs`; `crates/kernel/lillux/src/{secure_fs.rs,process_control/{cgroup.rs,scope.rs,oci_lifecycle.rs}}`; `crates/daemon/ryeos-node/src/host_runtime.rs`.

Consumers and evidence: contained profile under `bundles/.ai/node/init/profiles/`, `Dockerfile.release`, `images/contained-workflow/entrypoint.sh`, `tests/development/*oci*.py`, `tests/e2e/contained-workflow/`, and the hosted-OCI knowledge/config/verifier definitions.

Required invariant: an administrator observes an exact OCI init lifetime C and prepares R as its strict physical cgroup descendant. A protected binding names that generation, selected non-root account, node and app root. The unprivileged controller can use only that authority. Reuse requires exact old-lifetime death and descendant cleanup.

## F01: explicit exact-process-root access

Problem: generic `PinnedDirectory::open` correctly rejects every symlink component, but the hook passes paths containing `/proc/<pid>/root`. Do not change its default security contract.

Implementation design:

1. Add or reuse a Lillux-owned operation that retains the exact process identity (birth/boot and protected process handle), pins its proc directory, and opens the process-root reference through that controlled procfs interface. This is an explicit permitted procfs magic-link operation, not an arbitrary pathname-following flag available to callers.
2. Validate process liveness/incarnation before and after opening; retain the root directory descriptor and required namespace identities. A PID number alone is insufficient. Inspect whether existing exact-process primitives already provide the necessary handles before adding a second identity representation.
3. Traverse `data/app`, `run/ryeos`, and the cgroup mount destination from the pinned process-root descriptor using existing bounded no-follow child traversal. Reject `..`, symlink descendants and changed identities.
4. Update both hook prestart and controller mount installation. Retain all owners until the final operation finishes; never return a bare descriptor path backed by a temporary owner.
5. Keep mount/namespace syscalls in Lillux. Host adapter orchestrates the journal and calls those primitives; daemon consumes the completed protected binding.

Tests: actual process-root acquisition; exact descendant directory identity; symlinked app/binding descendants; init exits or changes between observations; wrong process generation; denied permissions; root/mount namespace replacement. Failure must leave recoverable intent, not publish an active binding. Use the real Rust primitive; a Python flag demonstration is only a regression explanation.

## F02: distinct administrator and controller validation

Problem: root bootstrap opens a UID10001 delegation using an API that requires owner UID equal to current euid.

Define two explicit operations, with names selected during implementation:

- Administrator-side generation validation: requires administrator authority; compares delegation ownership to the selected validated controller account; checks physical scope/generation/app-root/node bindings without pretending root is the controller.
- Controller-side provider opening: after irreversible credential transition, requires the current account to match the retained controller account and delegation. Existing normal non-root ownership checks remain strict.

Do not add `if root { skip ownership }`. Expected UID/GID must come from the validated binding, not a browser request, environment variable or untrusted OCI annotation. Keep protected binding validation before execution and repeat the relevant provider checks after the account transition. Audit supplementary groups, inherited descriptors and executable/cwd authority at the transition using existing Lillux account primitives.

Tests: correct root→10001 sequence; wrong UID/GID; group/world writable delegation; forged account/binding; root incorrectly entering normal controller opener; selected user attempting administrator operation; stale generation. Assert a launched probe's actual credentials, not only a constructed command configuration.

## F03: adopt an already prepared delegation without native-parent assumptions

Problem: OCI exposes R as `/sys/fs/cgroup`; native provisioning opens its enclosing `/sys/fs` and expects cgroup2.

Preferred design: separate native creation of a child delegation from adoption/bootstrap of an already prepared exact root. Both should share lower-level validation/controller-leaf placement, but have distinct authority prerequisites. The OCI path must use the retained generation and pinned R descriptor, not infer trust from the literal mount path. Native callers must continue validating their administrator-owned cgroup parent.

Inspect the existing configuration format before implementation. If topology mode is added to serialized scope configuration, version it explicitly and update all producers, consumers, signed profile/schema fixtures and recovery tests atomically. Alternatively pass typed preparation evidence through an existing non-wire bootstrap path if that fully preserves restart validation. Do not decide topology by probing and falling back after a failed security check.

Prove that the installed R is the same object observed under C on the host, even though the container sees a different mount-root pathname. Create/place the controller leaf only beneath that exact root. Preserve control-file ownership and prevent controller authority over its own ancestor lifecycle controls.

Tests: cgroup2 mounted directly at container mount root; native child-delegation regression; wrong mount/object; read-only mount; replaced mount; host-root selection; physical ancestry disagreement; absent expected controllers; correct readiness probe after adoption. Run hook output through `ryeosd host-runtime`, not a hand-authored substitute config.

## F04: exact bottom-up retirement

Preferred design: Lillux performs bounded, descriptor-relative retirement of a proved-ended exact controller subtree. The host adapter keeps the lease/journal state machine.

1. Validate retained boot/init/scope generation and required exact death proof.
2. Pin the relevant roots and verify recursive emptiness. Bound traversal depth/count and reject unexpected filesystem/mount transitions, replacement or ownership ambiguity.
3. Walk permitted descendants bottom-up; check identity and emptiness before removing each exact child. Do not follow symlinks, traverse siblings or invoke broad recursive pathname deletion.
4. Recheck as needed against concurrent membership/directory mutation; any population or ambiguous change blocks release. Determine the writer-exclusion/freeze authority needed for this proof from the existing topology rather than assuming an emptiness read is atomic.
5. Remove R only after children are gone. If the runtime already removed the exact tree, reconcile absence against retained lifetime evidence; absence of an arbitrary path is not proof that a substituted tree was cleaned.
6. Advance the existing release journal/indices only after proof. A crash after deleting some children must be idempotently recoverable. Never mark Released in a `finally` block after partial failure.

Tests: empty controller leaf; nested empty execution scopes; populated descendant; new member during retirement; wrong/replaced scope; unrelated sibling survives; cuts before/after each removal and each durable release write; repeated poststop; recover after runtime has removed C; unreleased volume remains blocked on every ambiguous outcome.

## Integrated acceptance and rollout

Required sequence on a named disposable Linux host: initialize exact account/app-root/policy → invoke real prestart → exec actual contained entrypoint → validate root binding → drop credentials → disposable placement/freeze/kill readiness → execute scoped work → daemon restart within C → stop → recover dead hierarchy → release lease → create fresh C and binding → reuse volume. Include negative replacements at each identity boundary.

Keep structural Python tests, but supplement them with implementation-coupled Rust tests and an installed hook/bootstrap test. Green source-string checks cannot close these findings. Record image/source/hook digests, account, boot/init birth, physical C/R identities and policy generation. No installed claim until the evidence verifier authenticates the configured attestor.

Existing interrupted records: recover under their exact retained format and meaning. If a new format is necessary, provide an explicit upgrade/refusal path; do not discard intent because the old code could not finish startup. Do not promote the contained image or change external-provider cleanup requirements merely because startup now succeeds.

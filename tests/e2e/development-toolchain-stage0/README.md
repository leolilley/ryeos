# Stage-0 offline behavior fixture

This fixture belongs to E2E qualification, not compiler production. Its move
here preserves the existing source bytes and does not reproduce or rewrite any
historical platform archive, producer testimony, inventory or result receipt.
The strengthened nonshared-link probe still needs a fresh complete publisher
run; the separate installed Cargo evidence is in `../development-cargo/`.

This tiny, dependency-free Rust workspace exercises a build script, generated
source, a procedural macro, native C compilation/archive/linkage, a Rust test
and a child executable. Its C file is test input, not a RyeOS/compiler adapter.

After full artifact verification/materialization, run the exact publisher with
network disabled, the platform read-only at `/ryeos/realizations/platform`, and
this directory read-only at `/fixture`. Invoke `/bin/bash /fixture/probe.sh`.
It writes only to that disposable container's private `/tmp` and does not need
the repository, installed node, credentials, shared Cargo cache or host compiler.

This is authoring-side behavior evidence, **not Lillux isolation acceptance**.
The same cases must later pass through admitted project Tools and the actual
target-local binding before hosted development is qualified. Do not install
this probe as a worker-side command dispatcher or add it to release test gates.

The adjacent `test-ownership.sh`, `test-runtime.sh` and `test-artifact.sh` own
focused source and artifact qualification. The ownership test proves that only
the external acquisition entry contains network transport and that the
publisher calls the Tool-adjacent offline producer. The other tests call the canonical runtime verifier at
`.ai/tools/ryeos/development/stage0-platform-production/lib/runtime.sh` and the
bootstrap-artifact verifier in that same library. Tests live here; reusable
verification behavior does not.

`test-bootstrap.py` checks the Tool-owned bootstrap export handoff: bounded
regular archive/checksum members, saved-source verification, destination
preservation and failure cleanup. It uses mocked subprocesses, not Docker,
Cargo or a live node:

```sh
python3 tests/e2e/development-toolchain-stage0/test-bootstrap.py
```

The bootstrap entry and usage belong to the Stage-0 tooling and the existing
`.ai/knowledge/ryeos/development/source-local-bundle-development.md` reference,
not a separate release-script workflow.

`test-artifact.sh` requires two independently produced archive/checksum pairs.
It does not download inputs, produce an archive, or treat a moved source file as
new reproduction evidence. Historical archives retain the producer digest they
were originally emitted with.

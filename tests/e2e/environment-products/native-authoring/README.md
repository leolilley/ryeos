# Native authoring runtime qualification fixture

The existing standard Bundle supplies one independent qualification policy and
direct Tool verifier for the retained `runtime` product of
`graph:ryeos/development/authoring-environment-production`. It uses RyeOS's
generic product composition and qualification owners; it adds no publication,
cache, recipe language, or manifest-discovery path.

The verifier Tool has exactly two root product slots. `authoring-runtime`
selects the final product at `/ryeos/realizations/authoring-tools`, while
`prepared-inputs` selects the existing preparation product at
`/ryeos/realizations/authoring-inputs`. Its small Bundle runtime uses the exact
pinned bootstrap CPython; there is no host Python, shell, loader, library
search, or Project `config_resolve` fallback. The general core Python runtime
is deliberately not reused because its local interpreter and execution Config
resolution are broader than this captured qualification contract.

The Tool reads the daemon-sealed `RYEOS_EXTERNAL_REALIZATIONS` projection for
the subject manifest coordinate. It does not accept a caller manifest and does
not trust the producer's receipt, inventory, provenance, or build-evidence
files. It independently:

- walks the mounted runtime under the product's entry/depth/byte ceilings and
  compares its observed entry and byte totals with the sealed realization;
- requires the exact finite 43-command set and normalized executable modes;
- verifies the exact size, mode and digest of the selected prepared-input
  `readelf`, loader and complete finite inspector library set before invoking
  them, rejects hardware-capability substitution, non-x86_64 objects, executable stacks,
  ambient interpreters/libraries, RPATH, missing exact RUNPATH and absent
  `NODEFLIB`, and proves every `DT_NEEDED` name exists in the selected runtime;
- runs representative selected `sed`, `grep`, `cp`, `cmp`, `find`, `sort`,
  `sha256sum`, `git`, `zsh`, `printf`, and `test` behavior with an exact PATH,
  isolated network authority, and private scratch; and
- returns only `authoring_runtime_closed` plus bounded counts and canonical
  digests of the observed inventory, ELF closure and behavior probe. It does
  not place the full inventory in qualification testimony.

The production relationship Config keeps runtime output identity dynamic. One
qualification-null relationship admits the verifier's subject slot; the
separate worker relationship requires this policy and claim. The development
worker Config now declares `authoring-tools` as that product slot instead of a
literal manifest. Authored definitions alone do not activate the worker: they
must be signed, and an operator must supply the exact current product and
qualification witness through ordinary complete composition. No old digest is
used as an implicit fallback.

Run the bounded source check with no daemon or build:

```sh
python3 -B tests/e2e/environment-products/native-authoring/check-fixture.py
python3 -B -m unittest discover -s tests/e2e/authoring-environment -p 'test_*.py'
```

For a real disposable acceptance, publish the changed standard and development
Bundle sources through their normal signing/population owners. Then use only
the current CLI owners and returned coordinates:

1. run/capture the prepared-input producer and retain its exact witness;
2. run/capture the built-utility and final environment producers with their
   complete root selection maps, retaining the final `runtime` witness;
3. launch `tool:ryeos/environments/qualification/native-authoring/verify` with both root
   witness hashes and explicit null qualification hashes, require a successful
   non-continued terminal, then qualify the runtime witness using relationship
   `runtime_to_authoring_worker` and the returned verifier root/thread;
4. compose `config:development/ryeos/worker-environment` with declaration
   `authoring-tools`, the exact runtime witness, and the returned qualification
   hash; and
5. verify the composed binding reports those exact hashes before any worker
   session is considered eligible.

Signing, Bundle installation, node lifecycle, product publication,
qualification publication, worker composition and worker execution are not
performed by this fixture. A source check is not a qualification pass.

## Boundary with the GNU Python product

This native-authoring fixture qualifies the finite command environment above;
it is not evidence for the separately produced GNU CPython runtime. That
runtime preserves a `python/` subtree below its selected product root and adds
only explicitly declared `DT_NEEDED` edges. Its ELF closure accepts an absent
provider `DT_SONAME` only when the exact admitted `python/lib/<DT_NEEDED>`
member resolves the request, while any present mismatching SONAME refuses.

The GNU qualifier does not trust the producer's ELF inventory or relocation
receipt as independent testimony. It executes the selected interpreter and
checks the actual mapped startup providers, normal dependency-name lookup and
the finite required versioned symbols. Those observations establish only the
declared built-in-zlib and startup-provider claims for that exact product; they
do not establish a universal native-extension ABI or library provenance.

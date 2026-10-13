# Static-link inputs profile B source fixture

This directory is an isolated source overlay for a disposable I-development
candidate. It does not alter the installed A profile. The four signed files
under `overlay/` replace the same relative paths in that candidate only:

- the development contract adds the pinned image's real `libmcheck.a` member;
- the Standard intent selects the new contract body;
- the Standard verifier policy independently expects all 12 members;
- the producer Tool pins the resulting B `large_content` realization.

The B export has 12 files, 18 manifest entries, 9,990,972 bytes, and manifest
SHA-256 `2d949d2079ffa86459ccba2960b73332893aa9adebb0fb65b1f9e4d0576b5ff1`.
`libmcheck.a` is a 1,360-byte ELF relocatable object in this image, despite
its `.a` suffix. This fixture makes a member-closure claim; actual native
linking is a separate consumer proof.

From the repository root, reproduce the transport tree from the cached,
digest-pinned image using the existing offline exporter. The destination must
be absent and Docker access must already be available:

```sh
python3 -B .ai/tools/ryeos/development/static-link-inputs/lib/bootstrap.py \
  --inputs tests/fixtures/static-link-inputs-b/overlay/.ai/config/development/ryeos/static-link-inputs.yaml \
  --output /tmp/ryeos-static-link-inputs-b \
  --docker-command docker
python3 -B tests/fixtures/static-link-inputs-b/check.py \
  --realization /tmp/ryeos-static-link-inputs-b
```

The exporter uses `--pull=never` and `--network none` and verifies every
exported member against the signed contract. The checker verifies the four
Ed25519 signatures against the checked-in development trust key, checks that
A remains the 11-member baseline, and derives both canonical manifest hashes.

Apply the four overlay paths explicitly to a separate candidate project before
admission. Import the verified B tree as `large_content` under a node-admitted
root, obtain the actual import receipt, and require its manifest hash to equal
the B pin before binding to that candidate's signed producer Tool. Capture a
new B product witness with its policy-bearing recipe, qualify it under the B
Standard policy, then bind that witness to `native-build` for the actual
consumer run. The fixture itself carries no import receipt, witness, binding,
or compiler-execution claim.

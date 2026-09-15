# Locked registry production qualification

Canonical selection, checksum verification and local-registry assembly belong
to `.ai/tools/ryeos/development/registry-production/lib/registry_inputs.py`.
The signed offline Tool and explicit first-bootstrap HTTPS entry call that same
implementation. Only the external entry owns curl/PyYAML, TLS/resolver and
network dependencies; its transport tests remain beside it.

```sh
python3 -B -m unittest discover -s tests/e2e/registry-production -p 'test_*.py'
python3 -B scripts/release/test-development-registry.py
```

Unit fixtures are synthetic, not acquisition or installed execution evidence.
Installed qualification must capture the signed Tool/Config and Cargo.lock,
bind the exact interpreter and retained registry input, and run the Tool with
captured filesystem, isolated networking and unchanged project HEAD. Compare
every selected archive and index row against the input, not only exit status.
The new registry-inputs.json receipt explicitly identifies retained-input
production; it cannot claim a new network acquisition or authorize binding.
Preserve historical acquisition archives and testimony unchanged.

Installed execution passed on 2026-09-07 for 406 locked packages. Independent
comparison of the imported input/result manifests matched all 1,108 payload
entries (771 files), including blob hashes and modes. Only the deliberately
new provenance receipt differs. `qualification.json` retains exact coordinates.
This does not imply a new acquisition, Cargo run or hosted-worker loop pass.

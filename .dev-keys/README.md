# Dev publisher keys

These keys are for **local development only**. They sign bundles that should
**never** be trusted in production.

The private key is intentionally public knowledge — it lives in version control.

- `PUBLISHER_DEV.pem` — private Ed25519 key (PKCS#8 PEM)
- `PUBLISHER_DEV_TRUST.toml` — publisher trust doc (fingerprint + pubkey)

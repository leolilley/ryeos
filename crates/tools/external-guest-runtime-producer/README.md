# External guest runtime producer

This source-product command is authored but not yet installed. Its signed Tool
captures the declaring Bundle's signed owner executable into the exact
`/ryeos/realizations/guest-owner-input` file realization. The production
graph obtains the current node's **public** `ed25519:<base64>` signing key
through `service:identity/public_key`; the Tool provides a canonical signed
owner profile and a retained private project workspace. The command never
reads an installed Bundle path,
provider credential, or Render API. It creates
`products/external-guest-owner-runtime` once and reports its observed content
manifest and owner executable digests.

The source bundles still need publication/installation, product capture and
independent Render snapshot qualification. A successful command or captured
product does not authorize allocation or activation; the snapshot must
separately prove it restored this exact runtime and controller root.

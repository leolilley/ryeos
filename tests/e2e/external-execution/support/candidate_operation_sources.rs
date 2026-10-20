//! Signed project-side operation sources (trust root and independent
//! evaluator) for the protocol tests' candidate flows.

use std::collections::BTreeMap;

use base64::{Engine as _, engine::general_purpose::STANDARD};
use lillux::crypto::EncodePublicKey as _;

pub fn signed_candidate_operation_sources(
    identity: &ryeos_node_runtime::identity::NodeIdentity,
) -> BTreeMap<String, (Vec<u8>, u32)> {
    let metadata = |description: &str, workspace_access: &str| {
        format!(
            "# ryeos-tool:\n#   category: test/external-candidate\n#   version: \"1.0.0\"\n#   description: {description}\n#   executor_id: tool:ryeos/core/runtimes/python/script\n#   execution_protocol: protocol:ryeos/core/opaque\n#   effects: live\n#   workspace_access: {workspace_access}\n#   filesystem_authority: node_policy\n#   network_authority: node_policy\n#   config_schema:\n#     type: object\n#     additionalProperties: true\n"
        )
    };
    let evaluator = format!(
        "{}{}",
        metadata(
            "Independently evaluate one exact candidate generation",
            "immutable_current_generation",
        ),
        r##"import json, sys
from pathlib import Path

request = json.loads(sys.stdin.buffer.read(65537))
project = Path(sys.argv[2])
candidate = request["candidate_snapshot_hash"]
base = request["base_snapshot_hash"]
accepted = candidate != base and (project / "candidate-strategy.txt").read_text() == "composed external candidate C\n"
if request.get("expect_integration"):
    integration = project / ".ai/knowledge/test/external-candidate/integration.md"
    accepted = accepted and integration.is_file() and "# Integration D" in integration.read_text()
print(json.dumps({"schema_version": 1, "candidate_snapshot_hash": candidate,
                  "base_snapshot_hash": base, "accepted": accepted,
                  "evidence": {"operation": "independent-test-evaluator"}},
                 sort_keys=True, separators=(",", ":")))
"##,
    );
    let signing_key = identity.signing_key();
    let fingerprint = identity.fingerprint();
    let public_der = identity.verifying_key().to_public_key_der().unwrap();
    let public_pem = format!(
        "-----BEGIN PUBLIC KEY-----\n{}\n-----END PUBLIC KEY-----\n",
        STANDARD.encode(public_der.as_bytes())
    );
    let trust = format!(
        "version = \"1.0.0\"\nfingerprint = \"{fingerprint}\"\nowner = \"external candidate E2E\"\nattestation = \"\"\n\n[public_key]\npem = \"\"\"\n{public_pem}\"\"\"\n"
    );
    BTreeMap::from([
        (
            ".ai/config/keys/trusted/e2e.toml".into(),
            (
                trust.into_bytes(),
                ryeos_state::objects::ProjectFile::REGULAR_MODE,
            ),
        ),
        (
            ".ai/tools/test/external-candidate/evaluate.py".into(),
            (
                lillux::signature::sign_content(&evaluator, signing_key, "#", None).into_bytes(),
                ryeos_state::objects::ProjectFile::REGULAR_MODE,
            ),
        ),
    ])
}

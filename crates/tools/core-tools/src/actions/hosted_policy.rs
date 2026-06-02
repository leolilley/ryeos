//! Hosted-node policy helpers for local operator tools.
//!
//! These helpers intentionally do not introduce a provider service or central
//! authority. They read the local node's installed hosted policy, if present,
//! so operator-side tools can align descriptor/admission behavior with the
//! node-level policy loaded by the daemon.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use ryeos_app::node_config::sections::hosted_node::{
    HostedNodePolicyRecord, HostedNodePolicySection,
};
use ryeos_app::node_config::NodeConfigSection;

/// Load the single installed hosted-node policy for `system_space_dir`.
///
/// Returns `Ok(None)` when no hosted policy is installed. Returns an error if
/// more than one policy is present because precedence/override semantics have
/// not been designed yet.
pub fn load_hosted_policy(system_space_dir: &Path) -> Result<Option<HostedNodePolicyRecord>> {
    let mut paths = candidate_policy_paths(system_space_dir)?;
    paths.sort();

    let mut records = Vec::new();
    let section = HostedNodePolicySection;
    for path in paths {
        let body = std::fs::read_to_string(&path)
            .with_context(|| format!("read hosted policy {}", path.display()))?;
        let value: serde_json::Value = serde_yaml::from_str(&body)
            .with_context(|| format!("parse hosted policy YAML {}", path.display()))?;
        let parsed = section
            .parse("policy", &value)
            .with_context(|| format!("validate hosted policy {}", path.display()))?;
        let mut record = parsed
            .as_any()
            .downcast_ref::<HostedNodePolicyRecord>()
            .context("HostedNodePolicySection::parse returned wrong type")?
            .clone();
        record.source_file = path;
        records.push(record);
    }

    match records.len() {
        0 => Ok(None),
        1 => Ok(records.pop()),
        _ => {
            let sources = records
                .iter()
                .map(|record| record.source_file.display().to_string())
                .collect::<Vec<_>>()
                .join(", ");
            bail!(
                "multiple hosted-node policies installed; refusing ambiguous hosted policy set: {}",
                sources
            )
        }
    }
}

fn candidate_policy_paths(system_space_dir: &Path) -> Result<Vec<PathBuf>> {
    let mut paths = Vec::new();
    let state_policy = system_space_dir
        .join(ryeos_engine::AI_DIR)
        .join("node")
        .join("hosted")
        .join("policy.yaml");
    if state_policy.is_file() {
        paths.push(state_policy);
    }

    let bundles_dir = system_space_dir.join(ryeos_engine::AI_DIR).join("bundles");
    if bundles_dir.is_dir() {
        let mut entries = std::fs::read_dir(&bundles_dir)
            .with_context(|| format!("read bundles dir {}", bundles_dir.display()))?
            .collect::<Result<Vec<_>, _>>()?;
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries {
            let policy = entry
                .path()
                .join(ryeos_engine::AI_DIR)
                .join("node")
                .join("hosted")
                .join("policy.yaml");
            if policy.is_file() {
                paths.push(policy);
            }
        }
    }

    Ok(paths)
}

#[cfg(test)]
mod tests {
    use super::*;

    const POLICY: &str = r#"
category: "hosted"
section: "hosted"
version: "0.1.0"
schema_version: "1.0.0"
description: "Default hosted-node operator policy for decentralized remote admission."
transport:
  public_https_required: true
  loopback_http_allowed: true
admission:
  mode: "one_time_token"
  token_ttl_secs: 600
  reject_wildcard_scopes: true
  token_delivery: "out_of_band"
descriptor:
  require_live_identity_match: true
  advertised_capabilities:
    - remote-execute
    - bundle-install
authorization:
  authority: "target_node_authorized_keys"
  central_bearer_tokens_allowed: false
  implicit_cross_node_authority_allowed: false
operations:
  audit_admission_events: true
  audit_grant_changes: true
  prefer_isolated_node_per_principal: true
  shared_daemon_multitenancy_enabled: false
"#;

    fn write_policy(path: &Path) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, POLICY).unwrap();
    }

    #[test]
    fn load_hosted_policy_reads_installed_bundle_policy() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp
            .path()
            .join(".ai/bundles/hosted-node/.ai/node/hosted/policy.yaml");
        write_policy(&path);

        let policy = load_hosted_policy(tmp.path())
            .unwrap()
            .expect("policy should load");

        assert_eq!(policy.admission.token_ttl_secs, 600);
        assert_eq!(
            policy.descriptor.advertised_capabilities,
            vec!["remote-execute".to_string(), "bundle-install".to_string()]
        );
        assert_eq!(policy.source_file, path);
    }

    #[test]
    fn load_hosted_policy_rejects_multiple_policies() {
        let tmp = tempfile::tempdir().unwrap();
        write_policy(&tmp.path().join(".ai/node/hosted/policy.yaml"));
        write_policy(
            &tmp.path()
                .join(".ai/bundles/hosted-node/.ai/node/hosted/policy.yaml"),
        );

        let err = load_hosted_policy(tmp.path()).unwrap_err();

        assert!(
            err.to_string().contains("multiple hosted-node policies"),
            "got: {err:#}"
        );
    }
}

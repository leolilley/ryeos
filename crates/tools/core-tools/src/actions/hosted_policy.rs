//! Hosted-node policy helpers for local operator tools.
//!
//! These helpers intentionally do not introduce a provider service or central
//! authority. They read the local node's installed hosted policy, if present,
//! so operator-side tools can align descriptor/admission behavior with the
//! node-level policy loaded by the daemon.

use std::path::Path;

use anyhow::{bail, Context, Result};
use ryeos_app::node_config::loader::BootstrapLoader;
use ryeos_app::node_config::sections::hosted_node::HostedNodePolicyRecord;
use ryeos_app::node_config::SectionTable;

/// Load the single installed hosted-node policy for `system_space_dir`.
///
/// Returns `Ok(None)` when no hosted policy is installed. Returns an error if
/// more than one policy is present because precedence/override semantics have
/// not been designed yet.
pub fn load_hosted_policy(system_space_dir: &Path) -> Result<Option<HostedNodePolicyRecord>> {
    let user_root = ryeos_engine::roots::user_root().ok();
    let trust_store =
        ryeos_engine::trust::TrustStore::load_three_tier(None, user_root.as_deref(), &[])
            .context("hosted policy: load trust store")?;
    let loader = BootstrapLoader {
        system_space_dir,
        trust_store: &trust_store,
    };
    let bundles = loader
        .load_bundle_section()
        .context("hosted policy: load verified node bundle registrations")?;
    let snapshot = loader
        .load_full(&SectionTable::new(), &bundles)
        .context("hosted policy: load verified node config")?;
    let mut records = snapshot.hosted_node_policies;

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

#[cfg(test)]
mod tests {
    use super::*;
    use rand::rngs::OsRng;
    use std::sync::{Mutex, MutexGuard};

    static ENV_MUTEX: Mutex<()> = Mutex::new(());

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

    struct Fixture {
        _tmp: tempfile::TempDir,
        _env_guard: MutexGuard<'static, ()>,
        system: std::path::PathBuf,
        key: lillux::crypto::SigningKey,
    }

    impl Fixture {
        fn new() -> Self {
            let env_guard = ENV_MUTEX.lock().unwrap();
            let tmp = tempfile::tempdir().unwrap();
            let user = tmp.path().join("user");
            let trust_dir = user
                .join(ryeos_engine::AI_DIR)
                .join("config")
                .join("keys")
                .join("trusted");
            std::fs::create_dir_all(&trust_dir).unwrap();
            let key = lillux::crypto::SigningKey::generate(&mut OsRng);
            ryeos_engine::trust::pin_key(&key.verifying_key(), "test", &trust_dir, None).unwrap();
            std::env::set_var("USER_SPACE", &user);

            Self {
                system: tmp.path().join("system"),
                _tmp: tmp,
                _env_guard: env_guard,
                key,
            }
        }

        fn write_policy(&self, path: &Path) {
            write_policy(path, &self.key);
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            std::env::remove_var("USER_SPACE");
        }
    }

    fn write_policy(path: &Path, key: &lillux::crypto::SigningKey) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            path,
            lillux::signature::sign_content(POLICY, key, "#", None),
        )
        .unwrap();
    }

    #[test]
    fn load_hosted_policy_reads_installed_bundle_policy() {
        let fixture = Fixture::new();
        let path = fixture.system.join(".ai/node/hosted/policy.yaml");
        fixture.write_policy(&path);

        let policy = load_hosted_policy(&fixture.system)
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
        let fixture = Fixture::new();
        fixture.write_policy(&fixture.system.join(".ai/node/hosted/policy.yaml"));
        fixture.write_policy(&fixture.system.join(".ai/node/hosted/extra/policy.yaml"));

        let err = load_hosted_policy(&fixture.system).unwrap_err();
        let rendered = format!("{err:#}");

        assert!(
            rendered.contains("multiple hosted-node policies"),
            "got: {rendered}"
        );
    }
}

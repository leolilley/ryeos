//! Hosted-node policy helpers for local operator tools.
//!
//! These helpers intentionally do not introduce a provider service or central
//! authority. They read the required node-owned hosted policy generation so
//! operator-side tools use the same admission choices as the daemon.

use std::path::Path;

use anyhow::{Context, Result};
use ryeos_app::node_policy::NodePolicyTable;
use ryeos_app::node_policy::sections::hosted::HostedNodePolicy;

#[derive(Debug, Clone)]
pub struct LoadedHostedNodePolicy {
    pub policy: HostedNodePolicy,
    pub source_file: std::path::PathBuf,
}

impl std::ops::Deref for LoadedHostedNodePolicy {
    type Target = HostedNodePolicy;

    fn deref(&self) -> &Self::Target {
        &self.policy
    }
}

/// Load the required hosted-node policy for `app_root`.
pub fn load_hosted_policy(app_root: &Path) -> Result<LoadedHostedNodePolicy> {
    let trust_store = ryeos_engine::trust::TrustStore::load(
        None,
        &ryeos_engine::roots::RuntimeRoot::new(app_root.to_path_buf()).config(),
    )
    .context("hosted policy: load trust store")?;
    let snapshot =
        ryeos_app::node_policy::load_snapshot(app_root, &trust_store, &NodePolicyTable::new())
            .context("hosted policy: load verified node policy generation")?;
    Ok(LoadedHostedNodePolicy {
        policy: snapshot.require::<HostedNodePolicy>()?.clone(),
        source_file: snapshot.source_file::<HostedNodePolicy>()?.to_path_buf(),
    })
}

#[cfg(test)]
pub(crate) fn write_required_non_hosted_test_policies(
    app_root: &Path,
    key: &lillux::crypto::SigningKey,
) {
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .find(|candidate| candidate.join("bundles/.ai/node/init/profiles").is_dir())
        .expect("workspace with node init profiles");
    let seed_path = workspace.join("bundles/.ai/node/init/profiles/hosted-workflow.yaml");
    let signed = std::fs::read_to_string(&seed_path).expect("read hosted-workflow node profile");
    let body = lillux::signature::strip_signature_lines(&signed);
    let seed: ryeos_app::node_policy::generation::NodeInitProfile =
        serde_yaml::from_str(&body).expect("parse hosted-workflow node profile");
    let policy_dir = app_root.join(".ai/node/policies");
    std::fs::create_dir_all(&policy_dir).expect("create test node-policy directory");
    for (name, body) in seed.policies() {
        if name == "hosted" {
            continue;
        }
        let body = serde_yaml::to_string(body).expect("serialize test node policy");
        std::fs::write(
            policy_dir.join(format!("{name}.yaml")),
            lillux::signature::sign_content(&body, key, "#", None),
        )
        .expect("write test node policy");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lillux::crypto::EncodePrivateKey;
    use rand::rngs::OsRng;
    const POLICY: &str = r#"
schema: 1
admission_enabled: true
admission_token_ttl_secs: 600
allow_loopback_http: true
"#;

    struct Fixture {
        _tmp: tempfile::TempDir,
        system: std::path::PathBuf,
        key: lillux::crypto::SigningKey,
    }

    impl Fixture {
        fn new() -> Self {
            let tmp = tempfile::tempdir().unwrap();
            let key = lillux::crypto::SigningKey::generate(&mut OsRng);
            let system = tmp.path().join("system");
            write_node_bootstrap(&system, &key);

            Self {
                system,
                _tmp: tmp,
                key,
            }
        }

        fn write_policy(&self, path: &Path) {
            write_policy(path, &self.key);
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

    fn write_node_bootstrap(app_root: &Path, key: &lillux::crypto::SigningKey) {
        let trust_dir = app_root.join(".ai/config/keys/trusted");
        std::fs::create_dir_all(&trust_dir).unwrap();
        ryeos_engine::trust::pin_key(&key.verifying_key(), "test", &trust_dir, None).unwrap();

        let identity_dir = app_root.join(".ai/node/identity");
        std::fs::create_dir_all(&identity_dir).unwrap();
        std::fs::write(
            identity_dir.join("private_key.pem"),
            key.to_pkcs8_pem(Default::default()).unwrap().as_bytes(),
        )
        .unwrap();

        let policy_dir = app_root.join(".ai/node/command_registration");
        std::fs::create_dir_all(&policy_dir).unwrap();
        let policy = r#"claim_rules:
  - claim:
      kind: command.root
      value: execute
    required_caps: []
system_source_caps:
  - ryeos.register.command.root.execute
"#;
        std::fs::write(
            policy_dir.join("default.yaml"),
            lillux::signature::sign_content(policy, key, "#", None),
        )
        .unwrap();
        write_required_non_hosted_test_policies(app_root, key);
    }

    #[test]
    fn load_hosted_policy_reads_current_node_policy_generation() {
        let fixture = Fixture::new();
        let path = fixture.system.join(".ai/node/policies/hosted.yaml");
        fixture.write_policy(&path);

        let policy = load_hosted_policy(&fixture.system).expect("policy should load");

        assert_eq!(policy.admission_token_ttl_secs, Some(600));
        assert!(policy.allow_loopback_http);
        assert_eq!(policy.source_file, path);
    }

    #[test]
    fn load_hosted_policy_rejects_missing_required_policy() {
        let fixture = Fixture::new();
        let err = load_hosted_policy(&fixture.system).unwrap_err();
        let rendered = format!("{err:#}");
        assert!(rendered.contains("hosted"), "got: {rendered}");
        assert!(
            rendered.contains("ExactlyOne") || rendered.contains("required"),
            "got: {rendered}"
        );
    }
}

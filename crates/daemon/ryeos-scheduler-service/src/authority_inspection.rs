//! Read-only inspection of schedules whose pinned grant generation no longer
//! matches the current grant, for the stopped-node doctor.
//!
//! The daemon learns this at a schedule's next fire, which it refuses and
//! pauses. The doctor reports it beforehand, from the same signed schedule
//! sources and the same generation check, so an operator can reauthorize.

use std::path::Path;

use anyhow::{Context as _, Result};
use ryeos_scheduler_contract::{ScheduledAuthorityRefusal, ScheduledAuthorityRefusalKind};
use serde::Serialize;

/// One schedule whose pinned grant generation no longer holds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SupersededScheduleGrant {
    pub schedule_id: String,
    pub principal_id: String,
    pub enabled: bool,
    pub refusal: ScheduledAuthorityRefusalKind,
    pub detail: String,
}

/// Every signed schedule source under `<app_root>/.ai/node/schedules` whose
/// pinned grant generation is missing or superseded in `authorized_keys_dir`.
/// Node-owned schedules pin no grant and are never reported. An unreadable or
/// untrusted source, or a grant that cannot be verified, is an error rather
/// than a finding.
pub fn superseded_schedule_grants(
    app_root: &Path,
    authorized_keys_dir: &Path,
    node_identity: &ryeos_node_runtime::identity::NodeIdentity,
    trust_store: &ryeos_engine::trust::TrustStore,
) -> Result<Vec<SupersededScheduleGrant>> {
    let schedules_dir = app_root
        .join(ryeos_engine::AI_DIR)
        .join("node")
        .join("schedules");
    if !schedules_dir.is_dir() {
        return Ok(Vec::new());
    }
    let mut superseded = Vec::new();
    for path in ryeos_scheduler::projection::canonical_schedule_source_paths(&schedules_dir)? {
        let source = ryeos_scheduler::projection::load_verified_schedule_source(
            &path,
            trust_store,
            node_identity.fingerprint(),
        )
        .with_context(|| format!("verify schedule source {}", path.display()))?;
        let record = source.record;
        match ryeos_app::scheduled_execution_authority::pinned_schedule_grants(
            &record.execution,
            authorized_keys_dir,
            node_identity,
        ) {
            Ok(_) => {}
            Err(error) => {
                let refusal = ScheduledAuthorityRefusal::find(&error)
                    .cloned()
                    .ok_or(error)
                    .with_context(|| {
                        format!("inspect pinned grants of schedule {}", record.schedule_id)
                    })?;
                superseded.push(SupersededScheduleGrant {
                    principal_id: record.execution.principal_id().to_owned(),
                    schedule_id: record.schedule_id,
                    enabled: record.enabled,
                    refusal: refusal.kind,
                    detail: refusal.detail,
                });
            }
        }
    }
    Ok(superseded)
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine as _;
    use ryeos_node_runtime::identity::{
        AuthorizedKeyPrincipalClass, NodeIdentity, create_authorized_key_toml,
        load_verified_authorized_key,
    };
    use ryeos_scheduler::types::ScheduleSourceRecord;
    use ryeos_scheduler_contract::{ScheduleExecution, ScheduleExecutionAuthority};

    struct Node {
        root: tempfile::TempDir,
        identity: NodeIdentity,
        principal: lillux::crypto::SigningKey,
    }

    impl Node {
        fn new() -> Self {
            let root = tempfile::tempdir().unwrap();
            let identity = NodeIdentity::create(&root.path().join("node.pem")).unwrap();
            Self {
                root,
                identity,
                principal: lillux::crypto::SigningKey::from_bytes(&[0x24; 32]),
            }
        }

        fn app_root(&self) -> &Path {
            self.root.path()
        }

        fn auth_dir(&self) -> std::path::PathBuf {
            self.root.path().join("authorized_keys")
        }

        fn principal_fp(&self) -> String {
            lillux::signature::compute_fingerprint(&self.principal.verifying_key())
        }

        fn trust(&self) -> ryeos_engine::trust::TrustStore {
            ryeos_engine::trust::TrustStore::from_signers(vec![
                ryeos_engine::trust::TrustedSigner {
                    fingerprint: lillux::signature::compute_fingerprint(
                        self.identity.verifying_key(),
                    ),
                    verifying_key: *self.identity.verifying_key(),
                    label: Some("node".to_owned()),
                },
            ])
        }

        fn grant(&self, created_at: &str) {
            let key = base64::engine::general_purpose::STANDARD
                .encode(self.principal.verifying_key().to_bytes());
            create_authorized_key_toml(
                &self.auth_dir(),
                &self.principal_fp(),
                &key,
                &["ryeos.execute.tool.test/job".to_owned()],
                "schedule owner",
                &format!("fp:{}", "99".repeat(32)),
                created_at,
                self.identity.signing_key(),
            )
            .unwrap();
        }

        fn revoke(&self) {
            std::fs::remove_file(
                self.auth_dir()
                    .join(format!("{}.toml", self.principal_fp())),
            )
            .unwrap();
        }

        /// Write a signed schedule pinned to the grant generation present now.
        fn pinned_schedule(&self, schedule_id: &str) {
            let grant = load_verified_authorized_key(
                &self.principal_fp(),
                &self.auth_dir(),
                &self.identity,
            )
            .unwrap()
            .expect("grant to pin");
            let record = ScheduleSourceRecord {
                spec_version: 2,
                schedule_id: schedule_id.to_owned(),
                item_ref: "tool:test/job".to_owned(),
                ref_bindings: std::collections::BTreeMap::new(),
                schedule_type: "interval".to_owned(),
                expression: "60".to_owned(),
                params: serde_json::json!({}),
                timezone: "UTC".to_owned(),
                misfire_policy: "skip".to_owned(),
                overlap_policy: "skip".to_owned(),
                lateness_grace_secs: 60,
                enabled: true,
                project_root: None,
                registered_at: 1_000,
                execution: ScheduleExecution {
                    authority: ScheduleExecutionAuthority::Authenticated {
                        principal_id: format!("fp:{}", self.principal_fp()),
                        principal_class: AuthorizedKeyPrincipalClass::LocalClient,
                        effective_origin_site_id: "site:test".to_owned(),
                        grant_authority:
                            ryeos_engine::principal_contract::AuthenticatedGrantAuthority {
                                principal_grant_hash: grant.source_file_hash,
                                forwarding: None,
                            },
                        registration_request_hash: "88".repeat(32),
                    },
                    capabilities: vec!["ryeos.execute.tool.test/job".to_owned()],
                    policy: ryeos_engine::execution_contract::ExecutionPolicy::projectless(
                        ryeos_engine::execution_contract::ExecutionResponse::Accepted,
                    ),
                    limits: None,
                    max_fires: None,
                    expires_at: None,
                },
                managed_by: None,
                paused_reason: None,
            };
            let bytes = ryeos_node_runtime::node_document::render_signed_item(
                "schedules",
                schedule_id,
                &serde_json::to_value(&record).unwrap(),
                &self.identity,
            )
            .unwrap();
            let dir = self.app_root().join(".ai/node/schedules");
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join(format!("{schedule_id}.yaml")), bytes).unwrap();
        }

        fn inspect(&self) -> Vec<SupersededScheduleGrant> {
            superseded_schedule_grants(
                self.app_root(),
                &self.auth_dir(),
                &self.identity,
                &self.trust(),
            )
            .unwrap()
        }
    }

    #[test]
    fn a_node_without_schedules_has_no_findings() {
        let node = Node::new();
        assert!(node.inspect().is_empty());
    }

    #[test]
    fn superseded_and_revoked_generations_are_reported() {
        let node = Node::new();
        node.grant("2026-11-13T00:00:00Z");
        node.pinned_schedule("nightly");
        assert!(
            node.inspect().is_empty(),
            "the pinned generation is current"
        );

        node.revoke();
        node.grant("2026-11-13T00:05:00Z");
        let findings = node.inspect();
        assert_eq!(findings.len(), 1, "{findings:#?}");
        assert_eq!(findings[0].schedule_id, "nightly");
        assert_eq!(
            findings[0].refusal,
            ScheduledAuthorityRefusalKind::GrantGenerationChanged
        );
        assert!(findings[0].enabled);

        node.revoke();
        assert_eq!(
            node.inspect()[0].refusal,
            ScheduledAuthorityRefusalKind::GrantRevoked
        );
    }
}

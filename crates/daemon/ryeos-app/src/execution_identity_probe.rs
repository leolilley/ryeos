//! Boot-time publication of the stable node execution substrate.
//!
//! This coordinate contains only facts shared by every launch on this daemon.
//! Interpreters, runtime trees, backends, numerics, and workload artifacts are
//! admitted per launch and must never be inferred from ambient node state here.

use std::collections::BTreeSet;
use std::sync::Arc;

use anyhow::{Context, Result, bail};

use ryeos_state::objects::{
    EXECUTION_IDENTITY_KIND, EXECUTION_IDENTITY_SCHEMA_VERSION, ExecutionCpuIdentity,
    ExecutionIdentity, ExecutionOperatingSystemIdentity, ExecutionSubstrateBuild,
};

/// Claim carried by the node-signed attestation over the substrate object.
pub const EXECUTION_IDENTITY_CLAIM: &str =
    ryeos_state::objects::EXECUTION_IDENTITY_ATTESTATION_CLAIM;
pub const EXECUTION_IDENTITY_POLICY: &str =
    ryeos_state::objects::EXECUTION_IDENTITY_ATTESTATION_POLICY;
pub const EXECUTION_IDENTITY_HEAD_NAMESPACE: &str = "execution-substrate";
pub const EXECUTION_IDENTITY_HEAD_NAME: &str = "current";

/// The exact rooted substrate evidence made available to launch admission.
#[derive(Debug, Clone)]
pub struct NodeExecutionIdentity {
    pub identity: ExecutionIdentity,
    pub digest: String,
    pub identity_hash: String,
    pub attestation_hash: String,
}

/// Observe stable node facts without consulting PATH or another workload-owned
/// executable. A missing kernel CPU description is represented honestly.
pub fn probe_node_execution_identity(
    build: &crate::build_info::BuildInfo,
    node_signer_fingerprint: &str,
) -> Result<ExecutionIdentity> {
    let (model, features) = probe_cpu_description();
    let identity = ExecutionIdentity {
        schema: EXECUTION_IDENTITY_SCHEMA_VERSION,
        kind: EXECUTION_IDENTITY_KIND.to_owned(),
        daemon: ExecutionSubstrateBuild {
            version: build.version.to_owned(),
            revision: build.revision.to_owned(),
            build_date: build.build_date.to_owned(),
            profile: build.profile.to_owned(),
        },
        operating_system: ExecutionOperatingSystemIdentity {
            family: std::env::consts::OS.to_owned(),
            architecture: std::env::consts::ARCH.to_owned(),
        },
        cpu: ExecutionCpuIdentity { model, features },
        node_signer_fingerprint: node_signer_fingerprint.to_owned(),
    };
    identity.validate()?;
    Ok(identity)
}

/// Publish identity + attestation and advance the signed durable head as one
/// required boot operation. Failure is fatal: an unrooted digest is not
/// execution evidence and must never be stamped on later launches.
pub fn boot_node_execution_identity(
    state_store: &crate::state_store::StateStore,
    identity: &crate::identity::NodeIdentity,
    build: &crate::build_info::BuildInfo,
) -> Result<Arc<NodeExecutionIdentity>> {
    let substrate = probe_node_execution_identity(build, identity.fingerprint())?;
    let digest = substrate.identity_digest()?;
    let authority = state_store.pinned_state_authority()?;
    let guard = authority.acquire_shared_guard()?;
    authority.ensure_guard(&guard)?;
    let cas = authority.cas_store()?;
    let identity_hash = cas
        .store_object(&substrate.to_value()?)
        .context("storing node execution substrate identity")?;

    let signer = crate::state_store::NodeIdentitySigner::from_identity(identity);
    let current_head = state_store.with_state_db(|db| {
        db.read_generic_head_ref(
            EXECUTION_IDENTITY_HEAD_NAMESPACE,
            EXECUTION_IDENTITY_HEAD_NAME,
        )
    })?;
    let attestation_hash = if let Some(current_head) = current_head.as_ref() {
        let current_value = cas
            .get_object(&current_head.target_hash)?
            .ok_or_else(|| anyhow::anyhow!("node execution substrate head target is missing"))?;
        let current = ryeos_state::objects::Attestation::from_value(&current_value)
            .context("parsing current node execution substrate attestation")?;
        let current_identity_value = cas.get_object(&current.subject_hash)?.ok_or_else(|| {
            anyhow::anyhow!("current node execution substrate identity is missing")
        })?;
        let current_identity =
            ryeos_state::objects::ExecutionIdentity::from_current_value(&current_identity_value)
                .context("parsing current node execution substrate identity")?;
        if current_identity.node_signer_fingerprint != identity.fingerprint() {
            bail!("current node execution substrate identity names another node signer");
        }
        let current_digest = current_identity.identity_digest()?;
        let same_identity = validate_current_attestation(
            &current,
            identity,
            &current_digest,
            &identity_hash,
            &digest,
        )?;
        if same_identity {
            current_head.target_hash.clone()
        } else {
            let next = build_attestation(&signer, &identity_hash, &digest)?;
            let next_hash = cas
                .store_object(&serde_json::to_value(&next)?)
                .context("storing node execution substrate attestation")?;
            state_store.with_state_db(|db| {
                db.advance_generic_head_ref(
                    EXECUTION_IDENTITY_HEAD_NAMESPACE,
                    EXECUTION_IDENTITY_HEAD_NAME,
                    &next_hash,
                    Some(&current_head.target_hash),
                    &signer,
                    &guard,
                )
                .context("advancing node execution substrate head")
            })?;
            next_hash
        }
    } else {
        let first = build_attestation(&signer, &identity_hash, &digest)?;
        let first_hash = cas
            .store_object(&serde_json::to_value(&first)?)
            .context("storing node execution substrate attestation")?;
        state_store.with_state_db(|db| {
            db.advance_generic_head_ref(
                EXECUTION_IDENTITY_HEAD_NAMESPACE,
                EXECUTION_IDENTITY_HEAD_NAME,
                &first_hash,
                None,
                &signer,
                &guard,
            )
            .context("publishing node execution substrate head")
        })?;
        first_hash
    };
    authority.ensure_guard(&guard)?;

    tracing::info!(
        %digest,
        %identity_hash,
        %attestation_hash,
        "published node execution substrate identity"
    );
    Ok(Arc::new(NodeExecutionIdentity {
        identity: substrate,
        digest,
        identity_hash,
        attestation_hash,
    }))
}

fn build_attestation(
    signer: &dyn ryeos_state::Signer,
    identity_hash: &str,
    digest: &str,
) -> Result<ryeos_state::objects::Attestation> {
    ryeos_state::objects::Attestation::unsigned(
        identity_hash.to_owned(),
        EXECUTION_IDENTITY_CLAIM.to_owned(),
        EXECUTION_IDENTITY_POLICY.to_owned(),
        lillux::time::iso8601_now(),
        None,
        serde_json::json!({ "identity_digest": digest }),
    )
    .sign(signer)
    .context("signing node execution substrate attestation")
}

/// Verify the currently rooted evidence before deciding whether a restart can
/// reuse it. A valid attestation for a prior substrate returns `false`; malformed,
/// wrongly scoped, expired, or differently signed evidence fails closed.
fn validate_current_attestation(
    attestation: &ryeos_state::objects::Attestation,
    node_identity: &crate::identity::NodeIdentity,
    subject_identity_digest: &str,
    identity_hash: &str,
    identity_digest: &str,
) -> Result<bool> {
    attestation
        .verify_with_key(node_identity.verifying_key())
        .context("verifying current node execution substrate attestation")?;
    if attestation.claim != EXECUTION_IDENTITY_CLAIM
        || attestation.policy != EXECUTION_IDENTITY_POLICY
    {
        bail!("current node execution substrate attestation has the wrong claim or policy");
    }
    if attestation.is_expired_at(&lillux::time::iso8601_now())? {
        bail!("current node execution substrate attestation is expired");
    }
    let observed_digest = attestation
        .evidence
        .get("identity_digest")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "current node execution substrate attestation has no identity digest evidence"
            )
        })?;
    if !lillux::valid_hash(observed_digest) {
        bail!("current node execution substrate attestation has a malformed identity digest");
    }
    if observed_digest != subject_identity_digest {
        bail!("current node execution substrate attestation contradicts its subject identity");
    }
    Ok(attestation.subject_hash == identity_hash && observed_digest == identity_digest)
}

fn probe_cpu_description() -> (Option<String>, Vec<String>) {
    #[cfg(target_os = "linux")]
    {
        let Ok(cpuinfo) = std::fs::read_to_string("/proc/cpuinfo") else {
            return (None, Vec::new());
        };
        return parse_linux_cpuinfo(&cpuinfo);
    }
    #[cfg(not(target_os = "linux"))]
    {
        (None, Vec::new())
    }
}

#[cfg(target_os = "linux")]
fn parse_linux_cpuinfo(cpuinfo: &str) -> (Option<String>, Vec<String>) {
    let mut model = None;
    let mut features = BTreeSet::new();
    for line in cpuinfo.lines() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let key = key.trim();
        let value = value.trim();
        if model.is_none() && matches!(key, "model name" | "Processor" | "cpu model") {
            if !value.is_empty() {
                model = Some(value.to_owned());
            }
        }
        if matches!(key, "flags" | "Features") {
            features.extend(value.split_ascii_whitespace().map(str::to_owned));
        }
    }
    (model, features.into_iter().collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node_identity() -> (tempfile::TempDir, crate::identity::NodeIdentity) {
        let directory = tempfile::tempdir().unwrap();
        let identity =
            crate::identity::NodeIdentity::create(&directory.path().join("node.pem")).unwrap();
        (directory, identity)
    }

    fn build() -> crate::build_info::BuildInfo {
        crate::build_info::BuildInfo {
            version: "1.2.3",
            revision: "abc",
            build_date: "2026-08-08",
            profile: "release",
        }
    }

    #[test]
    fn probe_contains_only_stable_node_substrate_facts() {
        let identity = probe_node_execution_identity(&build(), &"a".repeat(64)).unwrap();
        assert_eq!(identity.daemon.version, "1.2.3");
        assert_eq!(
            identity.operating_system.architecture,
            std::env::consts::ARCH
        );
        let value = identity.to_value().unwrap();
        for launch_scoped in ["interpreter", "kernel_stack", "numerics", "model"] {
            assert!(value.get(launch_scoped).is_none());
        }
    }

    #[test]
    fn restart_reuses_only_exact_current_substrate_evidence() {
        let (_directory, identity) = node_identity();
        let signer = crate::state_store::NodeIdentitySigner::from_identity(&identity);
        let identity_hash = "a".repeat(64);
        let digest = "b".repeat(64);
        let current = build_attestation(&signer, &identity_hash, &digest).unwrap();

        assert!(
            validate_current_attestation(&current, &identity, &digest, &identity_hash, &digest,)
                .unwrap()
        );
        assert!(
            !validate_current_attestation(
                &current,
                &identity,
                &digest,
                &"c".repeat(64),
                &"d".repeat(64),
            )
            .unwrap()
        );
        assert!(
            validate_current_attestation(
                &current,
                &identity,
                &"e".repeat(64),
                &identity_hash,
                &digest,
            )
            .is_err()
        );
    }

    #[test]
    fn restart_refuses_current_substrate_evidence_with_another_policy() {
        let (_directory, identity) = node_identity();
        let signer = crate::state_store::NodeIdentitySigner::from_identity(&identity);
        let attestation = ryeos_state::objects::Attestation::unsigned(
            "a".repeat(64),
            EXECUTION_IDENTITY_CLAIM.to_owned(),
            "another-policy".to_owned(),
            lillux::time::iso8601_now(),
            None,
            serde_json::json!({ "identity_digest": "b".repeat(64) }),
        )
        .sign(&signer)
        .unwrap();

        assert!(
            validate_current_attestation(
                &attestation,
                &identity,
                &"b".repeat(64),
                &"a".repeat(64),
                &"b".repeat(64),
            )
            .is_err()
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_cpu_description_is_canonical_and_component_sensitive() {
        let (model, features) = parse_linux_cpuinfo(
            "model name : Example CPU\nflags : sse4_2 avx2 sse4_2\nflags : avx2 xsave\n",
        );
        assert_eq!(model.as_deref(), Some("Example CPU"));
        assert_eq!(features, ["avx2", "sse4_2", "xsave"]);
    }
}

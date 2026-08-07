//! Boot-time probe of this node's execution identity.
//!
//! The probe observes the tranches that exist before any local-inference
//! runtime does — device (class + arch) and the ambient interpreter — and
//! publishes them through machinery the substrate already trusts: the
//! `ExecutionIdentity` object into CAS, and a node-key-signed
//! [`Attestation`] over its hash. Nothing new is invented for the "signed
//! node document"; an attestation *is* one. Kernel-stack and numerics
//! tranches stay absent until the sealed-local runtime can name them
//! honestly.
//!
//! Publication is evidence, not liveness: a node that cannot publish still
//! knows its digest in memory, and records stamp the digest string.

use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use sha2::{Digest as _, Sha256};

use ryeos_state::objects::{
    EXECUTION_IDENTITY_KIND, EXECUTION_IDENTITY_SCHEMA_VERSION, ExecutionDeviceIdentity,
    ExecutionIdentity, ExecutionInterpreterIdentity,
};
use ryeos_state::signer::Signer;

/// Claim string for the boot-probe attestation. Local policy that treats
/// node-issued execution identities as authoritative keys on this.
pub const EXECUTION_IDENTITY_CLAIM: &str = "ryeos.node.execution_identity";
pub const EXECUTION_IDENTITY_POLICY: &str = "node-boot-probe/v1";

/// The probed identity, its canonical digest, and (when publication
/// succeeded) the attestation object hash.
#[derive(Debug, Clone)]
pub struct NodeExecutionIdentity {
    pub identity: ExecutionIdentity,
    pub digest: String,
    pub identity_hash: Option<String>,
    pub attestation_hash: Option<String>,
}

/// Observe the device and interpreter tranches. Pure observation — no
/// state is touched, and a missing interpreter is an honest `None`, never
/// an error.
pub fn probe_node_execution_identity() -> Result<NodeExecutionIdentity> {
    let identity = ExecutionIdentity {
        schema: EXECUTION_IDENTITY_SCHEMA_VERSION,
        kind: EXECUTION_IDENTITY_KIND.to_string(),
        device: ExecutionDeviceIdentity {
            class: "cpu".to_string(),
            arch: std::env::consts::ARCH.to_string(),
            detail: None,
        },
        interpreter: probe_interpreter(),
        kernel_stack: None,
        numerics: None,
    };
    let digest = identity.identity_digest()?;
    Ok(NodeExecutionIdentity {
        identity,
        digest,
        identity_hash: None,
        attestation_hash: None,
    })
}

/// Publish the identity object and a node-signed attestation over it into
/// CAS. Returns the enriched handle; failures here are the caller's to log
/// — the in-memory digest stays valid either way.
pub fn publish_node_execution_identity(
    state_store: &crate::state_store::StateStore,
    identity: &crate::identity::NodeIdentity,
    probed: NodeExecutionIdentity,
) -> Result<NodeExecutionIdentity> {
    let authority = state_store.with_state_db(|db| db.pinned_authority())?;
    let guard = authority.acquire_shared_guard()?;
    authority.ensure_guard(&guard)?;
    let cas = authority.cas_store()?;
    let identity_hash = cas
        .store_object(&probed.identity.to_value()?)
        .context("storing execution identity object")?;

    let signer = crate::state_store::NodeIdentitySigner::from_identity(identity);
    let attestation = ryeos_state::objects::Attestation::unsigned(
        identity_hash.clone(),
        EXECUTION_IDENTITY_CLAIM.to_string(),
        EXECUTION_IDENTITY_POLICY.to_string(),
        lillux::time::iso8601_now(),
        None,
        serde_json::json!({ "identity_digest": &probed.digest }),
    )
    .sign(&signer)
    .context("signing execution identity attestation")?;
    let attestation_hash = cas
        .store_object(&serde_json::to_value(&attestation)?)
        .context("storing execution identity attestation")?;
    authority.ensure_guard(&guard)?;

    Ok(NodeExecutionIdentity {
        identity_hash: Some(identity_hash),
        attestation_hash: Some(attestation_hash),
        ..probed
    })
}

/// Probe, publish best-effort, and hand back the Arc the composition root
/// parks in the extensions bag. Publication failure downgrades to a warn:
/// the digest is still authoritative for this process's stamps.
pub fn boot_node_execution_identity(
    state_store: &crate::state_store::StateStore,
    identity: &crate::identity::NodeIdentity,
) -> Option<Arc<NodeExecutionIdentity>> {
    let probed = match probe_node_execution_identity() {
        Ok(probed) => probed,
        Err(error) => {
            tracing::warn!(%error, "execution identity probe failed; records will not be stamped");
            return None;
        }
    };
    let published = match publish_node_execution_identity(state_store, identity, probed.clone()) {
        Ok(published) => published,
        Err(error) => {
            tracing::warn!(
                %error,
                digest = %probed.digest,
                "execution identity publication failed; keeping unpublished digest"
            );
            probed
        }
    };
    tracing::info!(
        digest = %published.digest,
        identity_hash = published.identity_hash.as_deref().unwrap_or("<unpublished>"),
        attestation_hash = published.attestation_hash.as_deref().unwrap_or("<unpublished>"),
        interpreter = published
            .identity
            .interpreter
            .as_ref()
            .map(|interpreter| interpreter.version.as_str())
            .unwrap_or("<none>"),
        "node execution identity"
    );
    Some(Arc::new(published))
}

fn probe_interpreter() -> Option<ExecutionInterpreterIdentity> {
    let path = resolve_on_path("python3")?;
    let binary_sha256 = hash_file(&path).ok()?;
    let output = std::process::Command::new(&path)
        .arg("--version")
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(if output.stdout.is_empty() {
        &output.stderr
    } else {
        &output.stdout
    })
    .trim()
    .to_string();
    let version = text.strip_prefix("Python ")?.to_string();
    if version.is_empty() {
        return None;
    }
    Some(ExecutionInterpreterIdentity {
        version,
        binary_sha256,
    })
}

fn resolve_on_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(name);
        let Ok(metadata) = std::fs::metadata(&candidate) else {
            continue;
        };
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            if metadata.is_file() && metadata.permissions().mode() & 0o111 != 0 {
                return Some(candidate);
            }
        }
        #[cfg(not(unix))]
        if metadata.is_file() {
            return Some(candidate);
        }
    }
    None
}

fn hash_file(path: &Path) -> Result<String> {
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 128 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_probe_produces_a_valid_digestable_identity() {
        let probed = probe_node_execution_identity().unwrap();
        assert_eq!(probed.identity.device.class, "cpu");
        assert_eq!(probed.identity.device.arch, std::env::consts::ARCH);
        assert_eq!(probed.digest.len(), 64);
        // Probing twice on the same node yields the same coordinate.
        assert_eq!(probed.digest, probe_node_execution_identity().unwrap().digest);
    }
}

//! Bounded OCI lifecycle coordinates retained by the installed host adapter.
//!
//! OCI vocabulary stays inside Lillux. RyeOS receives only an opaque
//! generation digest and never parses container IDs, hook state, host PIDs, or
//! cgroup identities.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

const OCI_LIFECYCLE_GENERATION_VERSION: u32 = 1;

/// Closed subset of OCI hook state consumed from bounded standard input.
/// Annotations are retained as inert strings and never grant authority.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OciHookState {
    #[serde(rename = "ociVersion")]
    pub oci_version: String,
    pub id: String,
    pub status: String,
    pub pid: u32,
    #[serde(default)]
    pub bundle: Option<PathBuf>,
    #[serde(default)]
    pub annotations: BTreeMap<String, String>,
}

impl OciHookState {
    pub const MAX_DOCUMENT_BYTES: usize = 65_536;

    pub fn parse_bounded(bytes: &[u8]) -> Result<Self, String> {
        if bytes.is_empty() || bytes.len() > Self::MAX_DOCUMENT_BYTES {
            return Err("OCI hook state is absent or exceeds the bound".to_owned());
        }
        serde_json::from_slice(bytes).map_err(|error| format!("parse OCI hook state: {error}"))
    }

    pub fn validate_prestart(&self) -> Result<(), String> {
        self.validate_common()?;
        if self.status != "created" || self.pid <= 1 {
            return Err("OCI prestart requires one created container init process".to_owned());
        }
        let bundle = self
            .bundle
            .as_ref()
            .ok_or("OCI prestart state has no bundle path")?;
        if !bundle.is_absolute()
            || bundle.parent().is_none()
            || bundle.components().any(|component| {
                matches!(
                    component,
                    std::path::Component::ParentDir | std::path::Component::CurDir
                )
            })
        {
            return Err("OCI hook bundle must be an exact absolute path".to_owned());
        }
        Ok(())
    }

    pub fn validate_poststop(&self) -> Result<(), String> {
        self.validate_common()?;
        if self.status != "stopped" {
            return Err("OCI poststop requires one stopped container state".to_owned());
        }
        Ok(())
    }

    fn validate_common(&self) -> Result<(), String> {
        if !matches!(
            self.oci_version.as_str(),
            "1.0.0" | "1.0.1" | "1.0.2" | "1.1.0" | "1.2.0" | "1.3.0"
        ) {
            return Err("OCI hook state uses an unsupported specification version".to_owned());
        }
        if self.id.is_empty()
            || self.id.len() > 128
            || !self
                .id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
        {
            return Err("OCI hook state has a noncanonical container identity".to_owned());
        }
        if self.annotations.len() > 128
            || self.annotations.iter().any(|(name, value)| {
                name.is_empty()
                    || name.len() > 256
                    || value.len() > 4096
                    || name.contains('\0')
                    || value.contains('\0')
            })
        {
            return Err("OCI hook annotations exceed the inert bounded contract".to_owned());
        }
        Ok(())
    }
}

/// Host-retained identity of one exact enclosing OCI lifetime and its strict
/// Lillux controller descendant. Evidence and invalidation state, not a
/// transferable process-control capability.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OciLifecycleGeneration {
    version: u32,
    container_id: String,
    host_lifetime: super::ProcessHostLifetime,
    init_process: super::ExactProcessIdentity,
    lifecycle_scope: crate::PinnedDirectoryIdentity,
    controller_scope: crate::PinnedDirectoryIdentity,
    lifecycle_path: PathBuf,
    controller_path: PathBuf,
    generation: String,
}

impl OciLifecycleGeneration {
    pub(crate) fn new(
        container_id: String,
        host_lifetime: super::ProcessHostLifetime,
        init_process: super::ExactProcessIdentity,
        lifecycle_scope: crate::PinnedDirectoryIdentity,
        controller_scope: crate::PinnedDirectoryIdentity,
        lifecycle_path: PathBuf,
        controller_path: PathBuf,
        generation: String,
    ) -> Result<Self, String> {
        let value = Self {
            version: OCI_LIFECYCLE_GENERATION_VERSION,
            container_id,
            host_lifetime,
            init_process,
            lifecycle_scope,
            controller_scope,
            lifecycle_path,
            controller_path,
            generation,
        };
        value.validate()?;
        Ok(value)
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.version != OCI_LIFECYCLE_GENERATION_VERSION {
            return Err("OCI lifecycle generation contract is not current".to_owned());
        }
        self.host_lifetime.validate()?;
        self.init_process.validate()?;
        if self.container_id.is_empty()
            || self.container_id.len() > 128
            || !self
                .container_id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
        {
            return Err(
                "OCI lifecycle generation has a noncanonical container identity".to_owned(),
            );
        }
        if !self.generation.starts_with("sha256:")
            || self.generation.len() != 71
            || !self.generation[7..]
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err("OCI lifecycle generation digest is invalid".to_owned());
        }
        if self.lifecycle_scope == self.controller_scope {
            return Err("OCI controller scope is not a strict lifecycle descendant".to_owned());
        }
        if !self.lifecycle_path.is_absolute()
            || self.controller_path.parent() != Some(self.lifecycle_path.as_path())
        {
            return Err(
                "OCI lifecycle paths do not describe the strict controller child".to_owned(),
            );
        }
        Ok(())
    }

    pub fn digest(&self) -> &str {
        &self.generation
    }

    pub(crate) fn controller_scope(&self) -> crate::PinnedDirectoryIdentity {
        self.controller_scope
    }

    pub fn prove_ended_and_retire(&self) -> Result<(), String> {
        self.validate()?;
        if self.host_lifetime.has_ended()? {
            // Kernel cgroup and PID identities cannot survive a host boot.
            // Do not inspect or remove a coincidentally reused path/inode.
            return Ok(());
        }
        match super::capture_exact_process_identity(
            self.init_process.target_pid,
            Some(self.init_process.group_leader_pid),
        ) {
            Ok(current) if current == self.init_process => {
                return Err("OCI init process is still the admitted incarnation".to_owned());
            }
            Ok(_) => {}
            Err(error)
                if PathBuf::from(format!("/proc/{}", self.init_process.target_pid)).exists() =>
            {
                return Err(format!("cannot prove OCI init death: {error}"));
            }
            Err(_) => {}
        }
        #[cfg(target_os = "linux")]
        super::cgroup::retire_ended_oci_controller(
            &self.lifecycle_path,
            self.lifecycle_scope,
            &self.controller_path,
            self.controller_scope,
        )?;
        #[cfg(not(target_os = "linux"))]
        return Err("OCI lifecycle retirement is unavailable on this OS".to_owned());
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hook_state_is_closed_and_bounded() {
        let valid = serde_json::json!({
            "ociVersion": "1.3.0",
            "id": "contained-worker-1",
            "status": "created",
            "pid": 42,
            "bundle": "/run/oci/bundle",
            "annotations": {}
        });
        let state: OciHookState = serde_json::from_value(valid.clone()).unwrap();
        state.validate_prestart().unwrap();
        let mut unknown = valid;
        unknown["host_cgroup"] = "/ambient".into();
        assert!(serde_json::from_value::<OciHookState>(unknown).is_err());
    }
}

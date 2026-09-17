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
    pub bundle: PathBuf,
    #[serde(default)]
    pub annotations: BTreeMap<String, String>,
}

impl OciHookState {
    pub fn validate_prestart(&self) -> Result<(), String> {
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
        if self.status != "created" || self.pid <= 1 {
            return Err("OCI prestart requires one created container init process".to_owned());
        }
        if !self.bundle.is_absolute()
            || self.bundle.parent().is_none()
            || self.bundle.components().any(|component| {
                matches!(
                    component,
                    std::path::Component::ParentDir | std::path::Component::CurDir
                )
            })
        {
            return Err("OCI hook bundle must be an exact absolute path".to_owned());
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
    generation: String,
}

impl OciLifecycleGeneration {
    pub(crate) fn new(
        container_id: String,
        host_lifetime: super::ProcessHostLifetime,
        init_process: super::ExactProcessIdentity,
        lifecycle_scope: crate::PinnedDirectoryIdentity,
        controller_scope: crate::PinnedDirectoryIdentity,
        generation: String,
    ) -> Result<Self, String> {
        let value = Self {
            version: OCI_LIFECYCLE_GENERATION_VERSION,
            container_id,
            host_lifetime,
            init_process,
            lifecycle_scope,
            controller_scope,
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
        Ok(())
    }

    pub fn digest(&self) -> &str {
        &self.generation
    }

    pub(crate) fn controller_scope(&self) -> crate::PinnedDirectoryIdentity {
        self.controller_scope
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

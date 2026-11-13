//! Read-only inventory of what a provider account holds.
//!
//! The node records what it asked a provider for; it never asked the provider
//! what exists. An inventory lists the sandboxes and snapshots one provider
//! owner scope holds, so a node can find resources its records end, resources
//! it has no record of, and records the provider no longer has. Listing never
//! mutates the provider and never authorizes deleting anything.

use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};

use crate::MAX_LIFECYCLE_RESPONSE_BYTES;

pub const PROVIDER_INVENTORY_PROTOCOL: &str = "ryeos.provider-inventory.v1";
/// Rows one inventory may carry; a provider holding more is reported
/// incomplete, never silently cut.
pub const MAX_PROVIDER_INVENTORY_RESOURCES: usize = 256;
pub const MAX_PROVIDER_INVENTORY_REQUEST_BYTES: usize = 4 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderResourceKind {
    Sandbox,
    Snapshot,
}

/// One resource as the provider reports it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderResource {
    pub kind: ProviderResourceKind,
    pub provider_id: String,
    pub state: String,
    /// RFC 3339 creation time the provider reported, when it reported one.
    pub created_at: Option<String>,
}

impl ProviderResource {
    pub fn validate(&self) -> Result<()> {
        bounded_text(&self.provider_id, 256, "provider resource id")?;
        bounded_text(&self.state, 64, "provider resource state")?;
        if let Some(created_at) = &self.created_at {
            bounded_text(created_at, 64, "provider resource creation time")?;
        }
        Ok(())
    }
}

/// The adapter is handed the binding's credential, settings and network
/// inputs and lists the owner scope those settings name.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderInventoryAdapterRequest {
    pub protocol: String,
    pub provider_id: String,
    pub adapter_artifact_hash: String,
    pub settings_digest: String,
    pub provider_spec_digest: String,
}

impl ProviderInventoryAdapterRequest {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.protocol == PROVIDER_INVENTORY_PROTOCOL,
            "unsupported provider inventory protocol"
        );
        bounded_text(&self.provider_id, 128, "inventory provider")?;
        for (label, value) in [
            ("adapter", &self.adapter_artifact_hash),
            ("settings", &self.settings_digest),
            ("provider spec", &self.provider_spec_digest),
        ] {
            ensure!(
                lillux::valid_hash(value),
                "inventory {label} digest is invalid"
            );
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderInventoryAdapterResponse {
    pub protocol: String,
    pub provider_id: String,
    /// The provider owner scope that was listed.
    pub owner_scope: String,
    /// False when the listing stopped at its page or row bound: absence of a
    /// resource from an incomplete listing proves nothing.
    pub complete: bool,
    pub resources: Vec<ProviderResource>,
}

impl ProviderInventoryAdapterResponse {
    pub fn validate_for(&self, request: &ProviderInventoryAdapterRequest) -> Result<()> {
        ensure!(
            self.protocol == PROVIDER_INVENTORY_PROTOCOL && self.provider_id == request.provider_id,
            "provider inventory response differs from its request"
        );
        bounded_text(&self.owner_scope, 256, "inventory owner scope")?;
        ensure!(
            self.resources.len() <= MAX_PROVIDER_INVENTORY_RESOURCES,
            "provider inventory exceeds its row bound"
        );
        let mut seen = std::collections::BTreeSet::new();
        for resource in &self.resources {
            resource.validate()?;
            ensure!(
                seen.insert((resource.kind, resource.provider_id.as_str())),
                "provider inventory lists one resource twice"
            );
        }
        ensure!(
            crate::canonical_json(self)?.len() <= MAX_LIFECYCLE_RESPONSE_BYTES,
            "provider inventory exceeds its response bound"
        );
        Ok(())
    }
}

fn bounded_text(value: &str, maximum: usize, label: &str) -> Result<()> {
    ensure!(
        !value.is_empty() && value.len() <= maximum && !value.bytes().any(|b| b.is_ascii_control()),
        "{label} is empty, too long or has control characters"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request() -> ProviderInventoryAdapterRequest {
        ProviderInventoryAdapterRequest {
            protocol: PROVIDER_INVENTORY_PROTOCOL.into(),
            provider_id: "render-sandbox".into(),
            adapter_artifact_hash: "a".repeat(64),
            settings_digest: "b".repeat(64),
            provider_spec_digest: "c".repeat(64),
        }
    }

    fn sandbox(id: &str) -> ProviderResource {
        ProviderResource {
            kind: ProviderResourceKind::Sandbox,
            provider_id: id.into(),
            state: "running".into(),
            created_at: Some("2026-11-13T00:00:00Z".into()),
        }
    }

    #[test]
    fn a_response_is_bounded_and_lists_each_resource_once() {
        let request = request();
        request.validate().unwrap();
        let mut response = ProviderInventoryAdapterResponse {
            protocol: PROVIDER_INVENTORY_PROTOCOL.into(),
            provider_id: "render-sandbox".into(),
            owner_scope: "tea-1".into(),
            complete: true,
            resources: vec![sandbox("sbx-1"), sandbox("sbx-2")],
        };
        response.validate_for(&request).unwrap();
        response.resources.push(sandbox("sbx-1"));
        assert!(response.validate_for(&request).is_err());
        response.resources.pop();
        response.provider_id = "other".into();
        assert!(response.validate_for(&request).is_err());
    }
}

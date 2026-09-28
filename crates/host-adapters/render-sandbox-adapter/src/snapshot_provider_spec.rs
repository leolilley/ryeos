//! Closed, data-selected Render snapshot-production protocol.
//!
//! These profile bytes must arrive through the same signed, sealed artifact
//! path as lifecycle provider specs before this module may authorize contact.
//! Parsing a fixture by itself grants no provider or credential authority.

use anyhow::{Result, ensure};
use serde::Deserialize;

const MAX_SPEC_BYTES: usize = 8 * 1024;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SnapshotProductionSpec {
    schema: u32,
    provider_profile: String,
    origin_profile: String,
    routes: SnapshotRoutes,
    operations: SnapshotOperations,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SnapshotRoutes {
    upload_token: Vec<String>,
    create_snapshot: Vec<String>,
    get_snapshot: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SnapshotOperations {
    upload_content_type: String,
    upload_path: String,
    create_kind: String,
    create_status: u16,
    get_status: u16,
    readiness_state: String,
}

impl SnapshotProductionSpec {
    pub(crate) fn parse(bytes: &[u8]) -> Result<Self> {
        ensure!(
            !bytes.is_empty() && bytes.len() <= MAX_SPEC_BYTES,
            "snapshot production profile exceeds its bound"
        );
        let profile: Self =
            ryeos_external_execution_contract::from_json_slice_strict(bytes, MAX_SPEC_BYTES)?;
        profile.validate()?;
        Ok(profile)
    }

    fn validate(&self) -> Result<()> {
        ensure!(
            self.schema == 1
                && self.provider_profile == "render_sandbox_v1"
                && self.origin_profile == "render_api_v1"
                && self.routes.upload_token
                    == [
                        "v1",
                        "sandboxes",
                        "{source_sandbox_id}",
                        "files",
                        "upload",
                        "token",
                    ]
                && self.routes.create_snapshot
                    == ["v1", "sandboxes", "{source_sandbox_id}", "snapshots"]
                && self.routes.get_snapshot
                    == [
                        "v1",
                        "sandbox-groups",
                        "{sandbox_group_id}",
                        "snapshots",
                        "{snapshot_id}",
                    ]
                && self.operations.upload_content_type == "application/x-tar"
                && self.operations.upload_path == "/ryeos/guest-runtime"
                && self.operations.create_kind == "filesystem"
                && self.operations.create_status == 202
                && self.operations.get_status == 200
                && self.operations.readiness_state == "available",
            "snapshot production profile selects unsupported behavior"
        );
        Ok(())
    }

    pub(crate) fn upload_path(&self) -> &str {
        &self.operations.upload_path
    }

    pub(crate) fn upload_content_type(&self) -> &str {
        &self.operations.upload_content_type
    }

    pub(crate) fn source_upload_token_path(&self, source_sandbox_id: &str) -> Result<String> {
        ensure!(
            crate::valid_sandbox_id(source_sandbox_id),
            "snapshot source sandbox ID is invalid"
        );
        Ok(render_route(
            &self.routes.upload_token,
            &[("{source_sandbox_id}", source_sandbox_id)],
        ))
    }

    pub(crate) fn create_path(&self, source_sandbox_id: &str) -> Result<String> {
        ensure!(
            crate::valid_sandbox_id(source_sandbox_id),
            "snapshot source sandbox ID is invalid"
        );
        Ok(render_route(
            &self.routes.create_snapshot,
            &[("{source_sandbox_id}", source_sandbox_id)],
        ))
    }

    pub(crate) fn get_path(&self, sandbox_group_id: &str, snapshot_id: &str) -> Result<String> {
        ensure!(
            sandbox_group_id.starts_with("sbg-")
                && sandbox_group_id.len() <= 256
                && sandbox_group_id
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
                && crate::valid_snapshot_id(snapshot_id),
            "snapshot readiness locator is invalid"
        );
        Ok(render_route(
            &self.routes.get_snapshot,
            &[
                ("{sandbox_group_id}", sandbox_group_id),
                ("{snapshot_id}", snapshot_id),
            ],
        ))
    }
}

fn render_route(segments: &[String], bindings: &[(&str, &str)]) -> String {
    let mut path = String::new();
    for segment in segments {
        path.push('/');
        path.push_str(
            bindings
                .iter()
                .find_map(|(placeholder, value)| (segment == placeholder).then_some(*value))
                .unwrap_or(segment),
        );
    }
    path
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> Vec<u8> {
        include_bytes!("../fixtures/snapshot-production-spec.json").to_vec()
    }

    #[test]
    fn only_reviewed_snapshot_protocol_can_be_selected() {
        let profile = SnapshotProductionSpec::parse(&fixture()).unwrap();
        assert_eq!(profile.upload_content_type(), "application/x-tar");
        assert_eq!(profile.upload_path(), "/ryeos/guest-runtime");
        assert_eq!(
            profile.create_path("sbx-exact").unwrap(),
            "/v1/sandboxes/sbx-exact/snapshots"
        );
        assert_eq!(
            profile.source_upload_token_path("sbx-exact").unwrap(),
            "/v1/sandboxes/sbx-exact/files/upload/token"
        );
        assert_eq!(
            profile.get_path("sbg-exact", "snp-exact").unwrap(),
            "/v1/sandbox-groups/sbg-exact/snapshots/snp-exact"
        );
        assert!(profile.create_path("../other").is_err());
        assert!(profile.get_path("sbg-exact", "../other").is_err());
        let value: serde_json::Value = serde_json::from_slice(&fixture()).unwrap();
        for (pointer, replacement) in [
            ("/origin_profile", serde_json::json!("arbitrary_origin")),
            (
                "/operations/upload_content_type",
                serde_json::json!("application/octet-stream"),
            ),
            ("/operations/create_kind", serde_json::json!("runtime")),
            ("/operations/create_status", serde_json::json!(201)),
            ("/routes/get_snapshot/1", serde_json::json!("other-host")),
        ] {
            let mut changed = value.clone();
            *changed.pointer_mut(pointer).unwrap() = replacement;
            assert!(
                SnapshotProductionSpec::parse(&serde_json::to_vec(&changed).unwrap()).is_err(),
                "{pointer}"
            );
        }
        let mut unknown = value;
        unknown["credential_url"] = serde_json::json!("https://elsewhere.example");
        assert!(SnapshotProductionSpec::parse(&serde_json::to_vec(&unknown).unwrap()).is_err());
    }
}

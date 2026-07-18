//! `remote/pull` — fetch arbitrary CAS objects from a remote node.
//!
//! Thin CLI wrapper over typed remote CAS reads. Operator pulls materialize
//! explicit artifacts; they never create unrooted local CAS entries that GC
//! could immediately collect.

use std::sync::Arc;
use std::{ffi::OsStr, ffi::OsString};

use anyhow::{bail, Result};
use serde_json::Value;

use crate::registry::ServiceDescriptor;
use crate::remote::client::RemoteClient;
use ryeos_app::state::AppState;
use ryeos_executor::executor::ServiceAvailability;

fn default_remote() -> String {
    "default".to_string()
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    /// Remote config name.
    #[serde(default = "default_remote")]
    pub remote: String,
    /// Typed object hashes to fetch. Namespace is never inferred.
    #[serde(
        default,
        deserialize_with = "ryeos_runtime::scalar_or_vec::deserialize"
    )]
    pub object_hashes: Vec<String>,
    /// Typed blob hashes to fetch. Namespace is never inferred.
    #[serde(
        default,
        deserialize_with = "ryeos_runtime::scalar_or_vec::deserialize"
    )]
    pub blob_hashes: Vec<String>,
    /// Local directory in which typed artifacts are materialized.
    pub output_dir: String,
}

pub async fn handle(req: Request, state: Arc<AppState>) -> Result<Value> {
    if req.object_hashes.is_empty() && req.blob_hashes.is_empty() {
        anyhow::bail!("object_hashes and blob_hashes must not both be empty");
    }

    let client = RemoteClient::from_named_remote(&state, &req.remote, None)?;
    let presence = client
        .objects_has(&req.object_hashes, &req.blob_hashes)
        .await?;
    let missing = presence
        .missing_object_hashes
        .iter()
        .chain(&presence.missing_blob_hashes)
        .cloned()
        .collect::<Vec<_>>();
    let requested_count = req.object_hashes.len() + req.blob_hashes.len();
    if !missing.is_empty() {
        bail!(
            "remote.pull: {} of {} requested hashes not found on remote: {}",
            missing.len(),
            requested_count,
            missing.join(", ")
        );
    }
    let resp = client.objects_get(&req.object_hashes, &[]).await?;

    let mut fetched = 0usize;
    let mut stored_hashes: Vec<String> = Vec::new();

    let output_dir = std::path::PathBuf::from(&req.output_dir);
    if !output_dir.is_absolute() {
        anyhow::bail!("remote.pull output_dir must be an absolute path");
    }
    let parent_path = output_dir
        .parent()
        .ok_or_else(|| anyhow::anyhow!("remote.pull output_dir has no parent"))?;
    let output_name = output_dir
        .file_name()
        .ok_or_else(|| anyhow::anyhow!("remote.pull output_dir has no final name"))?;
    let parent = lillux::PinnedDirectory::open_or_create(parent_path)?;
    if parent.open_entry(output_name, false)?.is_some() {
        anyhow::bail!("remote.pull output_dir already exists; refusing a partial merge");
    }
    let staging = PinnedStagingDirectory::create(parent)?;

    for entry in &resp.entries {
        match entry.kind.as_str() {
            "object" => {
                let value = entry
                    .value
                    .as_ref()
                    .ok_or_else(|| anyhow::anyhow!("object {} missing value field", entry.hash))?;
                stored_hashes.push(entry.hash.clone());
                fetched += 1;
                let content = serde_json::to_vec_pretty(value)?;
                let name = OsString::from(format!("{}.json", entry.hash));
                if staging
                    .directory()
                    .atomic_create_regular(&name, &content, 0o600)?
                    .is_none()
                {
                    anyhow::bail!("duplicate remote object artifact name");
                }
            }
            "missing_object" | "missing_blob" => anyhow::bail!(
                "remote object disappeared after the preflight presence check: {}",
                entry.hash
            ),
            _ => {}
        }
    }

    for hash in presence.found_blob_hashes {
        let mut output =
            staging
                .directory()
                .open_regular_create(OsStr::new(&hash), true, true, 0o600)?;
        client.stream_blob(&hash, &mut output).await?;
        output.sync_all()?;
        stored_hashes.push(hash);
        fetched += 1;
    }
    staging.directory().try_clone_descriptor()?.sync_all()?;
    staging.publish(output_name)?;

    Ok(serde_json::json!({
        "fetched": fetched,
        "requested": requested_count,
        "hashes": stored_hashes,
    }))
}

struct PinnedStagingDirectory {
    parent: lillux::PinnedDirectory,
    name: OsString,
    directory: Option<lillux::PinnedDirectory>,
}

impl PinnedStagingDirectory {
    fn create(parent: lillux::PinnedDirectory) -> Result<Self> {
        for _ in 0..16 {
            let name = OsString::from(format!(
                ".ryeos-remote-pull-{}.{}",
                std::process::id(),
                rand::random::<u64>()
            ));
            match parent.create_child(&name, 0o700) {
                Ok(directory) => {
                    return Ok(Self {
                        parent,
                        name,
                        directory: Some(directory),
                    });
                }
                Err(error)
                    if error
                        .downcast_ref::<std::io::Error>()
                        .is_some_and(|error| error.kind() == std::io::ErrorKind::AlreadyExists) => {
                }
                Err(error) => return Err(error),
            }
        }
        anyhow::bail!("could not reserve a unique remote.pull staging directory")
    }

    fn directory(&self) -> &lillux::PinnedDirectory {
        self.directory
            .as_ref()
            .expect("staging directory exists until publication")
    }

    fn publish(mut self, target_name: &OsStr) -> Result<()> {
        let publication =
            self.parent
                .rename_child_directory_noreplace(&self.name, target_name, self.directory());
        match publication {
            Ok(()) => {
                self.directory.take();
                Ok(())
            }
            Err(error) => {
                // The rename may be visible even when the parent durability
                // barrier fails. Never let Drop mistake a committed output
                // directory for disposable staging in that state.
                if error.namespace_committed() {
                    self.directory.take();
                }
                Err(error.into())
            }
        }
    }
}

impl Drop for PinnedStagingDirectory {
    fn drop(&mut self) {
        let Some(directory) = self.directory.as_ref() else {
            return;
        };
        if let Err(error) = directory.remove_contents_recursive().and_then(|()| {
            self.parent
                .remove_empty_child_if_same(&self.name, directory)
                .and_then(|removed| {
                    if removed {
                        Ok(())
                    } else {
                        anyhow::bail!("staging directory remained non-empty")
                    }
                })
        }) {
            tracing::warn!(%error, "failed to clean remote.pull staging directory");
        }
    }
}

pub const DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:remote/pull",
    endpoint: "remote.pull",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &["ryeos.execute.service.objects/get"],
    handler: |params, _ctx, state| {
        Box::pin(async move {
            let req: Request = crate::handler_error::parse_request(params)?;
            handle(req, state).await
        })
    },
};

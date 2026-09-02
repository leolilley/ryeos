use std::path::{Path, PathBuf};

#[cfg(test)]
use std::fs;

use anyhow::{Context, Result};

const MAX_DERIVED_VIEW_BYTES: u64 = 1024 * 1024;

fn replace_if_unchanged(
    directory: &lillux::PinnedDirectory,
    name: &std::ffi::OsStr,
    incumbent: Option<&lillux::PinnedRegularFile>,
    bytes: &[u8],
) -> Result<()> {
    let state = incumbent
        .map(|file| {
            let observation = file.observation()?;
            let bytes = file.read_stable_bounded(&observation, MAX_DERIVED_VIEW_BYTES)?;
            Ok::<_, anyhow::Error>((observation, bytes))
        })
        .transpose()?;
    match state {
        Some((observation, incumbent_bytes)) => directory
            .replace_pinned_bytes_if_matches_atomic(
                name,
                incumbent,
                move |current| {
                    let current_observation = current.observation()?;
                    anyhow::ensure!(
                        current_observation.matches_quarantined_incumbent(&observation),
                        "node derived view changed before publication"
                    );
                    anyhow::ensure!(
                        current
                            .read_stable_bounded(&current_observation, MAX_DERIVED_VIEW_BYTES,)?
                            == incumbent_bytes,
                        "node derived-view bytes changed before publication"
                    );
                    Ok(())
                },
                bytes,
                0o600,
            )
            .map_err(anyhow::Error::from),
        None => directory
            .replace_pinned_bytes_if_matches_atomic(name, None, |_| Ok(()), bytes, 0o600)
            .map_err(anyhow::Error::from),
    }
}

struct NodeDerivedViewPaths {
    sync_dir: PathBuf,
    sync_policy: PathBuf,
}

impl NodeDerivedViewPaths {
    fn under(app_root: &Path) -> Self {
        let node = app_root.join(ryeos_engine::AI_DIR).join("node");
        let sync_dir = node.join("sync");
        Self {
            sync_policy: sync_dir.join("policy.yaml"),
            sync_dir,
        }
    }
}

/// Materialize read-only derived views after the complete node-signed policy
/// generation is selected. This function never authors policy: isolation and
/// ingest-ignore authority exist only in `.ai/node/policies/`.
pub(super) fn materialize_node_derived_views(app_root: &Path) -> Result<()> {
    let paths = NodeDerivedViewPaths::under(app_root);
    let node_path = app_root.join(ryeos_engine::AI_DIR).join("node");
    let node = lillux::PinnedDirectory::open_or_create(&node_path)
        .context("pin node namespace for derived-view publication")?;

    let sync = node
        .open_or_create_child(std::ffi::OsStr::new("sync"), 0o700)
        .with_context(|| format!("create sync dir {}", paths.sync_dir.display()))?;
    let policy_yaml = ryeos_state::project_sync::render_effective_sync_policy_yaml(
        ryeos_app::ignore::INGEST_IGNORE_POLICY_RELATIVE,
    );
    let sync_name = std::ffi::OsStr::new("policy.yaml");
    let incumbent_sync = sync.open_pinned_regular(sync_name, false)?;
    replace_if_unchanged(
        &sync,
        sync_name,
        incumbent_sync.as_ref(),
        policy_yaml.as_bytes(),
    )
    .with_context(|| format!("write sync policy {}", paths.sync_policy.display()))?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derived_view_paths_live_under_node_space() {
        let paths = NodeDerivedViewPaths::under(Path::new("/srv/ryeos"));
        assert_eq!(
            paths.sync_policy,
            Path::new("/srv/ryeos/.ai/node/sync/policy.yaml")
        );
    }

    #[test]
    fn init_materializes_only_the_read_only_sync_view() {
        let root = tempfile::tempdir().unwrap();
        let paths = NodeDerivedViewPaths::under(root.path());
        materialize_node_derived_views(root.path()).unwrap();

        let raw = fs::read_to_string(&paths.sync_policy).unwrap();
        assert!(raw.contains(ryeos_app::ignore::INGEST_IGNORE_POLICY_RELATIVE));
        assert!(!root.path().join(".ai/node/policies").exists());
    }
}

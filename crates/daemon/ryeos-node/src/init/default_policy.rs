use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

const DEFAULT_SANDBOX_POLICY: &str = r#"version: 1
mode: disabled
backend:
  kind: bubblewrap
  executable: /usr/bin/bwrap
filesystem:
  readable:
    - "{node_public_identity}"
    - "{daemon_socket}"
    - "{bundle_roots}"
    - "{operator_trusted_keys}"
    - "{verified_code}"
  writable:
    - "{project}"
    - "{checkpoint_dir}"
network:
  mode: host
environment:
  allow:
    - "*"
limits:
  open_files: 1024
  verified_artifact_file_bytes: 67108864
  verified_artifact_total_bytes: 268435456
  verified_artifact_files: 4096
"#;

struct NodeDefaultPaths {
    sandbox_policy: PathBuf,
    ingest_dir: PathBuf,
    ignore_config: PathBuf,
    sync_dir: PathBuf,
    sync_policy: PathBuf,
}

impl NodeDefaultPaths {
    fn under(app_root: &Path) -> Self {
        let node = app_root.join(ryeos_engine::AI_DIR).join("node");
        let ingest_dir = node.join("ingest");
        let sync_dir = node.join("sync");
        Self {
            sandbox_policy: node.join("sandbox.yaml"),
            ignore_config: ingest_dir.join("ignore.yaml"),
            ingest_dir,
            sync_policy: sync_dir.join("policy.yaml"),
            sync_dir,
        }
    }
}

/// Materialize node-owned default policy files in their established init order.
/// User-editable sandbox/ignore files are create-once; the generated sync view
/// is overwritten on every init so it tracks the running binary.
pub(super) fn materialize_node_defaults(app_root: &Path) -> Result<()> {
    let paths = NodeDefaultPaths::under(app_root);

    if !paths.sandbox_policy.exists() {
        lillux::atomic_write_private(&paths.sandbox_policy, DEFAULT_SANDBOX_POLICY.as_bytes())
            .with_context(|| {
                format!(
                    "write default sandbox policy {}",
                    paths.sandbox_policy.display()
                )
            })?;
    }

    if !paths.ignore_config.exists() {
        fs::create_dir_all(&paths.ingest_dir)
            .with_context(|| format!("create ingest dir {}", paths.ingest_dir.display()))?;
        let patterns_yaml = ryeos_app::ignore::builtin_patterns()
            .iter()
            .map(|pattern| format!("  - {:?}", pattern))
            .collect::<Vec<_>>()
            .join("\n");
        let content = format!("patterns:\n{}\n", patterns_yaml);
        fs::write(&paths.ignore_config, content)
            .with_context(|| format!("write ignore config {}", paths.ignore_config.display()))?;
    }

    fs::create_dir_all(&paths.sync_dir)
        .with_context(|| format!("create sync dir {}", paths.sync_dir.display()))?;
    let policy_yaml =
        ryeos_state::project_sync::render_effective_sync_policy_yaml(".ai/node/ingest/ignore.yaml");
    fs::write(&paths.sync_policy, policy_yaml)
        .with_context(|| format!("write sync policy {}", paths.sync_policy.display()))?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_paths_live_under_node_space() {
        let paths = NodeDefaultPaths::under(Path::new("/srv/ryeos"));

        assert_eq!(
            paths.sandbox_policy,
            Path::new("/srv/ryeos/.ai/node/sandbox.yaml")
        );
        assert_eq!(
            paths.ignore_config,
            Path::new("/srv/ryeos/.ai/node/ingest/ignore.yaml")
        );
        assert_eq!(
            paths.sync_policy,
            Path::new("/srv/ryeos/.ai/node/sync/policy.yaml")
        );
    }

    #[test]
    fn sandbox_default_preserves_execution_policy() {
        let policy: ryeos_engine::sandbox::SandboxPolicy =
            serde_yaml::from_str(DEFAULT_SANDBOX_POLICY).unwrap();

        assert_eq!(policy.version, 1);
        assert_eq!(policy.mode, ryeos_engine::sandbox::SandboxMode::Disabled);
        assert_eq!(
            policy.backend.kind,
            ryeos_engine::sandbox::SandboxBackendKind::Bubblewrap
        );
        assert_eq!(policy.backend.executable, Path::new("/usr/bin/bwrap"));
        assert_eq!(
            policy.filesystem.readable,
            vec![
                "{node_public_identity}".to_string(),
                "{daemon_socket}".to_string(),
                "{bundle_roots}".to_string(),
                "{operator_trusted_keys}".to_string(),
                "{verified_code}".to_string(),
            ]
        );
        assert_eq!(
            policy.filesystem.writable,
            vec!["{project}".to_string(), "{checkpoint_dir}".to_string()]
        );
        assert_eq!(
            policy.network.mode,
            ryeos_engine::sandbox::SandboxNetworkMode::Host
        );
        assert_eq!(policy.environment.allow, vec!["*".to_string()]);
        assert_eq!(policy.limits.open_files, Some(1024));
        assert_eq!(policy.limits.verified_artifact_file_bytes, 67_108_864);
        assert_eq!(policy.limits.verified_artifact_total_bytes, 268_435_456);
        assert_eq!(policy.limits.verified_artifact_files, 4_096);
    }
}

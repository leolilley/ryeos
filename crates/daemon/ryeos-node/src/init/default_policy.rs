use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

struct NodeDefaultPaths {
    isolation_policy: PathBuf,
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
            isolation_policy: app_root
                .join(ryeos_engine::AI_DIR)
                .join(ryeos_engine::isolation::ISOLATION_POLICY_RELATIVE_PATH),
            ignore_config: ingest_dir.join("ignore.yaml"),
            ingest_dir,
            sync_policy: sync_dir.join("policy.yaml"),
            sync_dir,
        }
    }
}

/// Materialize node-owned default policy files in their established init order.
/// The isolation policy is create-once. Ingest defaults are reconciled additively:
/// operator rules are preserved while newly shipped safety exclusions are
/// added. The generated sync view is overwritten on every init so it tracks
/// the running binary.
pub(super) fn materialize_node_defaults(app_root: &Path) -> Result<()> {
    let paths = NodeDefaultPaths::under(app_root);

    if !paths.isolation_policy.exists() {
        let policy =
            serde_yaml::to_string(&ryeos_engine::isolation::IsolationPolicy::default_disabled())
                .context("serialize default isolation policy")?;
        lillux::atomic_write_private(&paths.isolation_policy, policy.as_bytes()).with_context(
            || {
                format!(
                    "write default isolation policy {}",
                    paths.isolation_policy.display()
                )
            },
        )?;
    }

    fs::create_dir_all(&paths.ingest_dir)
        .with_context(|| format!("create ingest dir {}", paths.ingest_dir.display()))?;
    let mut ignore_config = if paths.ignore_config.exists() {
        let raw = fs::read_to_string(&paths.ignore_config)
            .with_context(|| format!("read ignore config {}", paths.ignore_config.display()))?;
        serde_yaml::from_str::<ryeos_app::ignore::IgnoreConfig>(&raw)
            .with_context(|| format!("parse ignore config {}", paths.ignore_config.display()))?
    } else {
        ryeos_app::ignore::IgnoreConfig {
            patterns: Vec::new(),
        }
    };
    let mut changed = !paths.ignore_config.exists();
    for pattern in ryeos_app::ignore::builtin_patterns() {
        if !ignore_config.patterns.iter().any(|entry| entry == pattern) {
            ignore_config.patterns.push(pattern.to_string());
            changed = true;
        }
    }
    if changed {
        let content = serde_yaml::to_string(&ignore_config)
            .context("serialize reconciled ingest ignore config")?;
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
            paths.isolation_policy,
            Path::new("/srv/ryeos/.ai/node/isolation.yaml")
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
    fn isolation_default_preserves_execution_policy() {
        let policy = ryeos_engine::isolation::IsolationPolicy::default_disabled();

        assert_eq!(policy.version, 1);
        assert_eq!(
            policy.mode,
            ryeos_engine::isolation::IsolationMode::Disabled
        );
        assert_eq!(policy.backend, None);
        assert_eq!(
            policy.filesystem.readable,
            vec![
                "{node_public_identity}".to_string(),
                "{daemon_socket}".to_string(),
                "{bundle_roots}".to_string(),
                "{node_trusted_keys}".to_string(),
                "{verified_code}".to_string(),
            ]
        );
        assert_eq!(
            policy.filesystem.writable,
            vec!["{project}".to_string(), "{checkpoint_dir}".to_string()]
        );
        assert_eq!(
            policy.network.mode,
            ryeos_engine::isolation::IsolationNetworkMode::Host
        );
        assert_eq!(policy.environment.allow, vec!["*".to_string()]);
        assert_eq!(policy.limits.open_files, Some(1024));
        assert_eq!(policy.limits.stdout_bytes, 8_388_608);
        assert_eq!(policy.limits.stderr_bytes, 8_388_608);
        assert_eq!(policy.limits.verified_artifact_file_bytes, 67_108_864);
        assert_eq!(policy.limits.verified_artifact_total_bytes, 268_435_456);
        assert_eq!(policy.limits.verified_artifact_files, 4_096);
    }

    #[test]
    fn init_adds_new_snapshot_safety_ignores_without_dropping_operator_rules() {
        let root = tempfile::tempdir().unwrap();
        let paths = NodeDefaultPaths::under(root.path());
        fs::create_dir_all(&paths.ingest_dir).unwrap();
        fs::write(
            &paths.ignore_config,
            "patterns:\n  - custom-build-output/\n  - .git/\n",
        )
        .unwrap();

        materialize_node_defaults(root.path()).unwrap();

        let raw = fs::read_to_string(&paths.ignore_config).unwrap();
        let config: ryeos_app::ignore::IgnoreConfig = serde_yaml::from_str(&raw).unwrap();
        assert!(config
            .patterns
            .iter()
            .any(|entry| entry == "custom-build-output/"));
        assert!(config.patterns.iter().any(|entry| entry == ".venv/"));
        assert!(config.patterns.iter().any(|entry| entry == "/.ai/state/"));
        assert!(config.patterns.iter().any(|entry| entry == "/.ai/cache/"));
    }
}

use std::path::{Path, PathBuf};

use anyhow::Context;
use ryeos_app::node_config::NodeConfigSnapshot;
use ryeos_engine::canonical_ref::CanonicalRef;
use ryeos_engine::contracts::SubjectResolutionAuthority;
use ryeos_engine::engine::{EffectiveItemRequest, Engine};
use serde_json::Value;

pub fn snapshot_bundle_roots(snapshot: &NodeConfigSnapshot) -> Vec<PathBuf> {
    snapshot
        .bundles
        .iter()
        .map(|record| record.path.clone())
        .collect()
}

pub fn build_effective_item_engine(
    app_root: &Path,
    project_path: Option<&Path>,
    bundle_roots: &[PathBuf],
) -> anyhow::Result<Engine> {
    let config = ryeos_app::config::Config::load(&ryeos_app::config::ConfigSources {
        app_root: Some(app_root.to_path_buf()),
        ..Default::default()
    })?;
    let isolation = ryeos_app::engine_init::load_locked_registered_isolation(&config.app_root)
        .context("load node isolation policy")?;
    let policy = crate::node_descriptors::load_verified_policy_snapshot(app_root)
        .context("load exact node execution policy")?;
    let execution_policy = policy.require::<
        ryeos_app::node_policy::sections::execution::NodeExecutionAdmissionPolicy,
    >()?;

    ryeos_app::engine_init::build_registered_engine_for_roots(
        &config,
        bundle_roots,
        project_path,
        None,
        isolation,
        execution_policy,
    )
    .context("build effective-item engine")
}

pub fn resolve_effective_composed_value(
    engine: &Engine,
    execute_ref: &str,
    project_path: Option<&Path>,
) -> anyhow::Result<Option<Value>> {
    let item_ref = CanonicalRef::parse(execute_ref)
        .with_context(|| format!("parse canonical ref '{execute_ref}'"))?;
    let item = engine
        .effective_item(EffectiveItemRequest {
            item_ref,
            expected_kind: None,
            project_root: project_path.map(Path::to_path_buf),
            subject_resolution_authority: SubjectResolutionAuthority::for_live_project_root(
                project_path,
            ),
        })
        .with_context(|| format!("resolve effective metadata for '{execute_ref}'"))?;
    Ok(Some(item.composed_value))
}

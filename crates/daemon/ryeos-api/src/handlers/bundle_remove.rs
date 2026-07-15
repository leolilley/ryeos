//! `bundle.remove` — remove a bundle via node-config deletion.
//!
//! Deletes the signed `kind: node` item at `<app_root>/.ai/node/bundles/<name>.yaml`
//! and the bundle directory at `<app_root>/.ai/bundles/<name>/`.
//!
//! OfflineOnly: the daemon must be stopped (engine reload not implemented).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use serde_json::Value;

use ryeos_bundle::plan::{build_plan, BundleAction, BundlePlan, BundlePlanMode};
use ryeos_engine::trust::TrustStore;

use crate::registry::ServiceDescriptor;
use ryeos_app::state::AppState;
use ryeos_executor::executor::ServiceAvailability;

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    /// Bundle name (directory under `<app_root>/.ai/bundles/`).
    pub name: String,
}

pub async fn handle(req: Request, state: Arc<AppState>) -> Result<Value> {
    if req.name.is_empty() || req.name.len() > 64 {
        bail!(
            "invalid bundle name '{}': must be 1–64 characters",
            req.name
        );
    }
    if !req
        .name
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
    {
        bail!(
            "invalid bundle name '{}': must contain only lowercase letters, digits, underscore, or hyphen",
            req.name
        );
    }

    // The registry guard remains held through reconciliation, exact
    // post-removal planning/admission, and the final namespace mutation. It is
    // always acquired before the per-name transaction lock.
    let registry_lock =
        ryeos_app::bundle_transaction::BundleRegistryMutationLock::acquire(&state.config.app_root)?;
    let transaction = registry_lock.acquire_bundle(&req.name)?;
    let recovered = match transaction.reconcile(state.identity.signing_key()) {
        Ok(recovered) => recovered,
        Err(error) => {
            // Reconciliation can mutate registration/tree state before a later
            // durability or journal-cleanup error. Conservatively invalidate
            // the cache before surfacing that failure.
            state.engine_cache.bump_system_install_generation();
            return Err(error).context("reconcile interrupted bundle transaction");
        }
    };
    if recovered.is_some() {
        let new_gen = state.engine_cache.bump_system_install_generation();
        tracing::info!(
            bundle = %req.name,
            engine_cache_generation = new_gen,
            operation = ?recovered,
            "reconciled bundle transaction: bumped engine cache generation"
        );
    }

    // Delete the signed node-config item
    let config_item_path = state
        .config
        .app_root
        .join(".ai")
        .join("node")
        .join("bundles")
        .join(format!("{}.yaml", req.name));

    // Delete the bundle directory
    let bundle_dir = state
        .config
        .app_root
        .join(".ai")
        .join("bundles")
        .join(&req.name);

    let registered = config_item_path.exists();
    let installed_tree = bundle_dir.exists();
    let recovered_remove = matches!(
        recovered,
        Some(ryeos_app::bundle_transaction::BundleOperation::Remove)
    );

    if !registered && !installed_tree {
        return Ok(serde_json::json!({
            "name": req.name,
            "removed_config_item": recovered_remove,
            "removed_dir": recovered_remove,
        }));
    }
    if registered != installed_tree {
        bail!(
            "bundle '{}' has inconsistent installed state (registration: {}, tree: {}); refusing unplanned removal",
            req.name,
            registered,
            installed_tree
        );
    }

    let plan = build_prospective_remove_plan(&state.config.app_root, &req.name)?;
    let prospective_validator = state
        .extensions
        .get::<ryeos_app::prospective_admission::ProspectiveNodeConfigValidator>()
        .context("prospective node-config validator is not installed at the composition root")?;
    admit_prospective_remove(
        &state.config.app_root,
        &req.name,
        &plan,
        &state.engine.node_trust_store,
        &prospective_validator,
        Arc::clone(&state.sandbox),
    )?;

    // Invalidate before any new journal or namespace mutation. Reconciliation
    // above may also have completed a previous operation, and `commit_absent` removes
    // registration and tree durably in sequence, so an error can otherwise
    // leave a partially removed namespace while cached engines remain live.
    // A conservative bump when the operation later fails is harmless.
    let new_gen = state.engine_cache.bump_system_install_generation();
    tracing::info!(
        bundle = %req.name,
        engine_cache_generation = new_gen,
        "bundle removal begun: bumped engine cache generation"
    );
    transaction.begin_remove()?;
    transaction.commit_absent()?;

    Ok(serde_json::json!({
        "name": req.name,
        "removed_config_item": true,
        "removed_dir": true,
    }))
}

fn build_prospective_remove_plan(app_root: &Path, bundle_name: &str) -> Result<BundlePlan> {
    let installed = ryeos_bundle::installed::load_installed_plan_inputs(app_root)
        .context("load verified installed bundle graph")?;
    let removal = installed
        .iter()
        .find(|input| input.name == bundle_name)
        .cloned()
        .with_context(|| format!("bundle '{bundle_name}' is not registered as installed"))?;
    build_plan(BundlePlanMode::Remove, &[removal], &installed)
        .context("resolve exact prospective removal graph")
}

fn admit_prospective_remove(
    app_root: &Path,
    bundle_name: &str,
    plan: &BundlePlan,
    node_trust_store: &TrustStore,
    prospective_validator: &ryeos_app::prospective_admission::ProspectiveNodeConfigValidator,
    sandbox: Arc<ryeos_engine::sandbox::SandboxRuntime>,
) -> Result<()> {
    let removal = plan
        .bundles
        .get(bundle_name)
        .with_context(|| format!("prospective removal plan omitted bundle '{bundle_name}'"))?;
    if removal.action != BundleAction::Remove {
        bail!(
            "prospective removal plan did not mark bundle '{}' for removal",
            bundle_name
        );
    }

    let prospective_roots: Vec<PathBuf> = plan
        .bundles
        .values()
        .filter(|bundle| bundle.action != BundleAction::Remove)
        .map(|bundle| bundle.source.root_path().clone())
        .collect();
    ryeos_app::engine_init::admit_node_bundle_roots(&prospective_roots, node_trust_store, sandbox)
        .context("prospective removal would fail node engine boot")?;

    let loader = ryeos_app::node_config::loader::BootstrapLoader {
        app_root,
        trust_store: node_trust_store,
    };
    let mut current_records: std::collections::BTreeMap<
        String,
        ryeos_app::node_config::BundleRecord,
    > = loader
        .load_bundle_section()
        .context("load current node bundle registrations for prospective removal")?
        .into_iter()
        .map(|record| (record.name.clone(), record))
        .collect();
    let prospective_records = plan
        .bundles
        .iter()
        .filter(|(_, bundle)| bundle.action != BundleAction::Remove)
        .map(|(name, bundle)| {
            let record = current_records.remove(name).with_context(|| {
                format!("prospective bundle '{name}' has no verified current registration")
            })?;
            if &record.path != bundle.source.root_path() {
                bail!(
                    "prospective bundle '{}' registry path mismatch: registration {}, plan {}",
                    name,
                    record.path.display(),
                    bundle.source.root_path().display()
                );
            }
            Ok(record)
        })
        .collect::<Result<Vec<_>>>()?;
    let snapshot = loader
        .load_full_prospective(
            &ryeos_app::node_config::SectionTable::new(),
            &prospective_records,
        )
        .context("prospective removal would fail full node-config boot")?;
    prospective_validator
        .validate(&snapshot)
        .context("prospective removal would fail composed node-config admission")?;
    Ok(())
}

pub const DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:bundle/remove",
    endpoint: "bundle.remove",
    availability: ServiceAvailability::OfflineOnly,
    required_caps: &["ryeos.execute.service.bundle/remove"],
    handler: |params, _ctx, state| {
        Box::pin(async move {
            let req: Request =
                serde_json::from_value(params).context("bundle.remove requires { name }")?;
            handle(req, state).await
        })
    },
};

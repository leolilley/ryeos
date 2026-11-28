//! `bundle.install` — install a downstream bundle via node-config writer.
//!
//! Copies source to `<app_root>/.ai/bundles/<name>/`, writes a signed
//! `kind: node` bundle registration item at `<app_root>/.ai/node/bundles/<name>.yaml`.
//!
//! Any bundle name is accepted — no special treatment for any name.
//!
//! Stopped-node only: the daemon must be stopped (engine reload not implemented).

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use serde_json::Value;

use ryeos_bundle::plan::{
    BundlePlan, BundlePlanMode, BundleSource, PlanInput, VerificationSubjectKind, build_plan,
};
use ryeos_bundle::preflight::{
    preflight_verify_bundle_staging_in_context, preflight_verify_named_bundle_in_context,
};
use ryeos_engine::trust::TrustStore;

use crate::registry::ServiceDescriptor;
use ryeos_app::state::AppState;
use ryeos_executor::executor::ServiceAvailability;

/// What admission needs to check a bundle that arrived through a publication:
/// its lane marker, this node's kinds, and what the node's `publication_lane`
/// policy admits for it.
pub(crate) struct LaneAdmission<'a> {
    pub marker: &'a ryeos_engine::trust::BundleLane,
    pub kinds: &'a ryeos_engine::kind_registry::KindRegistry,
    pub grants: ryeos_bundle::lane::LaneGrants,
}

/// The lane an install registers its bundle in: the marker the node signs into
/// the registration, and what the policy admits for the bundle.
#[derive(Debug, Clone)]
pub struct LaneInstall {
    pub marker: ryeos_engine::trust::BundleLane,
    pub grants: ryeos_bundle::lane::LaneGrants,
}

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    /// Bundle name; becomes the install directory name.
    pub name: String,
    /// Source directory to copy from.
    pub source_path: PathBuf,
    /// Replace an existing installed bundle after preflight verification.
    #[serde(default)]
    pub replace: bool,
    /// Preserve installed runtime artifacts when replacing from a sparse source.
    ///
    /// This keeps `.ai/bin`, `.ai/objects`, and `.ai/refs` from the existing
    /// installation when the replacement source does not provide them. Source
    /// files always win when present.
    #[serde(default)]
    pub preserve_runtime_artifacts: bool,
    /// Installed bindings (`<section>/<id>`) the replacement may leave
    /// pinning artifacts it removes. A replacement that strands any other
    /// binding is refused with `replacement_strands_bindings`; naming one it
    /// does not strand is refused with `invalidated_binding_not_stranded`.
    #[serde(default)]
    pub invalidate_bindings: Vec<ryeos_app::node_config::coherence::BindingRef>,
}

/// A public route that a running node's policy does not name is not served.
pub(crate) const PUBLIC_ROUTE_NOT_NAMED: &str = "public_route_not_named";
/// A route signed public must be keyless and static.
pub(crate) const PUBLIC_ROUTE_NOT_KEYLESS_STATIC: &str = "public_route_not_keyless_static";

/// Refuse a bundle whose routes signed `public: true` the node would not
/// serve: one that is not keyless and static (the boot table refuses it), or
/// one the node's current `public_routes` policy does not name. Every such
/// route is named in the refusal; an incomplete list is refused whole.
fn refuse_unadmitted_public_routes(
    routes: &[ryeos_engine::route_raw::RawRouteSpec],
    candidate_root: &Path,
    policy: &ryeos_node_runtime::node_policy::sections::public_routes::PublicRoutesPolicy,
) -> Result<()> {
    let candidate: Vec<&ryeos_engine::route_raw::RawRouteSpec> = routes
        .iter()
        .filter(|route| route.public && route.source_file.starts_with(candidate_root))
        .collect();
    let mut not_keyless_static: Vec<String> = candidate
        .iter()
        .filter(|route| route.auth != "none" || route.response.mode != "static")
        .map(|route| route.id.clone())
        .collect();
    not_keyless_static.sort();
    if !not_keyless_static.is_empty() {
        return Err(ryeos_app::handler_error::HandlerError::Structured {
            code: PUBLIC_ROUTE_NOT_KEYLESS_STATIC.to_owned(),
            status: 422,
            body: serde_json::json!({
                "code": PUBLIC_ROUTE_NOT_KEYLESS_STATIC,
                "error": "these routes are signed public but are not keyless static routes, so a node refuses its route table with them",
                "routes": not_keyless_static,
            }),
        }
        .into());
    }
    let mut not_named: Vec<String> = candidate
        .iter()
        .filter(|route| policy.limits_for(&route.id).is_none())
        .map(|route| route.id.clone())
        .collect();
    not_named.sort();
    if !not_named.is_empty() {
        return Err(ryeos_app::handler_error::HandlerError::Structured {
            code: PUBLIC_ROUTE_NOT_NAMED.to_owned(),
            status: 422,
            body: serde_json::json!({
                "code": PUBLIC_ROUTE_NOT_NAMED,
                "error": "these routes are signed public, and this node's public_routes policy does not name them; name every one with its limits before installing the bundle",
                "routes": not_named,
            }),
        }
        .into());
    }
    Ok(())
}

/// Refuse a staged bundle generation that strands an installed binding the
/// operator did not name, as a typed HTTP 409.
fn check_staged_bindings(
    admitted: &AdmittedStaging,
    node_trust_store: &TrustStore,
    invalidate: &[ryeos_app::node_config::coherence::BindingRef],
) -> Result<Vec<ryeos_app::node_config::coherence::BindingRef>> {
    ryeos_app::node_config::coherence::check_bundle_replacement(
        &admitted.snapshot,
        &admitted.current,
        &admitted.prospective,
        node_trust_store,
        invalidate,
    )
    .map_err(|error| {
        match error.downcast_ref::<ryeos_app::node_config::coherence::ReplacementBindingRefusal>() {
            Some(refusal) => refusal.handler_error().into(),
            None => error,
        }
    })
}

pub fn validate_name(name: &str) -> Result<()> {
    if name.is_empty() || name.len() > 64 {
        bail!("invalid bundle name '{}': must be 1–64 characters", name);
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
    {
        bail!(
            "invalid bundle name '{}': must contain only lowercase letters, digits, underscore, or hyphen",
            name
        );
    }
    Ok(())
}

/// What a bundle install acts under. The live or standalone service takes it
/// from its state; the daemon's startup, applying a signed
/// `node.bundle.replace`, composes it from the stopped node's configuration.
pub struct InstallAuthority<'a> {
    pub app_root: &'a Path,
    /// The node's signing key, which signs the bundle registration.
    pub node_signing_key: &'a lillux::crypto::SigningKey,
    pub node_trust_store: &'a TrustStore,
    /// The node's kind registry, which the lane audit reads activations from.
    pub kinds: &'a ryeos_engine::kind_registry::KindRegistry,
    pub prospective_validator: &'a ryeos_app::prospective_admission::ProspectiveNodeConfigValidator,
    pub isolation: Arc<ryeos_engine::isolation::IsolationRuntime>,
    /// The state lock the registration reseal is published under.
    pub state_lock: &'a ryeos_node_runtime::state_lock::StateLockLease,
    /// Called whenever a new bundle generation may be visible, so a process
    /// that caches engines drops them.
    pub generation_changed: &'a dyn Fn(&str),
}

pub async fn handle(req: Request, state: Arc<AppState>) -> Result<Value> {
    let prospective_validator = state
        .extensions
        .get::<ryeos_app::prospective_admission::ProspectiveNodeConfigValidator>()
        .context("prospective node-config validator is not installed at the composition root")?;
    let bump = |reason: &str| {
        let new_gen = state.engine_cache.bump_system_install_generation();
        tracing::info!(
            bundle = %req.name,
            engine_cache_generation = new_gen,
            reason,
            "bumped engine cache generation"
        );
    };
    install(
        &req,
        &InstallAuthority {
            app_root: &state.config.app_root,
            node_signing_key: state.identity.signing_key(),
            node_trust_store: &state.engine.node_trust_store,
            kinds: &state.engine.kinds,
            prospective_validator: &prospective_validator,
            isolation: Arc::clone(&state.isolation),
            state_lock: &state.controller_lifetime,
            generation_changed: &bump,
        },
    )
}

/// The bundle is installed and the request did not say to replace it. Typed
/// so the staged-apply path can report it as its own closed refusal while the
/// command-line install keeps its plain message.
#[derive(Debug)]
pub struct BundleAlreadyInstalledError {
    name: String,
    path: PathBuf,
}

impl BundleAlreadyInstalledError {
    pub fn name(&self) -> &str {
        &self.name
    }
}

impl std::fmt::Display for BundleAlreadyInstalledError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "bundle '{}' already installed at {}; use --replace to update it from source",
            self.name,
            self.path.display()
        )
    }
}

impl std::error::Error for BundleAlreadyInstalledError {}

/// Extra checks an install runs on its own copy of the bundle, beyond node
/// admission. Each runs on the bytes the install itself holds, never on the
/// source, so what is checked is what is activated.
pub struct InstallChecks<'a> {
    /// On the completed staging copy (`.<name>.staging`), before admission.
    pub staged: &'a dyn Fn(&Path) -> Result<()>,
    /// On the tree after it is moved into place, before the registration is
    /// committed. A refusal here is undone through the install journal: the
    /// moved tree is put back and the journal is abandoned, so the node is as
    /// it was and a registered bundle is never removed.
    pub activated: &'a dyn Fn(&Path) -> Result<()>,
    /// On the installed tree when a rerun finds an interrupted install that
    /// reconciliation completed. The registration set is already sealed; a
    /// refusal here leaves the completed install in place and sealed.
    pub converged: &'a dyn Fn(&Path) -> Result<()>,
    /// Maps a failure of node admission of the staging copy to the error this
    /// install reports.
    pub admission_error: &'a dyn Fn(anyhow::Error) -> anyhow::Error,
    /// The lane the bundle is registered in, with what admits it. `None` is a
    /// native install.
    pub lane: Option<LaneInstall>,
}

/// The replacement guard, read from the registrations without the lane
/// audit: a laned bundle is never replaced, and a laned install never takes a
/// name a native install holds, so a laned apply with `replace` cannot write a
/// lane marker over a native registration.
fn replacement_guard(
    app_root: &Path,
    trust_store: &TrustStore,
    name: &str,
    laned: bool,
) -> Result<()> {
    ryeos_app::bundle_transaction::refuse_laned_replacement(app_root, trust_store, name)?;
    if laned {
        ryeos_app::bundle_transaction::refuse_native_holder(app_root, trust_store, name)?;
    }
    Ok(())
}

fn no_check(_: &Path) -> Result<()> {
    Ok(())
}

fn keep_error(error: anyhow::Error) -> anyhow::Error {
    error
}

impl InstallChecks<'static> {
    /// No extra check: the command-line install and archive replacement.
    pub fn none() -> Self {
        Self {
            staged: &no_check,
            activated: &no_check,
            converged: &no_check,
            admission_error: &keep_error,
            lane: None,
        }
    }
}

/// What an install needs, with node admission and the registration reseal
/// supplied as functions so the install's own steps can be exercised without
/// a composed node.
struct InstallEnv<'a> {
    app_root: &'a Path,
    node_signing_key: &'a lillux::crypto::SigningKey,
    generation_changed: &'a dyn Fn(&str),
    /// Admit the completed staging copy; returns the bindings it invalidates.
    admit: &'a dyn Fn(&Path, bool) -> Result<Vec<ryeos_app::node_config::coherence::BindingRef>>,
    /// Refuse replacing the named bundle: always when its registration is in
    /// a lane, and, when the install is itself laned (the flag), also when a
    /// native install registered it. Runs under the registry lock, where the
    /// installed set is read.
    refuse_replace: &'a dyn Fn(&str, bool) -> Result<()>,
    /// Reseal the init completion over the changed registration set.
    seal: &'a dyn Fn() -> Result<()>,
}

/// Install or replace one bundle from a source directory under `authority`.
pub fn install(req: &Request, authority: &InstallAuthority<'_>) -> Result<Value> {
    install_with_checks(req, authority, &InstallChecks::none())
}

/// [`install`] with extra checks on the install's own copy (see
/// [`InstallChecks`]).
pub fn install_with_checks(
    req: &Request,
    authority: &InstallAuthority<'_>,
    checks: &InstallChecks<'_>,
) -> Result<Value> {
    let lane_admission = checks.lane.as_ref().map(|lane| LaneAdmission {
        marker: &lane.marker,
        kinds: authority.kinds,
        grants: lane.grants.clone(),
    });
    let refuse_replace = |name: &str, laned: bool| {
        replacement_guard(authority.app_root, authority.node_trust_store, name, laned)
    };
    let admit = |staging: &Path, replace: bool| {
        let admitted = admit_completed_staging(
            authority.app_root,
            &req.name,
            staging,
            replace,
            authority.node_trust_store,
            authority.prospective_validator,
            Arc::clone(&authority.isolation),
            lane_admission.as_ref(),
        )
        .context("admission refused completed install staging tree")?;
        check_staged_bindings(
            &admitted,
            authority.node_trust_store,
            &req.invalidate_bindings,
        )
    };
    let seal = || {
        ryeos_node::seal_init_completion_after_bundle_registration_change(
            authority.app_root,
            authority.state_lock,
        )
        .context("commit bundle registration into initialized state")
    };
    install_core(
        req,
        &InstallEnv {
            app_root: authority.app_root,
            node_signing_key: authority.node_signing_key,
            generation_changed: authority.generation_changed,
            admit: &admit,
            refuse_replace: &refuse_replace,
            seal: &seal,
        },
        checks,
    )
}

fn install_core(req: &Request, env: &InstallEnv<'_>, checks: &InstallChecks<'_>) -> Result<Value> {
    validate_name(&req.name)?;

    if !req.source_path.is_dir() {
        bail!(
            "source_path is not a directory: {}",
            req.source_path.display()
        );
    }

    let app_root = env.app_root;
    let bundles_root = app_root.join(".ai").join("bundles");
    let target = bundles_root.join(&req.name);
    // Keep the exact installed-set read, admission, and namespace mutation in
    // one node-wide critical section. The registry lock always precedes the
    // per-name transaction lock.
    let registry_lock =
        ryeos_node_runtime::registry::BundleRegistryMutationLock::acquire(app_root)?;
    let transaction = ryeos_app::bundle_transaction::BundleMutationTransaction::acquire(
        &registry_lock,
        &req.name,
    )?;
    let recovered = match transaction.reconcile(env.node_signing_key) {
        Ok(recovered) => recovered,
        Err(error) => {
            // Reconciliation may finish the tree/registration mutation and
            // then fail while durably removing its journal. Invalidate before
            // returning so a namespace-visible repair never leaves old engines
            // cached in this daemon process.
            (env.generation_changed)("reconcile failed");
            return Err(error).context("reconcile interrupted bundle transaction");
        }
    };
    if recovered.is_some() {
        (env.generation_changed)("reconciled bundle transaction");
        // Reconciliation may have committed or removed a registration. The
        // registration set is pinned by the signed init completion, so it is
        // resealed here, unconditionally and before anything else can refuse:
        // a registration that is real must never be left unsealed, because a
        // node whose registrations differ from its sealed record cannot boot.
        // The reseal rewrites the record from current state, so repeating it
        // is harmless.
        (env.seal)()?;
    }
    if matches!(
        recovered,
        Some(
            ryeos_app::bundle_transaction::BundleOperation::Install
                | ryeos_app::bundle_transaction::BundleOperation::RemoteInstall
        )
    ) && transaction.target().is_dir()
        && !req.replace
    {
        // The completed install is already sealed. Whether it is the bundle
        // this caller meant is decided after, and a refusal leaves it
        // installed and sealed.
        (checks.converged)(transaction.target())?;
        return Ok(serde_json::json!({
            "name": req.name,
            "path": transaction.target(),
            "recovered": true,
        }));
    }

    let replaced = target.exists();
    if replaced && !req.replace {
        return Err(anyhow::Error::new(BundleAlreadyInstalledError {
            name: req.name.clone(),
            path: target.clone(),
        }));
    }

    if replaced {
        (env.refuse_replace)(&req.name, checks.lane.is_some())?;
    }

    // Trust comes only from the node's persistent trust store. Admission runs
    // against the completed staging tree below, never the mutable source tree.

    fs::create_dir_all(&bundles_root)
        .with_context(|| format!("failed to create bundles root {}", bundles_root.display()))?;

    let mut registration = serde_json::json!({ "kind": "node", "path": target });
    if let Some(lane) = &checks.lane {
        registration["lane"] =
            serde_json::to_value(&lane.marker).context("serialize bundle lane marker")?;
    }
    let mut invalidated_bindings = Vec::new();
    let verify_staged = |staging: &Path,
                         replace: bool|
     -> Result<Vec<ryeos_app::node_config::coherence::BindingRef>> {
        (checks.staged)(staging)?;
        (env.admit)(staging, replace).map_err(checks.admission_error)
    };
    let activation = if replaced {
        replace_dir_atomic(
            &req.source_path,
            &target,
            req.preserve_runtime_artifacts,
            &transaction,
            registration.clone(),
            |staging| {
                invalidated_bindings = verify_staged(staging, true)?;
                Ok(())
            },
            checks.activated,
        )
        .with_context(|| {
            format!(
                "failed to replace bundle from {} to {}",
                req.source_path.display(),
                target.display()
            )
        })
    } else {
        install_dir_atomic(
            &req.source_path,
            &target,
            &transaction,
            registration.clone(),
            |staging| {
                invalidated_bindings = verify_staged(staging, false)?;
                Ok(())
            },
            checks.activated,
        )
        .with_context(|| {
            format!(
                "failed to install bundle from {} to {}",
                req.source_path.display(),
                target.display()
            )
        })
    };

    // Invalidate cached engines as soon as a generation may be visible. This
    // deliberately happens before propagating a post-rename/exchange journal
    // error: activation can succeed even when `mark_activated` or its durable
    // journal write fails. A conservative bump for a failed replacement that
    // left the old target unchanged is harmless.
    if target.is_dir() {
        (env.generation_changed)("bundle namespace observed after activation attempt");
    }
    activation?;

    let canonical_target = target
        .canonicalize()
        .context("failed to canonicalize installed bundle path")?;

    // Write signed kind: node bundle registration
    let config_item_path = transaction
        .commit_present(env.node_signing_key)
        .context("commit bundle registration")?;
    // The registration set is pinned by the signed init completion record;
    // reseal it so the node with this bundle is startable and the next
    // stopped-node operation is admitted.
    (env.seal)()?;

    let report = serde_json::json!({
        "name": req.name,
        "path": canonical_target.display().to_string(),
        "config_item": config_item_path.display().to_string(),
        "replaced": replaced,
        "preserve_runtime_artifacts": req.preserve_runtime_artifacts,
        "invalidated_bindings": invalidated_bindings,
    });
    Ok(report)
}

/// Install a verified stage under the name and tree digest its signed record
/// carries, over an installed bundle of that name only when `replace`.
///
/// The bytes come from the stage's `tree/` directory, but nothing about them is
/// trusted at the moment of copying: the install copies them into its own
/// staging directory and recomputes the tree digest on that copy before
/// admission, so the tree that is verified is the tree that is activated. The
/// signers are checked against the node's trust store as it stands now, by the
/// same admission a command-line install runs. After the tree is moved into
/// place the digest is recomputed once more, before the registration is
/// committed. Only the record's `bundle_name` and `tree_digest` are used, and
/// both are re-derived from the bytes.
///
/// Refusals are [`ryeos_node::bundle_staging::apply::ApplyRefusal`] values in
/// the error chain; anything else is a fault of the environment.
pub fn install_staged(
    staged: &ryeos_node::bundle_staging::StagedBundle,
    evidence: &std::collections::BTreeMap<String, Vec<u8>>,
    replace: bool,
    authority: &InstallAuthority<'_>,
) -> Result<Value> {
    use ryeos_node::bundle_staging::apply::{ApplyRefusal, ApplyRefusalCode};

    let name = staged.record.bundle_name.as_str();
    let expected = staged.record.tree_digest.as_str();
    // A stage from a publication is installed in the publication lane, under
    // what the policy admits as it stands now. Everything the lane needs is
    // re-derived from the evidence bytes the verified stage returned.
    let lane = super::stage_lane::prepare_apply_lane(staged, evidence, authority)?;
    let limits = ryeos_node::bundle_staging::TreeLimits {
        max_files: super::bundle_export::MAX_EXPORTED_BUNDLE_FILES,
        max_bytes: super::bundle_export::MAX_EXPORTED_BUNDLE_TOTAL_BYTES,
    };
    let verify_copy = |tree: &Path, mismatch: ApplyRefusalCode| -> Result<()> {
        verify_tree_digest(name, tree, expected, &limits, mismatch)
    };
    let staged_check = |copy: &Path| -> Result<()> {
        // The copy's directories took the daemon's umask; the digest accepts
        // only directories no group or other can write.
        lillux::set_tree_directory_modes(copy).context("set the install copy's directory modes")?;
        verify_copy(copy, ApplyRefusalCode::TreeDigestMismatch)?;
        // The publication-lane audit runs on this same copy as the first step
        // of admission, after the digest check above: the tree that is
        // audited is the tree that is activated. It takes its inputs from
        // `stage.evidence` (see `prepare_apply_lane`), never from the files.
        Ok(())
    };
    let activated_check = |target: &Path| -> Result<()> {
        verify_copy(target, ApplyRefusalCode::TreeChangedDuringInstall)
    };
    let converged_check = |target: &Path| -> Result<()> {
        // A rerun refuses a tree that is not the stage's own bytes: any
        // other tree under this name is an install somebody else made. The
        // registration it completed has already been sealed.
        verify_tree_digest(
            name,
            target,
            expected,
            &limits,
            ApplyRefusalCode::BundleAlreadyInstalled,
        )
        .map_err(|error| match ApplyRefusal::find(&error) {
            Some(refusal) => anyhow::Error::new(ApplyRefusal::new(
                ApplyRefusalCode::BundleAlreadyInstalled,
                format!(
                    "an interrupted install of `{name}` was completed, and the installed tree is not this stage: {}",
                    refusal.detail()
                ),
            )),
            None => error,
        })
    };
    let admission_error = |error: anyhow::Error| -> anyhow::Error {
        if ApplyRefusal::find(&error).is_some() {
            error
        } else if super::stage_lane::is_lane_audit_refusal(&error) {
            anyhow::Error::new(ApplyRefusal::new(
                ApplyRefusalCode::LaneRefused,
                format!("{error:#}"),
            ))
        } else {
            anyhow::Error::new(ApplyRefusal::new(
                ApplyRefusalCode::AdmissionRefused,
                format!("{error:#}"),
            ))
        }
    };
    if let Some(report) = converge_on_installed_stage(name, expected, &limits, authority)? {
        return Ok(report);
    }
    let result = install_with_checks(
        &Request {
            name: name.to_owned(),
            source_path: staged.tree(),
            replace,
            preserve_runtime_artifacts: false,
            // A replacement that would strand a binding refuses.
            invalidate_bindings: Vec::new(),
        },
        authority,
        &InstallChecks {
            staged: &staged_check,
            activated: &activated_check,
            converged: &converged_check,
            admission_error: &admission_error,
            lane: lane.clone(),
        },
    );
    let result = result.map(|mut report| {
        if let (Some(lane), Some(object)) = (&lane, report.as_object_mut()) {
            object.insert(
                "lane".to_owned(),
                super::stage_lane::applied_lane_report(lane),
            );
        }
        report
    });
    result.map_err(
        |error| match error.downcast_ref::<BundleAlreadyInstalledError>() {
            Some(installed) => anyhow::Error::new(ApplyRefusal::new(
                ApplyRefusalCode::BundleAlreadyInstalled,
                format!(
                    "bundle `{}` is installed and the request does not replace it",
                    installed.name()
                ),
            )),
            None => match ryeos_app::handler_error::extract_handler_error(&error) {
                // A laned bundle is only removed, never replaced.
                Some(crate::handler_error::HandlerError::Structured { code, .. })
                    if code == "bundle_lane_not_replaceable"
                        || code == "bundle_name_held_by_native_lane" =>
                {
                    anyhow::Error::new(ApplyRefusal::new(
                        ApplyRefusalCode::LaneNameRefused,
                        format!("{error:#}"),
                    ))
                }
                _ => error,
            },
        },
    )
}

/// A rerun after the node died past the registration commit: the bundle is
/// registered, no bundle transaction is pending for it, and the installed tree
/// hashes to the stage's digest. The registration set is then resealed, which
/// is idempotent, and nothing is copied. Any other state is not convergence:
/// `None` sends the apply down the install path.
fn converge_on_installed_stage(
    name: &str,
    expected: &str,
    limits: &ryeos_node::bundle_staging::TreeLimits,
    authority: &InstallAuthority<'_>,
) -> Result<Option<Value>> {
    use ryeos_node::bundle_staging::StagingRefusal;

    let app_root = authority.app_root;
    let target = app_root
        .join(ryeos_engine::AI_DIR)
        .join("bundles")
        .join(name);
    let registration = app_root
        .join(ryeos_engine::AI_DIR)
        .join("node/bundles")
        .join(format!("{name}.yaml"));
    if !target.is_dir() || !registration.is_file() {
        return Ok(None);
    }
    let _registry_lock =
        ryeos_node_runtime::registry::BundleRegistryMutationLock::acquire(app_root)?;
    let diagnostics = ryeos_app::bundle_transaction::inspect_bundle_transactions(app_root)
        .context("inspect pending bundle transactions")?;
    if diagnostics.pending.iter().any(|pending| pending == name)
        || diagnostics.invalid.iter().any(|invalid| invalid == name)
    {
        return Ok(None);
    }
    match ryeos_node::bundle_staging::tree_digest_from_dir(name, &target, limits) {
        Ok(summary) if summary.digest == expected => {}
        Ok(_) | Err(StagingRefusal::Invalid(_)) => return Ok(None),
        Err(fault) => return Err(anyhow::Error::new(fault)),
    }
    ryeos_node::seal_init_completion_after_bundle_registration_change(
        app_root,
        authority.state_lock,
    )
    .context("commit bundle registration into initialized state")?;
    Ok(Some(serde_json::json!({
        "name": name,
        "path": target.display().to_string(),
        "converged": true,
    })))
}

/// Recompute the digest of `tree` and compare it with `expected`. A tree the
/// digest refuses (link, special file, empty directory, owner, writable
/// directory, bounds) is `tree_invalid`, or `mismatch` if the tree is read but
/// differs; a fault of the environment is not a refusal.
fn verify_tree_digest(
    name: &str,
    tree: &Path,
    expected: &str,
    limits: &ryeos_node::bundle_staging::TreeLimits,
    mismatch: ryeos_node::bundle_staging::apply::ApplyRefusalCode,
) -> Result<()> {
    use ryeos_node::bundle_staging::StagingRefusal;
    use ryeos_node::bundle_staging::apply::{ApplyRefusal, ApplyRefusalCode};

    match ryeos_node::bundle_staging::tree_digest_from_dir(name, tree, limits) {
        Ok(summary) if summary.digest == expected => Ok(()),
        Ok(summary) => Err(anyhow::Error::new(ApplyRefusal::new(
            mismatch,
            format!(
                "the tree hashes to {}, not the {expected} the stage names",
                summary.digest
            ),
        ))),
        Err(StagingRefusal::Invalid(reason)) => {
            let code = if mismatch == ApplyRefusalCode::TreeChangedDuringInstall {
                ApplyRefusalCode::TreeChangedDuringInstall
            } else {
                ApplyRefusalCode::TreeInvalid
            };
            Err(anyhow::Error::new(ApplyRefusal::new(code, reason)))
        }
        Err(fault) => Err(anyhow::Error::new(fault)),
    }
}

/// Most bytes one bundle archive may expand to.
pub const MAX_ARCHIVE_EXPANDED_BYTES: u64 = 64 * 1024 * 1024;
/// Most entries one bundle archive may hold.
pub const MAX_ARCHIVE_ENTRIES: usize = 8192;

/// Install the bundle a gzip-compressed tar holds under `name`, replacing the
/// installed bundle of that name, with the same verification as
/// `bundle install --replace`. The archive holds one top-level directory
/// named after the bundle (`tar czf site.tgz site`), and only directories and
/// regular files below it.
pub fn replace_from_archive(
    name: &str,
    archive: &[u8],
    preserve_runtime_artifacts: bool,
    authority: &InstallAuthority<'_>,
) -> Result<Value> {
    validate_name(name)?;
    let bundles_root = authority
        .app_root
        .join(ryeos_engine::AI_DIR)
        .join("bundles");
    fs::create_dir_all(&bundles_root)
        .with_context(|| format!("failed to create bundles root {}", bundles_root.display()))?;
    let extraction = bundles_root.join(format!(".{name}.archive-{}", uuid::Uuid::new_v4()));
    fs::create_dir(&extraction)
        .with_context(|| format!("create archive extraction {}", extraction.display()))?;
    let result = extract_bundle_archive(archive, name, &extraction).and_then(|()| {
        install(
            &Request {
                name: name.to_owned(),
                source_path: extraction.join(name),
                replace: true,
                preserve_runtime_artifacts,
                // An archive install names no binding overrides: a
                // replacement that would strand a pinned binding refuses.
                invalidate_bindings: Vec::new(),
            },
            authority,
        )
    });
    if let Err(error) = fs::remove_dir_all(&extraction) {
        tracing::warn!(
            path = %extraction.display(),
            %error,
            "bundle archive extraction was not removed"
        );
    }
    result
}

/// Extract a bundle archive below `into`, refusing anything but directories
/// and regular files at plain relative paths under one top-level directory
/// named `name`, and anything past the entry and size bounds.
pub fn extract_bundle_archive(archive: &[u8], name: &str, into: &Path) -> Result<()> {
    use std::io::Read as _;
    let decoder = flate2::read::GzDecoder::new(archive);
    let mut tar = tar::Archive::new(decoder.take(MAX_ARCHIVE_EXPANDED_BYTES + 1));
    let mut entries = 0usize;
    let mut expanded = 0u64;
    let mut saw_root = false;
    for entry in tar
        .entries()
        .context("bundle archive is not a gzip-compressed tar")?
    {
        let mut entry = entry.context("read a bundle archive entry")?;
        entries += 1;
        if entries > MAX_ARCHIVE_ENTRIES {
            bail!("bundle archive holds more than {MAX_ARCHIVE_ENTRIES} entries");
        }
        let path = entry
            .path()
            .context("bundle archive entry path is not valid")?
            .into_owned();
        let mut components = Vec::new();
        for component in path.components() {
            match component {
                std::path::Component::Normal(part) => components.push(part.to_owned()),
                std::path::Component::CurDir if components.is_empty() => {}
                _ => bail!(
                    "bundle archive entry `{}` is not a plain relative path",
                    path.display()
                ),
            }
        }
        if components.first().and_then(|first| first.to_str()) != Some(name) {
            bail!(
                "bundle archive entry `{}` is not under the top-level directory `{name}`",
                path.display()
            );
        }
        let relative: PathBuf = components.iter().collect();
        let destination = into.join(&relative);
        let kind = entry.header().entry_type();
        if kind.is_dir() {
            saw_root |= components.len() == 1;
            fs::create_dir_all(&destination)
                .with_context(|| format!("create {}", destination.display()))?;
        } else if kind.is_file() {
            if components.len() == 1 {
                bail!("bundle archive top-level entry `{name}` is not a directory");
            }
            let size = entry.header().size().context("bundle archive entry size")?;
            expanded = expanded.saturating_add(size);
            if expanded > MAX_ARCHIVE_EXPANDED_BYTES {
                bail!("bundle archive expands past {MAX_ARCHIVE_EXPANDED_BYTES} bytes");
            }
            if let Some(parent) = destination.parent() {
                fs::create_dir_all(parent)
                    .with_context(|| format!("create {}", parent.display()))?;
            }
            let mut file = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&destination)
                .with_context(|| {
                    format!(
                        "bundle archive names `{}` twice or it cannot be written",
                        relative.display()
                    )
                })?;
            let copied = std::io::copy(&mut (&mut entry).take(size), &mut file)
                .with_context(|| format!("write {}", destination.display()))?;
            if copied != size {
                bail!("bundle archive entry `{}` is truncated", relative.display());
            }
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt as _;
                let mode = entry.header().mode().unwrap_or(0o644);
                let mode = if mode & 0o111 != 0 { 0o755 } else { 0o644 };
                fs::set_permissions(&destination, fs::Permissions::from_mode(mode))?;
            }
        } else {
            bail!(
                "bundle archive entry `{}` is a link or special file; a bundle archive holds only directories and regular files",
                path.display()
            );
        }
    }
    if !saw_root && !into.join(name).is_dir() {
        bail!("bundle archive holds no top-level directory `{name}`");
    }
    Ok(())
}

/// Build the exact post-operation bundle graph used for dependency closure and
/// prospective boot admission.
pub(crate) fn build_prospective_bundle_plan(
    app_root: &Path,
    bundle_name: &str,
    candidate_root: &Path,
    replace: bool,
) -> Result<BundlePlan> {
    let candidate_root = candidate_root.canonicalize().with_context(|| {
        format!(
            "canonicalize prospective bundle root {}",
            candidate_root.display()
        )
    })?;
    let installed = ryeos_bundle::installed::load_installed_plan_inputs(app_root)
        .context("load verified installed bundle graph")?;
    let candidate = PlanInput {
        name: bundle_name.to_string(),
        source: BundleSource::SourceDir(candidate_root),
    };
    let mode = if replace {
        BundlePlanMode::Replace
    } else {
        BundlePlanMode::Install
    };
    build_plan(mode, &[candidate], &installed).context("resolve prospective bundle graph")
}

/// Verify only the candidate using the planner's exact transitive dependency
/// closure, never the ambient set of every installed bundle.
pub(crate) fn verify_planned_candidate(
    plan: &BundlePlan,
    bundle_name: &str,
    app_root: &Path,
    completed_staging: bool,
    isolation: Arc<ryeos_engine::isolation::IsolationRuntime>,
) -> Result<()> {
    let job = plan
        .verification_jobs
        .iter()
        .find(|job| {
            job.subject == bundle_name
                && job.subject_kind == VerificationSubjectKind::CandidateSource
        })
        .with_context(|| {
            format!(
                "prospective plan has no candidate verification job for '{}'",
                bundle_name
            )
        })?;

    if completed_staging {
        preflight_verify_bundle_staging_in_context(
            &job.subject_root,
            bundle_name,
            &job.dependency_roots,
            app_root,
            isolation,
        )
    } else {
        preflight_verify_named_bundle_in_context(
            &job.subject_root,
            bundle_name,
            &job.dependency_roots,
            app_root,
            isolation,
        )
    }
}

/// What admission saw of a completed staging generation: the prospective
/// node config, and the bundle sets before and after, by name and root.
pub(crate) struct AdmittedStaging {
    pub(crate) snapshot: ryeos_app::node_config::NodeConfigSnapshot,
    pub(crate) current: Vec<(String, PathBuf)>,
    pub(crate) prospective: Vec<(String, PathBuf)>,
}

/// Re-plan the completed staging generation, verify it with the exact closure,
/// then run the same node-owned registry/executable admission used at boot.
// This admission boundary deliberately keeps every verified authority explicit.
pub(crate) fn admit_completed_staging(
    app_root: &Path,
    bundle_name: &str,
    staging: &Path,
    replace: bool,
    node_trust_store: &TrustStore,
    prospective_validator: &ryeos_app::prospective_admission::ProspectiveNodeConfigValidator,
    isolation: Arc<ryeos_engine::isolation::IsolationRuntime>,
    lane: Option<&LaneAdmission<'_>>,
) -> Result<AdmittedStaging> {
    if let Some(admission) = lane {
        // A laned bundle is checked against this node's kinds and its
        // publication_lane policy before anything else looks at the tree.
        // This is the install's own completed copy: the tree that is audited
        // is the tree that is activated.
        ryeos_bundle::lane::audit_installed_lane_bundle(
            staging,
            bundle_name,
            admission.kinds,
            &admission.grants,
        )
        .map_err(|refusal| {
            anyhow::Error::new(ryeos_app::handler_error::typed_refusal(
                refusal.code(),
                422,
                refusal.to_string(),
            ))
        })?;
    }
    let plan = build_prospective_bundle_plan(app_root, bundle_name, staging, replace)?;
    verify_planned_candidate(&plan, bundle_name, app_root, true, Arc::clone(&isolation))?;
    // Exercise the second boot phase too: bundle-contributed node config is
    // scanned from the prospective roots and policy-directory collisions are
    // rejected before activation. Command grants come only from the exact
    // node-policy generation, never from bundle registrations.
    let loader = ryeos_node_runtime::bundle_bootstrap::BundleBootstrapLoader {
        app_root,
        trust_store: node_trust_store,
    };
    let mut current_records: std::collections::BTreeMap<
        String,
        ryeos_node_runtime::node_config_source::BundleRecord,
    > = loader
        .load_bundle_section()
        .context("load current node bundle registrations for prospective admission")?
        .into_iter()
        .map(|record| (record.name.clone(), record))
        .collect();
    let current: Vec<(String, PathBuf)> = current_records
        .values()
        .map(|record| (record.name.clone(), record.path.clone()))
        .collect();
    let prospective_records = plan
        .bundles
        .iter()
        .map(|(name, bundle)| {
            if name == bundle_name {
                if replace {
                    current_records.remove(name).with_context(|| {
                        format!(
                            "replacement bundle '{}' has no verified current registration",
                            name
                        )
                    })?;
                }
                Ok(ryeos_node_runtime::node_config_source::BundleRecord {
                    name: name.clone(),
                    lane: lane.map(|admission| admission.marker.clone()),
                    path: bundle.source.root_path().clone(),
                    source_file: app_root
                        .join(ryeos_engine::AI_DIR)
                        .join("node/bundles")
                        .join(format!("{name}.yaml")),
                })
            } else {
                current_records.remove(name).with_context(|| {
                    format!(
                        "prospective bundle '{}' has no verified current registration",
                        name
                    )
                })
            }
        })
        .collect::<Result<Vec<_>>>()?;
    let config_table = ryeos_app::node_config::NodeConfigTable::new();
    let policy_table = ryeos_node_runtime::node_policy::NodePolicyTable::new();
    let policy_snapshot =
        ryeos_node_runtime::node_policy::load_snapshot(app_root, node_trust_store, &policy_table)
            .context("load exact node policy generation for prospective admission")?;
    let command_registration = policy_snapshot.require::<
        ryeos_node_runtime::node_policy::sections::command_registration::CommandRegistrationAuthority,
    >()?;
    let snapshot = ryeos_app::node_config::loader::NodeConfigLoader {
        app_root,
        trust_store: node_trust_store,
    }
    .load_full(
        &config_table,
        &prospective_records,
        command_registration,
        &policy_table,
    )
    .context("prospective bundle set would fail full node-config boot")?;
    // Public routes are judged first: the composed validator refuses a
    // keyed or non-static public route without saying which, and a public
    // route the policy does not name is a typed refusal here, not a boot skip.
    let candidate_root = plan
        .bundles
        .get(bundle_name)
        .map(|bundle| bundle.source.root_path().clone())
        .with_context(|| format!("the prospective plan has no bundle `{bundle_name}`"))?;
    refuse_unadmitted_public_routes(
        &snapshot.routes,
        &candidate_root,
        policy_snapshot.require::<
            ryeos_node_runtime::node_policy::sections::public_routes::PublicRoutesPolicy,
        >()?,
    )?;
    prospective_validator
        .validate(&snapshot)
        .context("prospective bundle set would fail composed node-config admission")?;
    let prospective_roots: Vec<PathBuf> = plan
        .bundles
        .values()
        .map(|bundle| bundle.source.root_path().clone())
        .collect();
    ryeos_node_runtime::engine_init::admit_node_bundle_roots(
        app_root,
        &prospective_roots,
        node_trust_store,
        policy_snapshot.require::<ryeos_engine::isolation::IsolationPolicy>()?,
        policy_snapshot.section_digest::<ryeos_engine::isolation::IsolationPolicy>()?,
    )
    .context("prospective bundle set failed node definition admission")?;
    let prospective = prospective_records
        .iter()
        .map(|record| (record.name.clone(), record.path.clone()))
        .collect();
    Ok(AdmittedStaging {
        snapshot,
        current,
        prospective,
    })
}

fn install_dir_atomic<F, A>(
    src: &Path,
    target: &Path,
    transaction: &ryeos_app::bundle_transaction::BundleTransaction,
    registration: serde_json::Value,
    verify_staged: F,
    verify_activated: A,
) -> Result<()>
where
    F: FnOnce(&Path) -> Result<()>,
    A: FnOnce(&Path) -> Result<()>,
{
    let source = src
        .canonicalize()
        .with_context(|| format!("canonicalize source path {}", src.display()))?;
    let parent = target
        .parent()
        .context("installed bundle target has no parent")?;
    let name = target
        .file_name()
        .and_then(|name| name.to_str())
        .context("installed bundle target has no valid directory name")?;
    let staging = parent.join(format!(".{name}.staging"));
    let canonical_parent = parent
        .canonicalize()
        .with_context(|| format!("canonicalize bundle parent {}", parent.display()))?;
    let canonical_target = canonical_parent.join(name);
    let canonical_staging = canonical_parent.join(format!(".{name}.staging"));
    if source == canonical_target
        || source.starts_with(&canonical_target)
        || canonical_target.starts_with(&source)
    {
        bail!(
            "source_path {} must be outside installed bundle target {}",
            source.display(),
            canonical_target.display()
        );
    }
    if source == canonical_staging
        || source.starts_with(&canonical_staging)
        || canonical_staging.starts_with(&source)
    {
        bail!(
            "source_path {} must be separate from bundle install staging path {}",
            source.display(),
            canonical_staging.display()
        );
    }
    let stale_staging = match fs::symlink_metadata(&staging) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                bail!(
                    "bundle install staging path is not a real directory: {}",
                    staging.display()
                );
            }
            true
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(error) => {
            return Err(error)
                .with_context(|| format!("inspect bundle install staging {}", staging.display()));
        }
    };
    (|| {
        match fs::symlink_metadata(target) {
            Ok(_) => {
                bail!(
                    "bundle target appeared during install: {}",
                    target.display()
                );
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("inspect bundle target {}", target.display()));
            }
        }
        if stale_staging {
            fs::remove_dir_all(&staging)
                .with_context(|| format!("remove stale staging dir {}", staging.display()))?;
        }
        discard_unadmitted_staging(
            &staging,
            (|| {
                copy_dir_recursive(&source, &staging).with_context(|| {
                    format!(
                        "copy bundle from {} to staging {}",
                        source.display(),
                        staging.display()
                    )
                })?;
                lillux::sync_tree_durable(&staging)
                    .with_context(|| format!("flush staged bundle {}", staging.display()))?;
                verify_staged(&staging)
            })(),
        )?;
        transaction.begin_present(
            ryeos_app::bundle_transaction::BundleOperation::Install,
            &staging,
            registration,
        )?;
        if let Err(error) = lillux::rename_path_durable(&staging, target) {
            if error.namespace_committed() {
                tracing::warn!(
                    %error,
                    target = %target.display(),
                    "bundle install is visible but its parent durability barrier failed"
                );
            } else {
                return Err(error).with_context(|| {
                    format!(
                        "activate staged bundle {} at {}",
                        staging.display(),
                        target.display()
                    )
                });
            }
        }
        // The tree is in place but not yet journaled as activated and not
        // registered: a refusal here puts it back.
        if let Err(refusal) = verify_activated(target) {
            return Err(undo_install_activation(
                &staging,
                target,
                transaction,
                refusal,
            ));
        }
        transaction.mark_activated()
    })()
}

// Test-only fault: make the step that moves a refused tree back fail, as a
// disk fault would.
#[cfg(test)]
thread_local! {
    static UNDO_FAULT: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    static MARK_FAULT: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Record that an undo is starting. Test-only fault: fail the record, as a disk
/// fault would, so the journal stays prepared.
fn record_abandoning(transaction: &ryeos_app::bundle_transaction::BundleTransaction) -> Result<()> {
    #[cfg(test)]
    {
        if MARK_FAULT.with(std::cell::Cell::get) {
            anyhow::bail!("injected mark fault");
        }
    }
    transaction.mark_abandoning()
}

/// What the refusal says when the undo did not finish. When the abandoning
/// record was written, reconciliation finishes the undo; when it was not, the
/// journal is still prepared and says nothing about an undo.
fn undo_incomplete_note(what: &str, recorded: bool, error: &anyhow::Error) -> String {
    if recorded {
        format!(
            "the refused {what} could not be fully undone; its journal records the undo and reconciliation finishes it without installing: {error:#}"
        )
    } else {
        format!(
            "the refused {what} could not be undone and the undo could not be recorded; its journal is still prepared, and reconciliation at the next start decides from the tree on disk: a changed tree is removed, an unchanged one is completed: {error:#}"
        )
    }
}

fn undo_fault() -> Result<()> {
    #[cfg(test)]
    {
        if UNDO_FAULT.with(std::cell::Cell::get) {
            anyhow::bail!("injected undo fault");
        }
    }
    Ok(())
}

/// Undo an install whose moved tree failed its post-move check. The journal is
/// first recorded as abandoning, so that whatever happens next the node can
/// only finish the undo, never the install; then the tree is moved back to its
/// staging path and removed, and the journal is removed. Nothing was
/// registered, so no registration is touched. A failure after the first step
/// leaves the abandoning journal, which the next reconciliation completes by
/// removing the refused tree: the node boots without the bundle.
fn undo_install_activation(
    staging: &Path,
    target: &Path,
    transaction: &ryeos_app::bundle_transaction::BundleTransaction,
    refusal: anyhow::Error,
) -> anyhow::Error {
    let recorded = std::cell::Cell::new(false);
    let undone = (|| -> Result<()> {
        record_abandoning(transaction)?;
        recorded.set(true);
        undo_fault()?;
        match lillux::rename_path_durable(target, staging) {
            Ok(()) => {}
            Err(error) if error.namespace_committed() => {
                tracing::warn!(%error, "refused install was moved back but its durability barrier failed");
            }
            Err(error) => {
                // It could not be moved back; remove it where it stands.
                lillux::remove_dir_all_durable(target).with_context(|| {
                    format!("move the refused tree back ({error}) and remove it in place")
                })?;
            }
        }
        lillux::remove_dir_all_durable(staging)
            .with_context(|| format!("remove the refused tree {}", staging.display()))?;
        transaction.finish_abandoned()
    })();
    match undone {
        Ok(()) => refusal,
        Err(error) => refusal.context(undo_incomplete_note("install", recorded.get(), &error)),
    }
}

fn replace_dir_atomic<F, A>(
    src: &Path,
    target: &Path,
    preserve_runtime_artifacts: bool,
    transaction: &ryeos_app::bundle_transaction::BundleTransaction,
    registration: serde_json::Value,
    verify_staged: F,
    verify_activated: A,
) -> Result<()>
where
    F: FnOnce(&Path) -> Result<()>,
    A: FnOnce(&Path) -> Result<()>,
{
    let source = src
        .canonicalize()
        .with_context(|| format!("canonicalize source path {}", src.display()))?;
    let canonical_target = target
        .canonicalize()
        .with_context(|| format!("canonicalize target path {}", target.display()))?;
    let parent = target
        .parent()
        .context("installed bundle target has no parent")?;
    let name = target
        .file_name()
        .and_then(|name| name.to_str())
        .context("installed bundle target has no valid directory name")?;
    let staging = parent.join(format!(".{name}.staging"));
    let canonical_parent = parent
        .canonicalize()
        .with_context(|| format!("canonicalize bundle parent {}", parent.display()))?;
    let canonical_staging = canonical_parent.join(format!(".{name}.staging"));
    if source == canonical_target
        || source.starts_with(&canonical_target)
        || canonical_target.starts_with(&source)
    {
        bail!(
            "source_path {} must be outside installed bundle target {} when using --replace",
            source.display(),
            canonical_target.display()
        );
    }
    if source == canonical_staging
        || source.starts_with(&canonical_staging)
        || canonical_staging.starts_with(&source)
    {
        bail!(
            "source_path {} must be separate from bundle replacement staging path {}",
            source.display(),
            canonical_staging.display()
        );
    }
    let stale_staging = match fs::symlink_metadata(&staging) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                bail!(
                    "bundle replacement staging path is not a real directory: {}",
                    staging.display()
                );
            }
            true
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(error) => {
            return Err(error).with_context(|| {
                format!("inspect bundle replacement staging {}", staging.display())
            });
        }
    };

    (|| {
        if stale_staging {
            fs::remove_dir_all(&staging)
                .with_context(|| format!("remove stale staging dir {}", staging.display()))?;
        }
        discard_unadmitted_staging(
            &staging,
            (|| {
                copy_dir_recursive(&source, &staging).with_context(|| {
                    format!(
                        "copy replacement bundle from {} to staging {}",
                        source.display(),
                        staging.display(),
                    )
                })?;
                if preserve_runtime_artifacts {
                    preserve_runtime_artifact_dirs(&canonical_target, &staging)?;
                }
                lillux::sync_tree_durable(&staging)
                    .with_context(|| format!("flush staged bundle {}", staging.display()))?;
                verify_staged(&staging)
            })(),
        )?;
        transaction.begin_present(
            ryeos_app::bundle_transaction::BundleOperation::Replace,
            &staging,
            registration,
        )?;
        if let Err(error) = lillux::atomic_exchange_paths(target, &staging) {
            if error.namespace_committed() {
                tracing::warn!(
                    %error,
                    target = %target.display(),
                    "bundle replacement is visible but its parent durability barrier failed"
                );
            } else {
                return Err(error).with_context(|| {
                    format!(
                        "atomically exchange installed bundle {} with {}",
                        target.display(),
                        staging.display()
                    )
                });
            }
        }
        // The previous generation still stands at the staging path, so a
        // refusal here swaps it back.
        if let Err(refusal) = verify_activated(target) {
            return Err(undo_replace_activation(
                &staging,
                target,
                transaction,
                refusal,
            ));
        }
        transaction.mark_activated()?;
        if let Err(error) = lillux::remove_dir_all_durable(&staging) {
            tracing::warn!(
                path = %staging.display(),
                error = %format!("{error:#}"),
                "bundle replacement committed but previous generation cleanup failed"
            );
        }
        Ok(())
    })()
}

/// Undo a replacement whose exchanged-in tree failed its post-move check: swap
/// the previous generation back, remove the refused one, abandon the journal.
fn undo_replace_activation(
    staging: &Path,
    target: &Path,
    transaction: &ryeos_app::bundle_transaction::BundleTransaction,
    refusal: anyhow::Error,
) -> anyhow::Error {
    let recorded = std::cell::Cell::new(false);
    let undone = (|| -> Result<()> {
        record_abandoning(transaction)?;
        recorded.set(true);
        undo_fault()?;
        match lillux::atomic_exchange_paths(target, staging) {
            Ok(()) => {}
            Err(error) if error.namespace_committed() => {
                tracing::warn!(%error, "refused replacement was swapped back but its durability barrier failed");
            }
            Err(error) => {
                return Err(anyhow::Error::new(error)
                    .context("swap the previous generation back into place"));
            }
        }
        lillux::remove_dir_all_durable(staging)
            .with_context(|| format!("remove the refused tree {}", staging.display()))?;
        transaction.finish_abandoned()
    })();
    match undone {
        Ok(()) => refusal,
        Err(error) => refusal.context(undo_incomplete_note("replacement", recorded.get(), &error)),
    }
}

/// A staging tree belongs to no bundle transaction until `begin_present`, so
/// when copying, flushing or admitting it fails the tree is removed here: left
/// in the bundle root it is an entry init refuses and nothing would recover.
fn discard_unadmitted_staging(staging: &Path, staged: Result<()>) -> Result<()> {
    let Err(error) = staged else {
        return Ok(());
    };
    match fs::remove_dir_all(staging) {
        Ok(()) => Err(error),
        Err(cleanup) if cleanup.kind() == std::io::ErrorKind::NotFound => Err(error),
        Err(cleanup) => Err(error.context(format!(
            "the unadmitted staging tree {} could not be removed: {cleanup}",
            staging.display()
        ))),
    }
}

fn preserve_runtime_artifact_dirs(existing: &Path, staging: &Path) -> Result<()> {
    for rel in [".ai/bin", ".ai/objects", ".ai/refs"] {
        let from = existing.join(rel);
        if !from.exists() {
            continue;
        }
        let to = staging.join(rel);
        if to.exists() {
            continue;
        }
        if let Some(parent) = to.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("create runtime artifact parent {}", parent.display()))?;
        }
        copy_dir_recursive(&from, &to).with_context(|| {
            format!(
                "preserve runtime artifact directory {} -> {}",
                from.display(),
                to.display()
            )
        })?;
    }
    Ok(())
}

fn copy_dir_recursive(src: &Path, dst: &Path) -> Result<()> {
    fs::create_dir_all(dst).with_context(|| format!("failed to create {}", dst.display()))?;
    for entry in fs::read_dir(src).with_context(|| format!("failed to read {}", src.display()))? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let from = entry.path();
        let to = dst.join(entry.file_name());
        if file_type.is_dir() {
            copy_dir_recursive(&from, &to)?;
        } else if file_type.is_symlink() {
            let link_target = fs::read_link(&from)?;
            #[cfg(unix)]
            std::os::unix::fs::symlink(&link_target, &to)
                .with_context(|| format!("failed to symlink {}", to.display()))?;
            #[cfg(not(unix))]
            {
                let _ = link_target;
                bail!("symlinks unsupported on this platform: {}", from.display());
            }
        } else {
            fs::copy(&from, &to).with_context(|| {
                format!("failed to copy {} -> {}", from.display(), to.display())
            })?;
        }
    }
    Ok(())
}

pub const DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:bundle/install",
    endpoint: "bundle.install",
    availability: ServiceAvailability::StoppedNodeOnly,
    required_caps: &["ryeos.execute.service.bundle/install"],
    handler: |params, _ctx, state| {
        Box::pin(async move {
            let req: Request = serde_json::from_value(params)
                .context("bundle.install requires { name, source_path, replace? }")?;
            handle(req, state).await
        })
    },
};

#[cfg(test)]
mod tests {
    use super::*;

    fn replace_for_test(src: &Path, target: &Path, preserve: bool) {
        let app_root = target.ancestors().nth(3).unwrap();
        let registry_lock =
            ryeos_node_runtime::registry::BundleRegistryMutationLock::acquire(app_root).unwrap();
        let transaction = ryeos_app::bundle_transaction::BundleMutationTransaction::acquire(
            &registry_lock,
            target.file_name().unwrap().to_str().unwrap(),
        )
        .unwrap();
        replace_dir_atomic(
            src,
            target,
            preserve,
            &transaction,
            serde_json::json!({ "kind": "node", "path": target }),
            |_| Ok(()),
            |_| Ok(()),
        )
        .unwrap();
    }

    fn tar_gz(entries: &[(&str, Option<&[u8]>, tar::EntryType)]) -> Vec<u8> {
        let mut builder = tar::Builder::new(flate2::write::GzEncoder::new(
            Vec::new(),
            flate2::Compression::default(),
        ));
        for (path, content, kind) in entries {
            let mut header = tar::Header::new_gnu();
            header.set_entry_type(*kind);
            header.set_mode(0o644);
            let bytes = content.unwrap_or_default();
            header.set_size(bytes.len() as u64);
            if *kind == tar::EntryType::Symlink {
                header.set_link_name("../../etc/passwd").unwrap();
            }
            // Written raw so a hostile path reaches the extractor unchanged.
            let name = path.as_bytes();
            header.as_old_mut().name[..name.len()].copy_from_slice(name);
            header.set_cksum();
            builder.append(&header, bytes).unwrap();
        }
        builder.into_inner().unwrap().finish().unwrap()
    }

    #[test]
    fn a_bundle_archive_extracts_its_one_tree() {
        let tmp = tempfile::tempdir().unwrap();
        let archive = tar_gz(&[
            ("site/", None, tar::EntryType::Directory),
            (
                "site/.ai/manifest.yaml",
                Some(b"name: site\n"),
                tar::EntryType::Regular,
            ),
        ]);
        extract_bundle_archive(&archive, "site", tmp.path()).unwrap();
        assert_eq!(
            fs::read(tmp.path().join("site/.ai/manifest.yaml")).unwrap(),
            b"name: site\n"
        );
    }

    #[test]
    fn bundle_replace_operation_refuses_unverified_archive() {
        for (entries, needle) in [
            (
                vec![("site/../escape", Some(&b"x"[..]), tar::EntryType::Regular)],
                "not a plain relative path",
            ),
            (
                vec![("/etc/site", Some(&b"x"[..]), tar::EntryType::Regular)],
                "not a plain relative path",
            ),
            (
                vec![("other/file", Some(&b"x"[..]), tar::EntryType::Regular)],
                "not under the top-level directory",
            ),
            (
                vec![("site/link", None, tar::EntryType::Symlink)],
                "link or special file",
            ),
            (
                vec![("site", Some(&b"x"[..]), tar::EntryType::Regular)],
                "not a directory",
            ),
        ] {
            let tmp = tempfile::tempdir().unwrap();
            let error = extract_bundle_archive(&tar_gz(&entries), "site", tmp.path()).unwrap_err();
            assert!(format!("{error:#}").contains(needle), "{error:#}");
            assert!(!tmp.path().join("escape").exists());
        }
        let tmp = tempfile::tempdir().unwrap();
        let error = extract_bundle_archive(b"not gzip", "site", tmp.path()).unwrap_err();
        assert!(format!("{error:#}").contains("bundle archive"), "{error:#}");
    }

    #[test]
    fn staged_binding_refusal_is_a_typed_conflict() {
        let admitted = AdmittedStaging {
            snapshot: ryeos_app::node_config::NodeConfigSnapshot {
                stale_bindings: Vec::new(),
                external_execution: Vec::new(),
                runtime_snapshot_production: Vec::new(),
                guest_runtime_materialization: Vec::new(),
                runtime_snapshot_qualification: Vec::new(),
                bundles: Vec::new(),
                routes: Vec::new(),
                commands: Vec::new(),
            },
            current: Vec::new(),
            prospective: Vec::new(),
        };
        let request: Request = serde_json::from_value(serde_json::json!({
            "name": "adapters",
            "source_path": "/nonexistent",
            "replace": true,
            "invalidate_bindings": ["runtime_snapshot_production/render-snapshot"],
        }))
        .unwrap();
        let trust = TrustStore::empty();
        // Nothing is pinned, so nothing is stranded: naming a binding is refused.
        let error =
            check_staged_bindings(&admitted, &trust, &request.invalidate_bindings).unwrap_err();
        let Some(ryeos_app::handler_error::HandlerError::Structured { code, status, body }) =
            ryeos_app::handler_error::extract_handler_error(&error)
        else {
            panic!("typed refusal: {error:#}");
        };
        assert_eq!(
            code,
            ryeos_app::node_config::coherence::INVALIDATED_BINDING_NOT_STRANDED
        );
        assert_eq!(status, 409);
        assert_eq!(
            body["not_stranded"],
            serde_json::json!(["runtime_snapshot_production/render-snapshot"])
        );
        assert!(
            check_staged_bindings(&admitted, &trust, &[])
                .unwrap()
                .is_empty()
        );

        let malformed = serde_json::from_value::<Request>(serde_json::json!({
            "name": "adapters",
            "source_path": "/nonexistent",
            "invalidate_bindings": ["bundles/core"],
        }));
        assert!(malformed.is_err());
    }

    #[test]
    fn validate_name_rejects_empty() {
        assert!(validate_name("").is_err());
    }

    #[test]
    fn validate_name_rejects_slashes() {
        assert!(validate_name("foo/bar").is_err());
    }

    #[test]
    fn validate_name_rejects_dots() {
        assert!(validate_name("foo.bar").is_err());
    }

    #[test]
    fn validate_name_rejects_uppercase() {
        assert!(validate_name("Foo").is_err());
    }

    #[test]
    fn validate_name_rejects_spaces() {
        assert!(validate_name("foo bar").is_err());
    }

    #[test]
    fn validate_name_accepts_valid() {
        assert!(validate_name("my-bundle_v2").is_ok());
        assert!(validate_name("core").is_ok());
        assert!(validate_name("knowledge").is_ok());
    }

    #[test]
    fn copy_dir_copies_nested_files() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        let dst = tmp.path().join("dst");
        fs::create_dir_all(src.join("a/b")).unwrap();
        fs::write(src.join("top.txt"), b"top").unwrap();
        fs::write(src.join("a/mid.txt"), b"mid").unwrap();
        fs::write(src.join("a/b/leaf.txt"), b"leaf").unwrap();

        copy_dir_recursive(&src, &dst).unwrap();

        assert_eq!(fs::read(dst.join("top.txt")).unwrap(), b"top");
        assert_eq!(fs::read(dst.join("a/mid.txt")).unwrap(), b"mid");
        assert_eq!(fs::read(dst.join("a/b/leaf.txt")).unwrap(), b"leaf");
    }

    #[test]
    fn replace_dir_replaces_existing_tree_and_cleans_staging() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        let bundles = tmp.path().join(".ai/bundles");
        let target = bundles.join("ryeos-ui");
        fs::create_dir_all(src.join("new")).unwrap();
        fs::create_dir_all(target.join("old")).unwrap();
        fs::write(src.join("new/file.txt"), b"new").unwrap();
        fs::write(target.join("old/file.txt"), b"old").unwrap();

        replace_for_test(&src, &target, false);

        assert_eq!(fs::read(target.join("new/file.txt")).unwrap(), b"new");
        assert!(!target.join("old/file.txt").exists());
        assert!(!bundles.join(".ryeos-ui.staging").exists());
    }

    #[test]
    fn replace_dir_preserves_runtime_artifacts_when_source_is_sparse() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        let bundles = tmp.path().join(".ai/bundles");
        let target = bundles.join("ryeos-ui");
        fs::create_dir_all(src.join(".ai/node/commands")).unwrap();
        fs::create_dir_all(target.join(".ai/bin/x86_64-unknown-linux-gnu")).unwrap();
        fs::create_dir_all(target.join(".ai/objects/blobs")).unwrap();
        fs::create_dir_all(target.join(".ai/refs/bundles")).unwrap();
        fs::write(src.join(".ai/node/commands/web.yaml"), b"web").unwrap();
        fs::write(target.join(".ai/bin/x86_64-unknown-linux-gnu/web"), b"bin").unwrap();
        fs::write(target.join(".ai/objects/blobs/blob"), b"blob").unwrap();
        fs::write(target.join(".ai/refs/bundles/manifest"), b"ref").unwrap();

        replace_for_test(&src, &target, true);

        assert_eq!(
            fs::read(target.join(".ai/node/commands/web.yaml")).unwrap(),
            b"web"
        );
        assert_eq!(
            fs::read(target.join(".ai/bin/x86_64-unknown-linux-gnu/web")).unwrap(),
            b"bin"
        );
        assert_eq!(
            fs::read(target.join(".ai/objects/blobs/blob")).unwrap(),
            b"blob"
        );
        assert_eq!(
            fs::read(target.join(".ai/refs/bundles/manifest")).unwrap(),
            b"ref"
        );
    }

    #[test]
    fn replacement_validates_completed_staging_before_activation() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        let bundles = tmp.path().join(".ai/bundles");
        let target = bundles.join("demo");
        fs::create_dir_all(src.join(".ai/node/commands")).unwrap();
        fs::create_dir_all(target.join(".ai/refs/bundles")).unwrap();
        fs::write(src.join(".ai/node/commands/new.yaml"), b"new").unwrap();
        fs::write(target.join(".ai/refs/bundles/manifest"), b"preserved").unwrap();
        fs::write(target.join("old.txt"), b"old").unwrap();
        let registry_lock =
            ryeos_node_runtime::registry::BundleRegistryMutationLock::acquire(tmp.path()).unwrap();
        let transaction = ryeos_app::bundle_transaction::BundleMutationTransaction::acquire(
            &registry_lock,
            "demo",
        )
        .unwrap();
        let validation_called = std::cell::Cell::new(false);

        let error = replace_dir_atomic(
            &src,
            &target,
            true,
            &transaction,
            serde_json::json!({ "kind": "node", "path": target }),
            |staging| {
                validation_called.set(true);
                assert_eq!(
                    fs::read(staging.join(".ai/refs/bundles/manifest")).unwrap(),
                    b"preserved"
                );
                anyhow::bail!("completed staging rejected")
            },
            |_| Ok(()),
        )
        .expect_err("failed completed-staging preflight must prevent activation");

        assert!(validation_called.get());
        assert!(error.to_string().contains("completed staging rejected"));
        assert_eq!(fs::read(target.join("old.txt")).unwrap(), b"old");
        assert!(!target.join(".ai/node/commands/new.yaml").exists());
        assert!(!bundles.join(".demo.staging").exists());
    }

    #[test]
    fn refused_install_leaves_no_staging_tree() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        let bundles = tmp.path().join(".ai/bundles");
        let target = bundles.join("demo");
        fs::create_dir_all(src.join(".ai/config")).unwrap();
        fs::create_dir_all(&bundles).unwrap();
        fs::write(src.join(".ai/config/policy.yaml"), b"policy").unwrap();
        let registry_lock =
            ryeos_node_runtime::registry::BundleRegistryMutationLock::acquire(tmp.path()).unwrap();
        let transaction = ryeos_app::bundle_transaction::BundleMutationTransaction::acquire(
            &registry_lock,
            "demo",
        )
        .unwrap();

        let error = install_dir_atomic(
            &src,
            &target,
            &transaction,
            serde_json::json!({ "kind": "node", "path": target }),
            |staging| {
                assert!(staging.join(".ai/config/policy.yaml").is_file());
                anyhow::bail!("completed staging rejected")
            },
            |_| Ok(()),
        )
        .expect_err("a refused staging tree must not activate");

        assert!(format!("{error:#}").contains("completed staging rejected"));
        assert!(!target.exists());
        assert!(!bundles.join(".demo.staging").exists());
    }

    // ── Staged install: verify closure, undo, and the recovered early return ──

    use ryeos_node::bundle_staging::apply::{ApplyRefusal, ApplyRefusalCode};

    fn test_limits() -> ryeos_node::bundle_staging::TreeLimits {
        ryeos_node::bundle_staging::TreeLimits {
            max_files: 1024,
            max_bytes: 1024 * 1024,
        }
    }

    /// A small bundle tree whose directories are exactly 0o755, and its tree
    /// digest under the name `demo`.
    fn digest_tree(root: &Path) -> String {
        fs::create_dir_all(root.join(".ai/node")).unwrap();
        fs::write(root.join("a.txt"), b"alpha").unwrap();
        fs::write(root.join(".ai/node/b.txt"), b"beta").unwrap();
        lillux::set_tree_directory_modes(root).unwrap();
        ryeos_node::bundle_staging::tree_digest_from_dir("demo", root, &test_limits())
            .unwrap()
            .digest
    }

    fn refusal_code(error: &anyhow::Error) -> Option<ApplyRefusalCode> {
        ApplyRefusal::find(error).map(ApplyRefusal::code)
    }

    #[test]
    fn the_tree_check_accepts_the_staged_bytes_and_names_each_way_they_differ() {
        let tmp = tempfile::tempdir().unwrap();
        let tree = tmp.path().join("tree");
        let expected = digest_tree(&tree);
        let check =
            |mismatch| verify_tree_digest("demo", &tree, &expected, &test_limits(), mismatch);
        check(ApplyRefusalCode::TreeDigestMismatch).unwrap();

        // A changed byte.
        fs::write(tree.join("a.txt"), b"alphA").unwrap();
        let error = check(ApplyRefusalCode::TreeDigestMismatch).unwrap_err();
        assert_eq!(
            refusal_code(&error),
            Some(ApplyRefusalCode::TreeDigestMismatch)
        );
        let error = check(ApplyRefusalCode::TreeChangedDuringInstall).unwrap_err();
        assert_eq!(
            refusal_code(&error),
            Some(ApplyRefusalCode::TreeChangedDuringInstall)
        );
        fs::write(tree.join("a.txt"), b"alpha").unwrap();
        check(ApplyRefusalCode::TreeDigestMismatch).unwrap();

        // An added file.
        fs::write(tree.join("extra.txt"), b"x").unwrap();
        let error = check(ApplyRefusalCode::TreeDigestMismatch).unwrap_err();
        assert_eq!(
            refusal_code(&error),
            Some(ApplyRefusalCode::TreeDigestMismatch)
        );
        fs::remove_file(tree.join("extra.txt")).unwrap();

        // A link is refused outright, not hashed.
        std::os::unix::fs::symlink("a.txt", tree.join("link")).unwrap();
        let error = check(ApplyRefusalCode::TreeDigestMismatch).unwrap_err();
        assert_eq!(refusal_code(&error), Some(ApplyRefusalCode::TreeInvalid));
        fs::remove_file(tree.join("link")).unwrap();

        // A group-writable directory (a daemon umask of 002) is refused until
        // the copy's directory modes are set, which the staged check does.
        {
            use std::os::unix::fs::PermissionsExt as _;
            fs::set_permissions(tree.join(".ai"), fs::Permissions::from_mode(0o775)).unwrap();
        }
        let error = check(ApplyRefusalCode::TreeDigestMismatch).unwrap_err();
        assert_eq!(refusal_code(&error), Some(ApplyRefusalCode::TreeInvalid));
        lillux::set_tree_directory_modes(&tree).unwrap();
        check(ApplyRefusalCode::TreeDigestMismatch).unwrap();
    }

    fn atomic_fixture(tmp: &Path) -> (PathBuf, PathBuf, PathBuf, lillux::crypto::SigningKey) {
        let src = tmp.join("src");
        let bundles = tmp.join(".ai/bundles");
        fs::create_dir_all(&bundles).unwrap();
        fs::create_dir_all(tmp.join(".ai/node/bundles")).unwrap();
        (
            src,
            bundles.clone(),
            bundles.join("demo"),
            lillux::crypto::SigningKey::from_bytes(&[3; 32]),
        )
    }

    fn acquire<'a>(
        tmp: &Path,
        lock: &'a ryeos_node_runtime::registry::BundleRegistryMutationLock,
    ) -> ryeos_app::bundle_transaction::BundleMutationTransaction<'a> {
        let _ = tmp;
        ryeos_app::bundle_transaction::BundleMutationTransaction::acquire(lock, "demo").unwrap()
    }

    #[test]
    fn a_post_move_refusal_undoes_an_install_through_the_journal() {
        let tmp = tempfile::tempdir().unwrap();
        let (src, bundles, target, key) = atomic_fixture(tmp.path());
        let expected = digest_tree(&src);
        let registry_lock =
            ryeos_node_runtime::registry::BundleRegistryMutationLock::acquire(tmp.path()).unwrap();
        let transaction = acquire(tmp.path(), &registry_lock);
        let seen_in_place = std::cell::Cell::new(false);

        let error = install_dir_atomic(
            &src,
            &target,
            &transaction,
            serde_json::json!({ "kind": "node", "path": target }),
            |staging| {
                // The copy is audited and passes; a same-user writer then
                // changes a byte before the install journals it.
                verify_tree_digest(
                    "demo",
                    staging,
                    &expected,
                    &test_limits(),
                    ApplyRefusalCode::TreeDigestMismatch,
                )?;
                fs::write(staging.join("a.txt"), b"evil!")?;
                Ok(())
            },
            |moved| {
                seen_in_place.set(moved.join("a.txt").is_file());
                verify_tree_digest(
                    "demo",
                    moved,
                    &expected,
                    &test_limits(),
                    ApplyRefusalCode::TreeChangedDuringInstall,
                )
            },
        )
        .expect_err("a tree that changed after the audit must not stay installed");

        assert!(seen_in_place.get(), "the check ran on the moved tree");
        assert_eq!(
            refusal_code(&error),
            Some(ApplyRefusalCode::TreeChangedDuringInstall)
        );
        assert!(!target.exists());
        assert!(!bundles.join(".demo.staging").exists());
        assert!(!tmp.path().join(".ai/node/bundles/demo.yaml").exists());
        // The journal is gone, so there is nothing for a restart to finish.
        assert_eq!(transaction.reconcile(&key).unwrap(), None);
    }

    #[test]
    fn a_failed_undo_leaves_a_journal_that_reconciles_to_the_undo_not_the_install() {
        let tmp = tempfile::tempdir().unwrap();
        let (src, bundles, target, key) = atomic_fixture(tmp.path());
        let expected = digest_tree(&src);
        let registry_lock =
            ryeos_node_runtime::registry::BundleRegistryMutationLock::acquire(tmp.path()).unwrap();
        let transaction = acquire(tmp.path(), &registry_lock);

        UNDO_FAULT.with(|fault| fault.set(true));
        let error = install_dir_atomic(
            &src,
            &target,
            &transaction,
            serde_json::json!({ "kind": "node", "path": target }),
            |staging| {
                // Changed after the audit, before the journal digest is taken:
                // the journal's digest is the changed tree's, so only the
                // abandoning record stops reconciliation completing it.
                fs::write(staging.join("a.txt"), b"evil!")?;
                Ok(())
            },
            |moved| {
                verify_tree_digest(
                    "demo",
                    moved,
                    &expected,
                    &test_limits(),
                    ApplyRefusalCode::TreeChangedDuringInstall,
                )
            },
        )
        .expect_err("the moved tree is refused");
        UNDO_FAULT.with(|fault| fault.set(false));
        assert_eq!(
            refusal_code(&error),
            Some(ApplyRefusalCode::TreeChangedDuringInstall)
        );
        // The undo failed: the unverified tree still stands in the target.
        assert!(target.is_dir());

        // Reconciliation finishes the undo and never completes the install.
        let recovered = transaction.reconcile(&key).unwrap();
        assert_eq!(
            recovered,
            Some(ryeos_app::bundle_transaction::BundleOperation::Install)
        );
        assert!(!target.exists());
        assert!(!bundles.join(".demo.staging").exists());
        assert!(!tmp.path().join(".ai/node/bundles/demo.yaml").exists());
        assert_eq!(transaction.reconcile(&key).unwrap(), None);
    }

    #[test]
    fn an_unrecorded_undo_says_so_and_reconciles_from_the_tree_on_disk() {
        // The record of the undo fails, so the journal stays prepared and the
        // refusal must not claim the journal records an undo. Reconciliation
        // then decides from the tree on disk: the tree was changed after the
        // journal took its digest, so the digest differs and the tree is removed
        // with no registration. (A tree still equal to the journal's digest
        // would be completed as the install it was prepared as.)
        let tmp = tempfile::tempdir().unwrap();
        let (src, bundles, target, key) = atomic_fixture(tmp.path());
        let expected = digest_tree(&src);
        let registry_lock =
            ryeos_node_runtime::registry::BundleRegistryMutationLock::acquire(tmp.path()).unwrap();
        let transaction = acquire(tmp.path(), &registry_lock);

        MARK_FAULT.with(|fault| fault.set(true));
        let error = install_dir_atomic(
            &src,
            &target,
            &transaction,
            serde_json::json!({ "kind": "node", "path": target }),
            |_| Ok(()),
            |moved| {
                // Changed after the journal's digest was taken.
                fs::write(moved.join("a.txt"), b"evil!")?;
                verify_tree_digest(
                    "demo",
                    moved,
                    &expected,
                    &test_limits(),
                    ApplyRefusalCode::TreeChangedDuringInstall,
                )
            },
        )
        .expect_err("the moved tree is refused");
        MARK_FAULT.with(|fault| fault.set(false));
        assert_eq!(
            refusal_code(&error),
            Some(ApplyRefusalCode::TreeChangedDuringInstall)
        );
        let text = format!("{error:#}");
        assert!(text.contains("the undo could not be recorded"), "{text}");
        assert!(text.contains("still prepared"), "{text}");
        assert!(!text.contains("its journal records the undo"), "{text}");
        // Nothing undid the move: the changed tree stands in the target.
        assert!(target.is_dir());

        let recovered = transaction.reconcile(&key).unwrap();
        assert_eq!(
            recovered,
            Some(ryeos_app::bundle_transaction::BundleOperation::Install)
        );
        assert!(!target.exists());
        assert!(!bundles.join(".demo.staging").exists());
        assert!(!tmp.path().join(".ai/node/bundles/demo.yaml").exists());
        assert_eq!(transaction.reconcile(&key).unwrap(), None);
    }

    #[test]
    fn an_unrecorded_undo_of_a_replacement_reconciles_back_to_the_previous_generation() {
        // The exchange was made, the moved tree was then changed, and the undo
        // could not even be recorded: the journal is still prepared, the target
        // holds the changed tree and the staging path holds the previous
        // generation. Reconciliation must put the previous generation back
        // under its registration, not discard it.
        let tmp = tempfile::tempdir().unwrap();
        let (src, _bundles, target, key) = atomic_fixture(tmp.path());
        fs::create_dir_all(&target).unwrap();
        fs::write(target.join("old.txt"), b"old").unwrap();
        fs::create_dir_all(&src).unwrap();
        fs::write(src.join("new.txt"), b"new").unwrap();
        let registration = tmp.path().join(".ai/node/bundles/demo.yaml");
        fs::create_dir_all(registration.parent().unwrap()).unwrap();
        fs::write(&registration, b"previous registration").unwrap();
        let registry_lock =
            ryeos_node_runtime::registry::BundleRegistryMutationLock::acquire(tmp.path()).unwrap();
        let transaction = acquire(tmp.path(), &registry_lock);

        MARK_FAULT.with(|fault| fault.set(true));
        let _ = replace_dir_atomic(
            &src,
            &target,
            false,
            &transaction,
            serde_json::json!({ "kind": "node", "path": target }),
            |_| Ok(()),
            |moved| {
                fs::write(moved.join("new.txt"), b"changed")?;
                anyhow::bail!("refused")
            },
        )
        .expect_err("refused");
        MARK_FAULT.with(|fault| fault.set(false));
        // Nothing was undone: the changed tree is in the target.
        assert_eq!(fs::read(target.join("new.txt")).unwrap(), b"changed");

        transaction.reconcile(&key).unwrap();
        assert_eq!(fs::read(target.join("old.txt")).unwrap(), b"old");
        assert!(!target.join("new.txt").exists());
        assert_eq!(fs::read(&registration).unwrap(), b"previous registration");
        assert_eq!(transaction.reconcile(&key).unwrap(), None);
    }

    #[test]
    fn a_failed_undo_of_a_replacement_reconciles_back_to_the_previous_generation() {
        let tmp = tempfile::tempdir().unwrap();
        let (src, _bundles, target, key) = atomic_fixture(tmp.path());
        fs::create_dir_all(&target).unwrap();
        fs::write(target.join("old.txt"), b"old").unwrap();
        fs::create_dir_all(&src).unwrap();
        fs::write(src.join("new.txt"), b"new").unwrap();
        let registry_lock =
            ryeos_node_runtime::registry::BundleRegistryMutationLock::acquire(tmp.path()).unwrap();
        let transaction = acquire(tmp.path(), &registry_lock);

        UNDO_FAULT.with(|fault| fault.set(true));
        let _ = replace_dir_atomic(
            &src,
            &target,
            false,
            &transaction,
            serde_json::json!({ "kind": "node", "path": target }),
            |_| Ok(()),
            |_| anyhow::bail!("refused"),
        )
        .expect_err("refused");
        UNDO_FAULT.with(|fault| fault.set(false));
        // Not swapped back yet: the refused generation is in the target.
        assert!(target.join("new.txt").is_file());

        transaction.reconcile(&key).unwrap();
        assert_eq!(fs::read(target.join("old.txt")).unwrap(), b"old");
        assert!(!target.join("new.txt").exists());
        assert!(!tmp.path().join(".ai/node/bundles/demo.yaml").exists());
        assert_eq!(transaction.reconcile(&key).unwrap(), None);
    }

    #[test]
    fn a_post_move_refusal_puts_the_previous_generation_back_on_replace() {
        let tmp = tempfile::tempdir().unwrap();
        let (src, bundles, target, key) = atomic_fixture(tmp.path());
        fs::create_dir_all(&target).unwrap();
        fs::write(target.join("old.txt"), b"old").unwrap();
        fs::create_dir_all(&src).unwrap();
        fs::write(src.join("new.txt"), b"new").unwrap();
        let registry_lock =
            ryeos_node_runtime::registry::BundleRegistryMutationLock::acquire(tmp.path()).unwrap();
        let transaction = acquire(tmp.path(), &registry_lock);

        let error = replace_dir_atomic(
            &src,
            &target,
            false,
            &transaction,
            serde_json::json!({ "kind": "node", "path": target }),
            |_| Ok(()),
            |moved| {
                assert!(moved.join("new.txt").is_file());
                anyhow::bail!("tree changed")
            },
        )
        .expect_err("the refused replacement must not stay");

        assert!(error.to_string().contains("tree changed"));
        assert_eq!(fs::read(target.join("old.txt")).unwrap(), b"old");
        assert!(!target.join("new.txt").exists());
        assert!(!bundles.join(".demo.staging").exists());
        assert_eq!(transaction.reconcile(&key).unwrap(), None);
    }

    fn no_admission(
        _: &Path,
        _: bool,
    ) -> Result<Vec<ryeos_app::node_config::coherence::BindingRef>> {
        panic!("a recovered install is not admitted again")
    }

    fn recovery_request(src: &Path) -> Request {
        Request {
            name: "demo".to_owned(),
            source_path: src.to_path_buf(),
            replace: false,
            preserve_runtime_artifacts: false,
            invalidate_bindings: Vec::new(),
        }
    }

    /// Leave an install that died after the tree was activated and before the
    /// registration was committed: the journal says `activated`.
    fn crash_after_activation(tmp: &Path, src: &Path, target: &Path) {
        let registry_lock =
            ryeos_node_runtime::registry::BundleRegistryMutationLock::acquire(tmp).unwrap();
        let transaction = acquire(tmp, &registry_lock);
        install_dir_atomic(
            src,
            target,
            &transaction,
            serde_json::json!({ "kind": "node", "path": target }),
            |_| Ok(()),
            |_| Ok(()),
        )
        .unwrap();
    }

    #[test]
    fn a_rerun_that_recovers_an_interrupted_install_reseals_the_registration_set() {
        let tmp = tempfile::tempdir().unwrap();
        let (src, _bundles, target, key) = atomic_fixture(tmp.path());
        digest_tree(&src);
        crash_after_activation(tmp.path(), &src, &target);
        assert!(!tmp.path().join(".ai/node/bundles/demo.yaml").exists());

        let seals = std::cell::Cell::new(0);
        let seal = || {
            seals.set(seals.get() + 1);
            Ok(())
        };
        let report = install_core(
            &recovery_request(&src),
            &InstallEnv {
                app_root: tmp.path(),
                node_signing_key: &key,
                generation_changed: &|_| {},
                admit: &no_admission,
                refuse_replace: &|_, _| Ok(()),
                seal: &seal,
            },
            &InstallChecks::none(),
        )
        .unwrap();

        assert_eq!(report["recovered"], true);
        // Reconciliation committed the registration, and the early return
        // sealed it: a node that died here boots.
        assert!(tmp.path().join(".ai/node/bundles/demo.yaml").is_file());
        assert_eq!(seals.get(), 1);
    }

    /// Rerun a staged install over an interrupted install of the tree at
    /// `src`, with the stage naming `stage_digest`. Returns the outcome and
    /// how many times the registration set was sealed.
    fn staged_rerun(
        tmp: &Path,
        src: &Path,
        key: &lillux::crypto::SigningKey,
        stage_digest: &str,
    ) -> (Result<Value>, usize) {
        let converged = |tree: &Path| {
            verify_tree_digest(
                "demo",
                tree,
                stage_digest,
                &test_limits(),
                ApplyRefusalCode::BundleAlreadyInstalled,
            )
        };
        let seals = std::cell::Cell::new(0);
        let seal = || {
            seals.set(seals.get() + 1);
            Ok(())
        };
        let outcome = install_core(
            &recovery_request(src),
            &InstallEnv {
                app_root: tmp,
                node_signing_key: key,
                generation_changed: &|_| {},
                admit: &no_admission,
                refuse_replace: &|_, _| Ok(()),
                seal: &seal,
            },
            &InstallChecks {
                converged: &converged,
                ..InstallChecks::none()
            },
        );
        (outcome, seals.get())
    }

    /// A node identity, a trust store that holds it, and a native
    /// registration of `demo` whose installed tree holds `old.txt`.
    fn node_with_a_native_demo(
        tmp: &Path,
        target: &Path,
    ) -> (ryeos_node_runtime::identity::NodeIdentity, TrustStore) {
        fs::create_dir_all(tmp.join(".ai/node/identity")).unwrap();
        let identity = ryeos_node_runtime::identity::NodeIdentity::create(
            &tmp.join(".ai/node/identity/private_key.pem"),
        )
        .unwrap();
        let trust = TrustStore::from_signers(vec![ryeos_engine::trust::TrustedSigner {
            fingerprint: identity.fingerprint().to_owned(),
            verifying_key: identity.signing_key().verifying_key(),
            label: None,
        }]);
        fs::create_dir_all(target).unwrap();
        fs::write(target.join("old.txt"), b"old").unwrap();
        let yaml =
            serde_yaml::to_string(&serde_json::json!({ "kind": "node", "path": target })).unwrap();
        fs::write(
            tmp.join(".ai/node/bundles/demo.yaml"),
            lillux::signature::sign_content(&yaml, identity.signing_key(), "#", None),
        )
        .unwrap();
        (identity, trust)
    }

    fn lane_install() -> LaneInstall {
        LaneInstall {
            marker: ryeos_engine::trust::BundleLane::publication(
                "a".repeat(64),
                "b".repeat(64),
                "c".repeat(64),
                "1.0.0",
            ),
            grants: ryeos_bundle::lane::LaneGrants::invoked_only(),
        }
    }

    #[test]
    fn a_laned_install_with_replace_never_takes_a_native_registration() {
        // The name looked free when the lane was prepared; a native install
        // took it before this install holds the registry lock. The guard under
        // the lock refuses, before any byte moves and before admission.
        let tmp = tempfile::tempdir().unwrap();
        let (src, _bundles, target, _key) = atomic_fixture(tmp.path());
        let (identity, trust) = node_with_a_native_demo(tmp.path(), &target);
        fs::create_dir_all(&src).unwrap();
        fs::write(src.join("new.txt"), b"new").unwrap();
        let guard = |name: &str, laned: bool| replacement_guard(tmp.path(), &trust, name, laned);
        let seal = || Ok(());

        let error = install_core(
            &Request {
                replace: true,
                ..recovery_request(&src)
            },
            &InstallEnv {
                app_root: tmp.path(),
                node_signing_key: identity.signing_key(),
                generation_changed: &|_| {},
                admit: &no_admission,
                refuse_replace: &guard,
                seal: &seal,
            },
            &InstallChecks {
                lane: Some(lane_install()),
                ..InstallChecks::none()
            },
        )
        .unwrap_err();
        assert_eq!(
            ryeos_app::handler_error::extract_handler_error(&error).map(|error| match error {
                crate::handler_error::HandlerError::Structured { code, .. } => code,
                _ => String::new(),
            }),
            Some("bundle_name_held_by_native_lane".to_owned()),
            "{error:#}"
        );
        assert_eq!(fs::read(target.join("old.txt")).unwrap(), b"old");
        assert!(!target.join("new.txt").exists());
        let registration =
            fs::read_to_string(tmp.path().join(".ai/node/bundles/demo.yaml")).unwrap();
        assert!(!registration.contains("lane"), "{registration}");
        // A native replacement of the same name is not the lane's concern.
        guard("demo", false).unwrap();
    }

    #[test]
    fn a_staged_rerun_seals_a_tree_equal_to_the_stage() {
        let tmp = tempfile::tempdir().unwrap();
        let (src, _bundles, target, key) = atomic_fixture(tmp.path());
        let expected = digest_tree(&src);
        crash_after_activation(tmp.path(), &src, &target);

        let (outcome, seals) = staged_rerun(tmp.path(), &src, &key, &expected);
        outcome.unwrap();
        assert_eq!(seals, 1);
    }

    #[test]
    fn a_staged_rerun_seals_then_refuses_when_the_installed_tree_is_not_the_stage() {
        let tmp = tempfile::tempdir().unwrap();
        let (src, _bundles, target, key) = atomic_fixture(tmp.path());
        digest_tree(&src);
        crash_after_activation(tmp.path(), &src, &target);
        // The stage names other bytes than the interrupted install holds.
        let other = tmp.path().join("other");
        digest_tree(&other);
        fs::write(other.join("a.txt"), b"different").unwrap();
        let stage_digest =
            ryeos_node::bundle_staging::tree_digest_from_dir("demo", &other, &test_limits())
                .unwrap()
                .digest;

        let (outcome, seals) = staged_rerun(tmp.path(), &src, &key, &stage_digest);
        let error = outcome.expect_err("an installed tree that is not the stage is refused");
        assert_eq!(
            refusal_code(&error),
            Some(ApplyRefusalCode::BundleAlreadyInstalled)
        );
        // The crashed install passed admission and its registration is real:
        // it is sealed first, so the node boots, and the refusal comes after.
        assert_eq!(seals, 1);
        assert!(tmp.path().join(".ai/node/bundles/demo.yaml").is_file());
        assert!(target.is_dir());
    }

    #[test]
    fn replace_dir_does_not_overwrite_source_runtime_artifacts() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        let bundles = tmp.path().join(".ai/bundles");
        let target = bundles.join("ryeos-ui");
        fs::create_dir_all(src.join(".ai/bin/x86_64-unknown-linux-gnu")).unwrap();
        fs::create_dir_all(target.join(".ai/bin/x86_64-unknown-linux-gnu")).unwrap();
        fs::write(src.join(".ai/bin/x86_64-unknown-linux-gnu/web"), b"new").unwrap();
        fs::write(target.join(".ai/bin/x86_64-unknown-linux-gnu/web"), b"old").unwrap();

        replace_for_test(&src, &target, true);

        assert_eq!(
            fs::read(target.join(".ai/bin/x86_64-unknown-linux-gnu/web")).unwrap(),
            b"new"
        );
    }

    fn public_route(id: &str, root: &str) -> ryeos_engine::route_raw::RawRouteSpec {
        let mut raw: ryeos_engine::route_raw::RawRouteSpec = serde_yaml::from_str(&format!(
            "id: {id}\npath: /{id}\nmethods: [GET]\nauth: none\nlimits:\n  body_bytes_max: 0\nresponse:\n  mode: static\n  status: 200\n  content_type: text/plain\n  body_b64: aGVsbG8=\npublic: true\n"
        ))
        .unwrap();
        raw.source_file = std::path::PathBuf::from(format!("{root}/.ai/node/routes/{id}.yaml"));
        raw
    }

    fn naming(
        ids: &[&str],
    ) -> ryeos_node_runtime::node_policy::sections::public_routes::PublicRoutesPolicy {
        use ryeos_node_runtime::node_policy::sections::public_routes::{
            PublicRouteLimits, PublicRoutesPolicy,
        };
        let mut policy = PublicRoutesPolicy::as_shipped();
        for id in ids {
            policy.routes.insert(
                (*id).to_owned(),
                PublicRouteLimits {
                    daily_bytes: 1,
                    concurrent_max: 1,
                },
            );
        }
        policy
    }

    fn refusal(error: &anyhow::Error) -> (String, serde_json::Value) {
        let Some(ryeos_app::handler_error::HandlerError::Structured { code, status, body }) =
            ryeos_app::handler_error::extract_handler_error(error)
        else {
            panic!("typed refusal: {error:#}");
        };
        assert_eq!(status, 422);
        (code, body)
    }

    #[test]
    fn a_bundle_installs_only_when_every_public_route_is_named() {
        let candidate = Path::new("/stage/public-page");
        let routes = vec![
            public_route("public/front", "/stage/public-page"),
            public_route("public/assets/style", "/stage/public-page"),
            // Another bundle's left-out public route does not block this one.
            public_route("public/other", "/node/.ai/bundles/other"),
        ];

        refuse_unadmitted_public_routes(
            &routes,
            candidate,
            &naming(&["public/front", "public/assets/style"]),
        )
        .expect("every public route of the bundle is named");

        let partial =
            refuse_unadmitted_public_routes(&routes, candidate, &naming(&["public/front"]))
                .unwrap_err();
        let (code, body) = refusal(&partial);
        assert_eq!(code, PUBLIC_ROUTE_NOT_NAMED);
        assert_eq!(body["routes"], serde_json::json!(["public/assets/style"]));

        let none = refuse_unadmitted_public_routes(&routes, candidate, &naming(&[])).unwrap_err();
        let (code, body) = refusal(&none);
        assert_eq!(code, PUBLIC_ROUTE_NOT_NAMED);
        assert_eq!(
            body["routes"],
            serde_json::json!(["public/assets/style", "public/front"])
        );
    }

    #[test]
    fn a_bundle_whose_public_route_is_keyed_or_not_static_is_refused() {
        let candidate = Path::new("/stage/public-page");
        let mut keyed = public_route("public/front", "/stage/public-page");
        keyed.auth = "browser_session".into();
        let mut json = public_route("public/data", "/stage/public-page");
        json.response.mode = "json".into();
        let error = refuse_unadmitted_public_routes(
            &[keyed, json],
            candidate,
            &naming(&["public/front", "public/data"]),
        )
        .unwrap_err();
        let (code, body) = refusal(&error);
        assert_eq!(code, PUBLIC_ROUTE_NOT_KEYLESS_STATIC);
        assert_eq!(
            body["routes"],
            serde_json::json!(["public/data", "public/front"])
        );
    }
}

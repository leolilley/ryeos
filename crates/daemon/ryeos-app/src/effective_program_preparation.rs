//! Shared source-time hook and validator projection, before product selection.
use ryeos_engine::canonical_ref::CanonicalRef;
use ryeos_engine::hooks::{EffectiveHookPlan, HookLayer};
use ryeos_engine::launch_config::LaunchConfigSnapshotSet;
use ryeos_engine::resolution::ResolutionOutput;

/// Shared preselection phase for a kind with no configured hook authority.
/// Used by current immutable verifier checks which deliberately do not reopen
/// the launch's project hook context.
pub fn prepare_hookless_preselection_effective_program(
    engine: &ryeos_engine::engine::Engine,
    kind: &str,
    resolution: &mut ResolutionOutput,
) -> anyhow::Result<()> {
    anyhow::ensure!(
        engine
            .kinds
            .get(kind)
            .and_then(|schema| schema.execution.as_ref())
            .and_then(|execution| execution.hooks.as_ref())
            .is_none(),
        "hook-capable program requires exact hook-source preparation"
    );
    let validation = engine.effective_validators.validate(kind, resolution)?;
    *resolution = ryeos_engine::effective_program::capture_validated_effective_program_derived(
        resolution.clone(),
        validation,
    )?;
    Ok(())
}

/// Admit the source-owned semantic projections before selecting product inputs.
/// This mints no execution authority: finalization still revalidates these
/// projections and the returned mutable configuration proof.
pub fn prepare_preselection_effective_program(
    engine: &ryeos_engine::engine::Engine,
    resolved: &ryeos_engine::contracts::ResolvedItem,
    resolution: &mut ResolutionOutput,
    roots: &ryeos_engine::item_resolution::ResolutionRoots,
    trust_store: &ryeos_engine::trust::TrustStore,
    project: Option<(
        &std::path::Path,
        &dyn ryeos_engine::project_content::AuthoritativeProjectContent,
    )>,
) -> anyhow::Result<Option<LaunchConfigSnapshotSet>> {
    anyhow::ensure!(
        resolution.root.resolved_ref == resolved.canonical_ref.to_string()
            && resolution.root.source_content_digest == resolved.content_hash,
        "preselection source does not match the exact resolved item"
    );
    let kind = &resolved.canonical_ref.kind;
    let hook_contract = engine
        .kinds
        .get(kind)
        .and_then(|schema| schema.execution.as_ref())
        .and_then(|execution| execution.hooks.as_ref());
    let (prepared, validation, snapshots) = if let Some(hooks) = hook_contract {
        anyhow::ensure!(
            !resolution
                .composed
                .derived
                .contains_key(&hooks.plan_derived),
            "fresh preselection source already carries a hook projection"
        );
        let declared = resolution.composed.policy_fact_string_seq("effective_caps");
        ryeos_bundle::runtime_authority::reject_disallowed_composed_grants(&declared)
            .map_err(anyhow::Error::msg)?;
        let minted = crate::runtime_capability_admission::mint_runtime_capability_caps(
            resolution.composed.composed.get("requires"),
            resolved,
            resolution.effective_trust_class,
            engine,
        )
        .map_err(anyhow::Error::msg)?;
        let caps = declared
            .into_iter()
            .chain(minted)
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect::<Vec<_>>();
        let config_roots = engine.launch_config_roots(roots);
        let declarations = ryeos_engine::hooks::hook_source_declarations();
        let snapshots = match project {
            Some((project_root, content)) =>
                ryeos_engine::launch_config::load_launch_config_snapshots_with_proof_under_project_authority(
                    &declarations, &config_roots, &engine.parser_dispatcher,
                    &engine.parser_dispatcher, &engine.kinds, trust_store,
                    &engine.node_trust_store, project_root, content,
                )?,
            None => ryeos_engine::launch_config::load_launch_config_snapshots_with_proof(
                &declarations, &config_roots, &engine.parser_dispatcher,
                &engine.parser_dispatcher, &engine.kinds, trust_store, &engine.node_trust_store,
            )?,
        };
        let (prepared, validation) = capture_and_validate_with_hook_snapshots(
            engine,
            kind,
            resolution.clone(),
            &caps,
            trust_store,
            &snapshots,
        )?;
        (prepared, validation, Some(snapshots))
    } else {
        let validation = engine.effective_validators.validate(kind, resolution)?;
        (resolution.clone(), validation, None)
    };
    *resolution = ryeos_engine::effective_program::capture_validated_effective_program_derived(
        prepared, validation,
    )?;
    Ok(snapshots)
}

pub fn capture_and_validate_with_hook_snapshots(
    engine: &ryeos_engine::engine::Engine,
    kind: &str,
    mut resolution: ResolutionOutput,
    effective_caps: &[String],
    trust_store: &ryeos_engine::trust::TrustStore,
    snapshots: &LaunchConfigSnapshotSet,
) -> Result<
    (
        ResolutionOutput,
        ryeos_engine::effective_program::EffectiveValidationSuccess,
    ),
    anyhow::Error,
> {
    let hook_contract = engine
        .kinds
        .get(kind)
        .and_then(|schema| schema.execution.as_ref())
        .and_then(|execution| execution.hooks.as_ref())
        .ok_or_else(|| {
            anyhow::anyhow!("managed runtime kind `{kind}` has no signed hook contract")
        })?;
    let authored =
        value_at_composed_path(&resolution.composed.composed, &hook_contract.authored_path);
    let known_event_contracts = engine
        .kinds
        .kinds()
        .filter_map(|known_kind| {
            engine
                .kinds
                .get(known_kind)
                .and_then(|schema| schema.execution.as_ref())
                .and_then(|execution| execution.hooks.as_ref())
                .map(|hooks| (known_kind.to_string(), hooks.events.clone()))
        })
        .collect::<std::collections::BTreeMap<_, _>>();
    let plan = ryeos_engine::hooks::capture_effective_hook_plan(
        kind,
        hook_contract.events.clone(),
        &known_event_contracts,
        authored,
        effective_caps.to_vec(),
        &snapshots.snapshots,
    )
    .map_err(|error| anyhow::anyhow!(error))?;
    for (layer, body) in plan
        .iter_layers()
        .filter(|(layer, _)| *layer != HookLayer::Authored)
    {
        ryeos_bundle::runtime_authority::reject_disallowed_composed_grants(&body.dispatch_caps)
            .map_err(|error| {
                anyhow::anyhow!(
                    "{} hook source declares an inadmissible dispatch grant: {error}",
                    layer.as_str()
                )
            })?;
    }
    validate_captured_hook_plan_pre_spawn(&plan)?;
    crate::admitted_program_trust::validate_hook_plan_current_trust(engine, trust_store, &plan)?;
    resolution.composed.derived.insert(
        hook_contract.plan_derived.clone(),
        plan.to_value().map_err(|error| anyhow::anyhow!(error))?,
    );

    let validation = engine
        .effective_validators
        .validate(kind, &resolution)
        .map_err(|error| anyhow::anyhow!(error))?;
    Ok((resolution, validation))
}

/// Compile and validate the exact admitted plan before any callback token,
/// capsule, or runtime process exists. This deliberately reuses the runtime's
/// single compiler and hook-action parser; admission does not maintain a
/// second expression/template or action grammar.
pub fn validate_captured_hook_plan_pre_spawn(
    plan: &EffectiveHookPlan,
) -> Result<(), anyhow::Error> {
    ryeos_runtime::compile_effective_hook_plan(plan, &ryeos_runtime::CompilationLimits::default())
        .map_err(|error| anyhow::anyhow!("captured hook plan does not compile: {error}"))?;

    for (layer, body) in plan.iter_layers() {
        for hook in &body.hooks {
            let action = ryeos_runtime::callback::parse_hook_action(hook.action.clone()).map_err(
                |error| {
                    anyhow::anyhow!(
                        "{} hook `{}` has an invalid action: {error}",
                        layer.as_str(),
                        hook.id
                    )
                },
            )?;
            if action.thread != "inline" {
                anyhow::bail!("{} hook `{}` must dispatch inline", layer.as_str(), hook.id);
            }
            if layer == HookLayer::Authored {
                continue;
            }
            validate_configured_action_grants(
                layer,
                &hook.id,
                &action.item_id,
                action.ref_bindings.values().map(String::as_str),
                &body.dispatch_caps,
            )?;
        }
    }
    Ok(())
}

#[derive(Debug, PartialEq, Eq)]
enum DispatchTargetCoverage {
    Exact(String),
    Kind(String),
    Any,
}

/// Derive a conservative capability requirement for an unrendered target.
/// A literal ref requires its exact execution cap. A template with a literal
/// canonical kind prefix requires kind-wide authority; an arbitrary template
/// requires execute authority across kinds. This is intentionally broader
/// than the eventual rendered target, so passing it proves the source grant
/// covers every value the template could produce.
fn dispatch_target_coverage(target: &str) -> Result<DispatchTargetCoverage, anyhow::Error> {
    let Some(template_start) = target.find("${") else {
        let canonical = CanonicalRef::parse(target).map_err(|error| {
            anyhow::anyhow!("configured hook target `{target}` is not canonical: {error}")
        })?;
        return Ok(DispatchTargetCoverage::Exact(format!(
            "ryeos.execute.{}.{}",
            canonical.kind, canonical.bare_id
        )));
    };

    let literal_prefix = &target[..template_start];
    if let Some((kind, _)) = literal_prefix.split_once(':')
        && CanonicalRef::parse(&format!("{kind}:probe")).is_ok()
    {
        return Ok(DispatchTargetCoverage::Kind(kind.to_string()));
    }
    Ok(DispatchTargetCoverage::Any)
}

fn grant_covers_target(grant: &str, target: &DispatchTargetCoverage) -> bool {
    match target {
        DispatchTargetCoverage::Exact(required) => {
            ryeos_runtime::authorizer::cap_matches(grant, required)
        }
        DispatchTargetCoverage::Kind(kind) => {
            matches!(grant, "*" | "ryeos.*" | "ryeos.execute.*")
                || grant == format!("ryeos.execute.{kind}")
                || grant == format!("ryeos.execute.{kind}.*")
                || grant == "ryeos.execute.*.*"
        }
        DispatchTargetCoverage::Any => {
            matches!(
                grant,
                "*" | "ryeos.*" | "ryeos.execute.*" | "ryeos.execute.*.*"
            )
        }
    }
}

fn validate_configured_action_grants<'a>(
    layer: HookLayer,
    hook_id: &str,
    item_id: &'a str,
    ref_bindings: impl Iterator<Item = &'a str>,
    dispatch_caps: &[String],
) -> Result<(), anyhow::Error> {
    for target in std::iter::once(item_id).chain(ref_bindings) {
        let coverage = dispatch_target_coverage(target)?;
        if !dispatch_caps
            .iter()
            .any(|grant| grant_covers_target(grant, &coverage))
        {
            anyhow::bail!(
                "{} hook `{hook_id}` action target `{target}` is not covered by its source-owned dispatch grants",
                layer.as_str()
            );
        }
    }
    Ok(())
}

fn value_at_composed_path<'a>(
    value: &'a serde_json::Value,
    path: &[String],
) -> Option<&'a serde_json::Value> {
    path.iter()
        .try_fold(value, |current, part| current.get(part))
}

//! Read-only projection of the exact effective program a fresh managed launch
//! would admit under current authority.
//!
//! This module owns the common hook-capture/finalization path used by both
//! managed launch and UI projection. Projection never mints a token, capsule,
//! thread, or runtime and refuses kinds with launch-only augmentation.

use std::path::{Path, PathBuf};

use ryeos_engine::canonical_ref::CanonicalRef;
use ryeos_engine::contracts::{PlanContext, SubjectResolutionAuthority};
use ryeos_engine::effective_program::FinalizedEffectiveProgram;
use ryeos_engine::hooks::{EffectiveHookPlan, HookLayer};
use ryeos_engine::resolution::{EffectiveDefinitionDigest, ResolutionOutput};

use crate::dispatch_error::DispatchError;

#[derive(Debug, Clone)]
pub struct EffectiveProgramProjection {
    pub canonical_ref: String,
    pub kind: String,
    pub root_source_content_digest: String,
    pub root_raw_content_digest: String,
    pub effective_definition_digest: EffectiveDefinitionDigest,
    pub resolution: ResolutionOutput,
}

/// Resolve, compose, capture, validate, and finalize one current graph through
/// the same authority path used by fresh managed launch. The returned value is
/// evidence only and cannot be converted into launch authority.
pub fn prepare_effective_program_projection(
    engine: &ryeos_engine::engine::Engine,
    plan_context: &PlanContext,
    project_root: Option<&Path>,
    canonical_ref: &CanonicalRef,
) -> Result<EffectiveProgramProjection, DispatchError> {
    plan_context
        .subject_resolution_authority
        .validate_for_materialized_root(project_root)
        .map_err(|error| DispatchError::Internal(error.context("validate projection authority")))?;
    if !matches!(
        plan_context.subject_resolution_authority,
        SubjectResolutionAuthority::LiveFs | SubjectResolutionAuthority::Projectless
    ) {
        return Err(DispatchError::Internal(anyhow::anyhow!(
            "current effective-program projection requires live or projectless authority"
        )));
    }

    let resolved = engine
        .resolve(plan_context, canonical_ref)
        .map_err(|error| DispatchError::Internal(anyhow::anyhow!(error)))?;
    let root_source_content_digest = resolved.content_hash.clone();
    let root_raw_content_digest = resolved.raw_content_digest.clone();
    let kind = resolved.kind.clone();
    let attestation = engine
        .verify_attested(plan_context, resolved.clone())
        .map_err(|error| DispatchError::Internal(anyhow::anyhow!(error)))?;
    let closure = engine
        .resolve_verified_resolution_closure(
            plan_context,
            &attestation,
            project_root.map(PathBuf::from),
        )
        .map_err(|error| DispatchError::Internal(anyhow::anyhow!(error)))?;
    let (resolution, _, _) = closure.into_parts();

    super::launch::enforce_effective_trust(
        resolution.effective_trust_class,
        &canonical_ref.to_string(),
        &kind,
    )
    .map_err(|error| DispatchError::Internal(anyhow::anyhow!(error)))?;
    let execution = engine
        .kinds
        .get(&kind)
        .and_then(|schema| schema.execution())
        .ok_or_else(|| {
            DispatchError::Internal(anyhow::anyhow!(
                "effective-program projection kind `{kind}` is not executable"
            ))
        })?;
    if !execution.launch_augmentations.is_empty() {
        return Err(DispatchError::Internal(anyhow::anyhow!(
            "effective-program projection refuses kind `{kind}` because it declares launch-only augmentation"
        )));
    }

    let declared_caps = super::launch::derive_effective_caps(&resolution.composed);
    ryeos_bundle::runtime_authority::reject_disallowed_composed_grants(&declared_caps)
        .map_err(|error| DispatchError::Internal(anyhow::anyhow!(error)))?;
    let runtime_caps = crate::dispatch::mint_runtime_capability_caps(
        resolution.composed.composed.get("requires"),
        &resolved,
        resolution.effective_trust_class,
        engine,
    )
    .map_err(|error| DispatchError::Internal(anyhow::anyhow!(error)))?;
    let effective_caps = declared_caps
        .into_iter()
        .chain(runtime_caps)
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    let roots = engine.resolution_roots(project_root.map(PathBuf::from));
    let request_snapshot = engine
        .effective_request_snapshot(project_root, &plan_context.subject_resolution_authority)
        .map_err(|error| DispatchError::Internal(anyhow::anyhow!(error)))?;
    let effective_program = capture_and_finalize_fresh_effective_program(
        engine,
        &kind,
        resolution,
        &effective_caps,
        &roots,
        &request_snapshot.parser_dispatcher,
        &request_snapshot.trust_store,
        None,
    )?;
    let effective_definition_digest = effective_program.effective_definition_digest().clone();
    let (resolution, _) = effective_program.into_parts();
    Ok(EffectiveProgramProjection {
        canonical_ref: canonical_ref.to_string(),
        kind,
        root_source_content_digest,
        root_raw_content_digest,
        effective_definition_digest,
        resolution,
    })
}

pub(crate) fn capture_and_finalize_fresh_effective_program(
    engine: &ryeos_engine::engine::Engine,
    kind: &str,
    mut resolution: ResolutionOutput,
    effective_caps: &[String],
    roots: &ryeos_engine::item_resolution::ResolutionRoots,
    parsers: &ryeos_engine::parsers::ParserDispatcher,
    trust_store: &ryeos_engine::trust::TrustStore,
    materialization: Option<&ryeos_app::resolution_cache::ResolutionMaterializationBinding>,
) -> Result<FinalizedEffectiveProgram, DispatchError> {
    let hook_contract = engine
        .kinds
        .get(kind)
        .and_then(|schema| schema.execution.as_ref())
        .and_then(|execution| execution.hooks.as_ref())
        .ok_or_else(|| {
            DispatchError::Internal(anyhow::anyhow!(
                "managed runtime kind `{kind}` has no signed hook contract"
            ))
        })?;
    let config_roots = engine.launch_config_roots(roots);
    let snapshots = super::launch_preparation::load_launch_config_set_under_current_authority(
        &ryeos_engine::hooks::hook_source_declarations(),
        &config_roots,
        parsers,
        engine,
        trust_store,
        materialization,
    )?;
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
    .map_err(|error| DispatchError::Internal(anyhow::anyhow!(error)))?;
    for (layer, body) in plan
        .iter_layers()
        .filter(|(layer, _)| *layer != HookLayer::Authored)
    {
        ryeos_bundle::runtime_authority::reject_disallowed_composed_grants(&body.dispatch_caps)
            .map_err(|error| {
                DispatchError::Internal(anyhow::anyhow!(
                    "{} hook source declares an inadmissible dispatch grant: {error}",
                    layer.as_str()
                ))
            })?;
    }
    validate_captured_hook_plan_pre_spawn(&plan)?;
    super::admitted_trust::validate_hook_plan_current_trust(engine, trust_store, &plan)
        .map_err(DispatchError::Internal)?;
    resolution.composed.derived.insert(
        hook_contract.plan_derived.clone(),
        plan.to_value()
            .map_err(|error| DispatchError::Internal(anyhow::anyhow!(error)))?,
    );

    let validation = engine
        .effective_validators
        .validate(kind, &resolution)
        .map_err(|error| DispatchError::Internal(anyhow::anyhow!(error)))?;
    let candidate =
        ryeos_engine::effective_program::lock_validated_effective_program(resolution, validation)
            .map_err(|error| DispatchError::Internal(anyhow::anyhow!(error)))?;
    let project = materialization
        .map(|binding| binding.authoritative_project_content())
        .transpose()
        .map_err(DispatchError::Internal)?
        .flatten()
        .map(|(root, content)| {
            (
                root,
                content as &dyn ryeos_engine::project_content::AuthoritativeProjectContent,
            )
        });
    let proof = ryeos_engine::effective_program::prove_finalization_authority(
        &candidate,
        &[snapshots.dependency_proof],
        &config_roots,
        project,
    )
    .map_err(|error| DispatchError::Internal(anyhow::anyhow!(error)))?;
    ryeos_engine::effective_program::finalize_effective_program(candidate, proof)
        .map_err(|error| DispatchError::Internal(anyhow::anyhow!(error)))
}

/// Compile and validate the exact admitted plan before any callback token,
/// capsule, or runtime process exists. This deliberately reuses the runtime's
/// single compiler and hook-action parser; admission does not maintain a
/// second expression/template or action grammar.
pub(crate) fn validate_captured_hook_plan_pre_spawn(
    plan: &EffectiveHookPlan,
) -> Result<(), DispatchError> {
    ryeos_runtime::compile_effective_hook_plan(plan, &ryeos_runtime::CompilationLimits::default())
        .map_err(|error| {
            DispatchError::Internal(anyhow::anyhow!(
                "captured hook plan does not compile: {error}"
            ))
        })?;

    for (layer, body) in plan.iter_layers() {
        for hook in &body.hooks {
            let action = ryeos_runtime::callback::parse_hook_action(hook.action.clone()).map_err(
                |error| {
                    DispatchError::Internal(anyhow::anyhow!(
                        "{} hook `{}` has an invalid action: {error}",
                        layer.as_str(),
                        hook.id
                    ))
                },
            )?;
            if action.thread != "inline" {
                return Err(DispatchError::Internal(anyhow::anyhow!(
                    "{} hook `{}` must dispatch inline",
                    layer.as_str(),
                    hook.id
                )));
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
fn dispatch_target_coverage(target: &str) -> Result<DispatchTargetCoverage, DispatchError> {
    let Some(template_start) = target.find("${") else {
        let canonical = CanonicalRef::parse(target).map_err(|error| {
            DispatchError::Internal(anyhow::anyhow!(
                "configured hook target `{target}` is not canonical: {error}"
            ))
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
) -> Result<(), DispatchError> {
    for target in std::iter::once(item_id).chain(ref_bindings) {
        let coverage = dispatch_target_coverage(target)?;
        if !dispatch_caps
            .iter()
            .any(|grant| grant_covers_target(grant, &coverage))
        {
            return Err(DispatchError::Internal(anyhow::anyhow!(
                "{} hook `{hook_id}` action target `{target}` is not covered by its source-owned dispatch grants",
                layer.as_str()
            )));
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

#[cfg(test)]
mod tests {
    use super::*;
    use ryeos_engine::contracts::ItemSpace;
    use ryeos_engine::hooks::{
        EFFECTIVE_HOOK_PLAN_SCHEMA, EffectiveHookLayer, ExpressionCondition, HOOK_CONTEXT_SCHEMA,
        HookContextContract, HookDefinition, HookEventContract, HookResultMode, HookSourceEvidence,
    };
    use ryeos_engine::resolution::TrustClass;
    use std::collections::{BTreeMap, BTreeSet};

    fn plan(action: serde_json::Value, caps: Vec<&str>) -> EffectiveHookPlan {
        let hook = HookDefinition {
            id: "configured-audit".to_string(),
            event: "graph_completed".to_string(),
            result: HookResultMode::Observation,
            condition: ExpressionCondition::Absent,
            action,
        };
        EffectiveHookPlan {
            schema: EFFECTIVE_HOOK_PLAN_SCHEMA.to_string(),
            owner_kind: "graph".to_string(),
            event_contracts: BTreeMap::from([(
                "graph_completed".to_string(),
                HookEventContract {
                    context_contract: HookContextContract {
                        schema: HOOK_CONTEXT_SCHEMA.to_string(),
                        allowed_roots: BTreeSet::from(["event".to_string()]),
                    },
                    allowed_results: BTreeSet::from([HookResultMode::Observation]),
                },
            )]),
            authored: EffectiveHookLayer::empty(),
            builtin: EffectiveHookLayer::empty(),
            infrastructure: EffectiveHookLayer::empty(),
            context: EffectiveHookLayer::empty(),
            operator: EffectiveHookLayer {
                hooks: vec![hook],
                dispatch_caps: caps.into_iter().map(str::to_string).collect(),
            },
            project: EffectiveHookLayer::empty(),
            sources: vec![HookSourceEvidence {
                layer: HookLayer::Operator,
                canonical_ref: "config:ryeos-runtime/hooks/operator".to_string(),
                source_space: ItemSpace::Node,
                trust_class: TrustClass::TrustedNode,
                signer_fingerprint: "f".repeat(64),
                source_raw_content_digest: "a".repeat(64),
            }],
        }
    }

    #[test]
    fn configured_action_must_be_covered_by_source_owned_grants() {
        let action = serde_json::json!({
            "item_id": "tool:test/audit",
            "ref_bindings": {},
            "params": {},
        });
        validate_captured_hook_plan_pre_spawn(&plan(
            action.clone(),
            vec!["ryeos.execute.tool.test/audit"],
        ))
        .unwrap();
        let error = validate_captured_hook_plan_pre_spawn(&plan(
            action,
            vec!["ryeos.execute.tool.test/other"],
        ))
        .unwrap_err();
        assert!(error.to_string().contains("source-owned dispatch grants"));
    }

    #[test]
    fn templated_target_requires_kind_wide_source_authority() {
        let action = serde_json::json!({
            "item_id": "tool:${event.target}",
            "ref_bindings": {},
            "params": {},
        });
        validate_captured_hook_plan_pre_spawn(&plan(action.clone(), vec!["ryeos.execute.tool.*"]))
            .unwrap();
        assert!(
            validate_captured_hook_plan_pre_spawn(&plan(
                action,
                vec!["ryeos.execute.tool.test/audit"],
            ))
            .is_err()
        );
    }

    #[test]
    fn plan_references_are_compiled_before_spawn() {
        let mut invalid = plan(
            serde_json::json!({
                "item_id": "tool:test/audit",
                "ref_bindings": {},
                "params": "${state.secret}",
            }),
            vec!["ryeos.execute.tool.test/audit"],
        );
        invalid.operator.hooks[0].result = HookResultMode::Observation;
        let error = validate_captured_hook_plan_pre_spawn(&invalid).unwrap_err();
        assert!(error.to_string().contains("undeclared root `state`"));
    }
}

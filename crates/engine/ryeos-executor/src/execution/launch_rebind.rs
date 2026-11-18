//! Operator rebind of ref bindings on a continuation.
//!
//! A continuation normally reuses the prepared launch sealed in its source
//! capsule. An operator rebind names a different item at one or more ref
//! bindings, so the part of the prepared launch that launch preparation
//! derives from the bound items must be prepared again. This module holds the
//! mechanical rules for joining that fresh preparation onto the admitted one:
//! which fields are replaced, which must come out equal, and which signed
//! limits the replacement has to meet. It interprets no runtime-owned value.

use std::collections::{BTreeMap, BTreeSet};

use ryeos_engine::runtime_registry::RuntimeFactDecl;
use serde::Serialize;
use serde_json::Value;

use super::launch_preparation::PreparedRuntimeLaunch;
use crate::dispatch_error::DispatchError;

/// The admitted runtime contract does not let an operator rebind the name, or
/// the thread is of a kind that cannot be rebound at all.
pub(crate) const REBIND_NOT_PERMITTED: &str = "ref_binding_rebind_not_permitted";
/// The source thread cannot continue into a rebound invocation.
pub(crate) const REBIND_INVOCATION_REFUSED: &str = "ref_binding_rebind_refused";
/// Re-preparation changed a part of the launch a continuation must keep.
pub(crate) const REBIND_CHANGES_SEALED_LAUNCH: &str = "ref_binding_rebind_changes_sealed_launch";
/// The rebound launch would change the admitted external effect class, or
/// let more retries follow an outcome that leaves the effect unknown.
pub(crate) const REBIND_CHANGES_EFFECT_AUTHORITY: &str =
    "ref_binding_rebind_changes_effect_authority";
/// The rebound launch would gain or lose its financial authority.
pub(crate) const REBIND_CHANGES_FINANCIAL_AUTHORITY: &str =
    "ref_binding_rebind_changes_financial_authority";
/// The source's operational launch metadata no longer matches the capsule
/// rooted for it.
pub(crate) const REBIND_SOURCE_NOT_ADMITTED: &str = "ref_binding_rebind_source_not_admitted";
/// The runtime the thread was admitted with is no longer the installed one.
pub(crate) const REBIND_RUNTIME_NOT_INSTALLED: &str = "ref_binding_rebind_runtime_not_installed";
/// A runtime fact of the rebound launch is below its signed rebind minimum.
pub(crate) const REBIND_FACT_BELOW_MINIMUM: &str = "ref_binding_rebind_fact_below_minimum";

/// A refusal of the rebind itself, attributed to the first rebound name.
pub(crate) fn rebind_refusal(
    code: &str,
    message: impl Into<String>,
    names: &BTreeSet<String>,
) -> DispatchError {
    DispatchError::LaunchPolicyForbidden {
        code: code.to_owned(),
        message: message.into(),
        binding: names.iter().next().cloned(),
    }
}

/// What a rebound successor changed, for the daemon's audit of the rebind.
#[derive(Debug, Clone, PartialEq)]
pub struct ReboundLaunch {
    /// The thread whose admitted launch was rebound.
    pub source_thread_id: String,
    /// One entry per rebound ref binding, in name order.
    pub changes: Vec<ReboundBinding>,
    /// The source launch's runtime facts that the runtime contract marks for
    /// the rebind record.
    pub from_facts: BTreeMap<String, Value>,
    /// The same facts of the re-prepared launch.
    pub to_facts: BTreeMap<String, Value>,
}

/// One rebound ref binding: the item the source sealed and its replacement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReboundBinding {
    pub name: String,
    pub from: String,
    pub to: String,
}

impl ReboundLaunch {
    /// `runtime_facts` is the fact contract sealed with the source. Only the
    /// facts it marks `rebind_record` are carried, each side keeping the ones
    /// its launch stated; every other fact stays out of the record.
    pub(crate) fn between(
        source_thread_id: &str,
        runtime_facts: &BTreeMap<String, RuntimeFactDecl>,
        source: &PreparedRuntimeLaunch,
        rebound: &PreparedRuntimeLaunch,
        names: &BTreeSet<String>,
    ) -> anyhow::Result<Self> {
        let recorded = |facts: &BTreeMap<String, Value>| -> BTreeMap<String, Value> {
            facts
                .iter()
                .filter(|(name, _)| {
                    runtime_facts
                        .get(*name)
                        .is_some_and(|declaration| declaration.rebind_record)
                })
                .map(|(name, value)| (name.clone(), value.clone()))
                .collect()
        };
        let changes = names
            .iter()
            .map(|name| {
                let from = source.binding_records.get(name).ok_or_else(|| {
                    anyhow::anyhow!("rebound source launch has no record for ref binding `{name}`")
                })?;
                let to = rebound.binding_records.get(name).ok_or_else(|| {
                    anyhow::anyhow!("rebound launch has no record for ref binding `{name}`")
                })?;
                Ok(ReboundBinding {
                    name: name.clone(),
                    from: from.canonical_ref.clone(),
                    to: to.canonical_ref.clone(),
                })
            })
            .collect::<anyhow::Result<Vec<_>>>()?;
        Ok(Self {
            source_thread_id: source_thread_id.to_owned(),
            changes,
            from_facts: recorded(&source.runtime_facts),
            to_facts: recorded(&rebound.runtime_facts),
        })
    }
}

/// Cache-key identity of the executor chain a rebound launch runs on.
///
/// A fresh launch keys its preparation by the attestation of the executor it
/// has just verified in the installed bundles. A rebound continuation keeps
/// the executor its source capsule admitted and verifies nothing new, so its
/// preparation is keyed by that admitted artifact identity instead. The two
/// forms never collide.
pub(crate) fn admitted_executor_chain_identity(
    artifact_identity: &ryeos_state::objects::AdmittedLaunchArtifactIdentity,
) -> anyhow::Result<String> {
    let value = serde_json::json!({
        "schema_version": 1,
        "admitted_artifact_identity": artifact_identity,
    });
    Ok(lillux::sha256_hex(
        lillux::canonical_json(&value)?.as_bytes(),
    ))
}

fn same_value<T: Serialize>(left: &T, right: &T) -> Result<bool, DispatchError> {
    let value = |side: &T| {
        serde_json::to_value(side).map_err(|error| {
            DispatchError::Internal(anyhow::anyhow!(
                "serialize prepared launch field for rebind comparison: {error}"
            ))
        })
    };
    Ok(value(left)? == value(right)?)
}

/// The one capsule an operator rebind may re-prepare: the capsule its
/// continuation source rooted on this node, for a successor that does not
/// exist yet.
///
/// A row that already has its own capsule (a re-driven or recovered
/// successor, rebound at birth or not) is launched from that capsule and is
/// never prepared again, and a capsule transferred from another site is
/// admitted by the cross-site rules only. Both are refused here whatever
/// names the caller passes.
pub(crate) fn rebind_source_capsule<'a, Capsule>(
    persisted: Option<&'a Capsule>,
    transferred: Option<&'a Capsule>,
    inherited: Option<&'a Capsule>,
    has_seed: bool,
) -> anyhow::Result<&'a Capsule> {
    match (persisted, transferred, inherited) {
        (None, None, Some(source)) if has_seed => Ok(source),
        _ => anyhow::bail!(
            "an operator rebind is prepared only for a new successor of a locally admitted source"
        ),
    }
}

/// Require that the runtime a thread was admitted with is still the runtime
/// installed under its ref.
///
/// A rebind runs the launch preparer registered for the runtime now and
/// hands its output to the executor the capsule sealed. That pairing is only
/// the admitted one while the installed runtime descriptor is byte-for-byte
/// the sealed one. After a runtime upgrade the thread still continues on its
/// sealed bindings, but it cannot be rebound.
pub(crate) fn require_sealed_runtime_installed(
    runtime_ref: &str,
    sealed_content_hash: &str,
    installed_content_hash: Option<&str>,
    names: &BTreeSet<String>,
) -> Result<(), DispatchError> {
    if installed_content_hash == Some(sealed_content_hash) {
        return Ok(());
    }
    Err(rebind_refusal(
        REBIND_RUNTIME_NOT_INSTALLED,
        format!(
            "the runtime this thread was admitted with (`{runtime_ref}`) is no longer installed; start a new conversation to change its model"
        ),
        names,
    ))
}

/// The external-effect authority of a rebound launch must not differ from
/// the source's in what it lets the thread do: the admitted effect class
/// stays exactly the source's, and no more retries may follow an outcome
/// that leaves the effect unknown.
///
/// The class decides whether provider calls are durably recorded at all, so
/// neither a wider class nor dropping to none is a rebind. The authority
/// family is left to the binding: it names the wire family of the provider
/// the bound item selects, and the daemon checks each call against the
/// family sealed in that thread's own capsule.
fn effect_authority_unchanged(
    source: Option<&ryeos_effect_contract::AdmittedExternalEffectAuthority>,
    rebound: Option<&ryeos_effect_contract::AdmittedExternalEffectAuthority>,
) -> Result<(), &'static str> {
    match (source, rebound) {
        (None, None) => Ok(()),
        (Some(source), Some(rebound)) => {
            if source.admitted_effect_class != rebound.admitted_effect_class {
                return Err(
                    "rebinding would change the external effect class the thread was admitted with",
                );
            }
            if rebound.unknown_outcome_retry_ceiling > source.unknown_outcome_retry_ceiling {
                return Err(
                    "rebinding would allow more retries after an unknown outcome than the thread was admitted with",
                );
            }
            Ok(())
        }
        _ => Err("rebinding would add or remove the thread's external-effect authority"),
    }
}

/// A rebound launch carries financial authority exactly when its source did.
///
/// The accounting scope a thread charges is minted with its first launch and
/// copied to every successor. A source with no financial authority minted no
/// item-level budget, so a successor that gained one would have nothing to
/// reserve against and fail inside its run; a successor that lost one would
/// keep running with no accounting at all. Which authority it is (the tariff
/// and spend bound of the bound provider) is the binding's to change.
fn financial_authority_presence_held(
    source_has_authority: bool,
    rebound_has_authority: bool,
) -> Result<(), &'static str> {
    match (source_has_authority, rebound_has_authority) {
        (true, true) | (false, false) => Ok(()),
        (false, true) => Err(
            "rebinding would give the thread financial authority it was not admitted with; it has no budget to charge",
        ),
        (true, false) => Err(
            "rebinding would remove the financial authority the thread was admitted with; its spend would no longer be accounted",
        ),
    }
}

/// Join a fresh launch preparation onto the prepared launch a source capsule
/// admitted, for a continuation that rebinds the ref bindings at `names`.
///
/// `runtime_facts` is the fact contract of the runtime descriptor sealed in
/// the source capsule; `fresh` is what launch preparation produced now from
/// the sealed primary, the stated bindings and current signed configuration.
///
/// Replaced from `fresh`: the binding record at each name, and the fields
/// preparation derives from the bound items and configuration (runtime data
/// and facts, required secrets, config contributors, financial and
/// external-effect authority).
///
/// Kept from `admitted`: everything else. The isolation, network and resource
/// ceilings are projected from the primary and narrowed by the parent at
/// first admission; evidence attachments and admitted sessions are settled
/// after preparation. None of them is derived from a binding, so the admitted
/// values stand.
///
/// Refused: a fresh preparation that disagrees with the admitted launch on a
/// field a continuation must keep (the project-result and target
/// requirements, execution, content and environment dependencies, or the
/// record of a binding that was not named), a changed external effect class
/// or a higher unknown-outcome retry ceiling, financial authority appearing
/// or disappearing, and a runtime fact below the minimum its contract signs
/// for a rebind.
pub(crate) fn join_rebound_prepared_launch(
    runtime_facts: &BTreeMap<String, RuntimeFactDecl>,
    admitted: &PreparedRuntimeLaunch,
    fresh: PreparedRuntimeLaunch,
    names: &BTreeSet<String>,
) -> Result<PreparedRuntimeLaunch, DispatchError> {
    if names.is_empty() {
        return Err(DispatchError::Internal(anyhow::anyhow!(
            "an operator rebind continuation names no ref binding"
        )));
    }
    // Exhaustive on purpose: a field added to the prepared launch does not
    // compile here until it is replaced, compared, or deliberately kept.
    let PreparedRuntimeLaunch {
        project_result_requirement,
        filesystem_authority_ceiling: _,
        network_authority_ceiling: _,
        target_requirement,
        resource_authority_ceiling: _,
        runtime_data,
        required_secrets,
        runtime_facts: rebound_facts,
        binding_records,
        execution_dependencies,
        content_dependencies,
        evidence_attachments: _,
        environment_contributions,
        admitted_sessions: _,
        config_contributors,
        financial_authority,
        external_effect_authority,
    } = fresh;

    let changed = |field: &str| {
        rebind_refusal(
            REBIND_CHANGES_SEALED_LAUNCH,
            format!(
                "rebinding would change the admitted launch's {field}, which a continuation must keep"
            ),
            names,
        )
    };
    if !same_value(
        &project_result_requirement,
        &admitted.project_result_requirement,
    )? {
        return Err(changed("project result requirement"));
    }
    if !same_value(&target_requirement, &admitted.target_requirement)? {
        return Err(changed("target requirement"));
    }
    if !same_value(&execution_dependencies, &admitted.execution_dependencies)? {
        return Err(changed("execution dependencies"));
    }
    if !same_value(&content_dependencies, &admitted.content_dependencies)? {
        return Err(changed("content dependencies"));
    }
    if !same_value(
        &environment_contributions,
        &admitted.environment_contributions,
    )? {
        return Err(changed("environment contributions"));
    }
    if binding_records.keys().ne(admitted.binding_records.keys()) {
        return Err(changed("set of ref bindings"));
    }
    for (name, record) in &binding_records {
        if names.contains(name) {
            continue;
        }
        if !same_value(&Some(record), &admitted.binding_records.get(name))? {
            return Err(changed(&format!("ref binding `{name}`")));
        }
    }
    for name in names {
        if !binding_records.contains_key(name) {
            return Err(rebind_refusal(
                REBIND_NOT_PERMITTED,
                format!("the admitted launch does not bind `{name}`"),
                names,
            ));
        }
    }

    if let Err(reason) = effect_authority_unchanged(
        admitted.external_effect_authority.as_ref(),
        external_effect_authority.as_ref(),
    ) {
        return Err(rebind_refusal(
            REBIND_CHANGES_EFFECT_AUTHORITY,
            reason,
            names,
        ));
    }

    if let Err(reason) = financial_authority_presence_held(
        admitted.financial_authority.is_some(),
        financial_authority.is_some(),
    ) {
        return Err(rebind_refusal(
            REBIND_CHANGES_FINANCIAL_AUTHORITY,
            reason,
            names,
        ));
    }

    for (fact, declaration) in runtime_facts {
        let Some(minimum) = declaration.rebind_minimum else {
            continue;
        };
        let value = rebound_facts.get(fact).and_then(Value::as_u64);
        if value.is_none_or(|value| value < minimum) {
            let stated = value
                .map(|value| value.to_string())
                .unwrap_or_else(|| "no value".to_owned());
            return Err(rebind_refusal(
                REBIND_FACT_BELOW_MINIMUM,
                format!(
                    "the rebound launch states {stated} for runtime fact `{fact}`; the runtime contract requires at least {minimum} to continue a thread"
                ),
                names,
            ));
        }
    }

    let mut rebound = admitted.clone();
    rebound.runtime_data = runtime_data;
    rebound.required_secrets = required_secrets;
    rebound.runtime_facts = rebound_facts;
    rebound.config_contributors = config_contributors;
    rebound.financial_authority = financial_authority;
    rebound.external_effect_authority = external_effect_authority;
    for name in names {
        if let Some(record) = binding_records.get(name) {
            rebound.binding_records.insert(name.clone(), record.clone());
        }
    }
    Ok(rebound)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::execution::launch_preparation::RefBindingLaunchRecord;
    use ryeos_effect_contract::EffectClass;
    use ryeos_engine::contracts::ItemSpace;
    use ryeos_engine::resolution::TrustClass;
    use ryeos_engine::runtime_registry::ProjectResultRequirement;

    fn record(canonical_ref: &str, digest: &str) -> RefBindingLaunchRecord {
        RefBindingLaunchRecord {
            canonical_ref: canonical_ref.to_owned(),
            source_space: ItemSpace::Project,
            effective_trust_class: TrustClass::TrustedProject,
            resolution: ryeos_engine::resolution::AsLaunchedResolutionDigest {
                root: ryeos_engine::resolution::ResolutionDigestNode {
                    requested_id: canonical_ref.to_owned(),
                    resolved_ref: canonical_ref.to_owned(),
                    source_space: ItemSpace::Project,
                    source_root: ryeos_engine::contracts::ItemSourceRoot::Search {
                        label: "fixture".to_owned(),
                    },
                    trust_class: TrustClass::TrustedProject,
                    signer_fingerprint: Some("b".repeat(64)),
                    raw_content_digest: digest.repeat(64),
                },
                ancestors: Vec::new(),
                referenced_items: Vec::new(),
                effective_trust_class: TrustClass::TrustedProject,
                policy_facts: Default::default(),
            },
        }
    }

    fn effect(
        class: Option<EffectClass>,
    ) -> ryeos_effect_contract::AdmittedExternalEffectAuthority {
        ryeos_effect_contract::AdmittedExternalEffectAuthority {
            authority_family: "fixture".to_owned(),
            admitted_effect_class: class,
            unknown_outcome_retry_ceiling: 0,
        }
    }

    /// A prepared launch that binds `model` and `guard`. `model` and `window`
    /// vary the bound item and the facts preparation derived from it.
    fn prepared(model: &str, window: u64) -> PreparedRuntimeLaunch {
        PreparedRuntimeLaunch {
            project_result_requirement: ProjectResultRequirement::None,
            filesystem_authority_ceiling:
                ryeos_engine::isolation::IsolationFilesystemAuthorityCeiling::NodePolicy,
            network_authority_ceiling:
                ryeos_engine::isolation::IsolationNetworkAuthorityCeiling::NodePolicy,
            target_requirement: None,
            resource_authority_ceiling:
                ryeos_engine::contracts::ExecutionResourceAuthorityCeiling::NodePolicy,
            runtime_data: BTreeMap::from([(
                "snapshot".to_owned(),
                serde_json::json!({"model": model}),
            )]),
            required_secrets: Vec::new(),
            runtime_facts: BTreeMap::from([
                ("model_name".to_owned(), serde_json::json!(model)),
                ("context_window".to_owned(), serde_json::json!(window)),
            ]),
            binding_records: BTreeMap::from([
                ("guard".to_owned(), record("directive:test/guard", "c")),
                ("model".to_owned(), record(model, "d")),
            ]),
            execution_dependencies: Default::default(),
            content_dependencies: Default::default(),
            evidence_attachments: Default::default(),
            environment_contributions: Default::default(),
            admitted_sessions: Default::default(),
            config_contributors: Vec::new(),
            financial_authority: None,
            external_effect_authority: Some(effect(None)),
        }
    }

    fn fact_contract(minimum: Option<u64>) -> BTreeMap<String, RuntimeFactDecl> {
        let mut window = serde_json::json!({"required": true, "kind": "integer", "max_bytes": 32});
        if let Some(minimum) = minimum {
            window["rebind_minimum"] = serde_json::json!(minimum);
        }
        BTreeMap::from([
            (
                "context_window".to_owned(),
                serde_json::from_value(window).unwrap(),
            ),
            (
                "model_name".to_owned(),
                serde_json::from_value(serde_json::json!({
                    "required": true,
                    "kind": "string",
                    "max_bytes": 256,
                    "rebind_record": true,
                }))
                .unwrap(),
            ),
        ])
    }

    fn names(names: &[&str]) -> BTreeSet<String> {
        names.iter().map(|name| (*name).to_owned()).collect()
    }

    fn refusal_code(error: DispatchError) -> String {
        match error {
            DispatchError::LaunchPolicyForbidden { code, .. } => code,
            other => panic!("expected a launch policy refusal, got {other:?}"),
        }
    }

    const FIRST: &str = "directive:test/models/first";
    const SECOND: &str = "directive:test/models/second";

    #[test]
    fn a_rebind_replaces_only_the_binding_derived_fields() {
        let mut admitted = prepared(FIRST, 200_000);
        // Values first admission settled after preparation; a fresh
        // preparation never carries them.
        admitted.filesystem_authority_ceiling =
            ryeos_engine::isolation::IsolationFilesystemAuthorityCeiling::CapturedExecution;
        admitted
            .admitted_sessions
            .insert("worker".to_owned(), "e".repeat(64));
        let fresh = prepared(SECOND, 128_000);
        let model = names(&["model"]);

        let rebound = join_rebound_prepared_launch(
            &fact_contract(Some(16_000)),
            &admitted,
            fresh.clone(),
            &model,
        )
        .unwrap();
        assert_eq!(rebound.binding_records["model"].canonical_ref, SECOND);
        assert_eq!(rebound.runtime_facts, fresh.runtime_facts);
        assert_eq!(rebound.runtime_data, fresh.runtime_data);
        assert_eq!(
            rebound.filesystem_authority_ceiling,
            admitted.filesystem_authority_ceiling
        );
        assert_eq!(rebound.admitted_sessions, admitted.admitted_sessions);

        // The joined launch differs from the admitted one in exactly the
        // places the capsule comparison lets a rebound successor differ.
        let admitted_value = serde_json::to_value(&admitted).unwrap();
        let rebound_value = serde_json::to_value(&rebound).unwrap();
        let differing: BTreeSet<&str> = admitted_value
            .as_object()
            .unwrap()
            .iter()
            .filter(|(field, value)| rebound_value.get(field.as_str()) != Some(*value))
            .map(|(field, _)| field.as_str())
            .collect();
        let replaceable: BTreeSet<&str> = ryeos_state::objects::REBOUND_PREPARED_LAUNCH_FIELDS
            .iter()
            .copied()
            .chain(std::iter::once("binding_records"))
            .collect();
        assert!(differing.is_subset(&replaceable), "{differing:?}");
        assert!(differing.contains("binding_records") && differing.contains("runtime_facts"));
        assert_eq!(
            admitted_value["binding_records"]["guard"],
            rebound_value["binding_records"]["guard"]
        );

        let audit = ReboundLaunch::between(
            "T-source",
            &fact_contract(Some(16_000)),
            &admitted,
            &rebound,
            &model,
        )
        .unwrap();
        assert_eq!(
            audit.changes,
            vec![ReboundBinding {
                name: "model".to_owned(),
                from: FIRST.to_owned(),
                to: SECOND.to_owned(),
            }]
        );
        // Only the facts the contract marks for the record are carried.
        assert_eq!(
            audit.from_facts,
            BTreeMap::from([("model_name".to_owned(), serde_json::json!(FIRST))])
        );
        assert_eq!(
            audit.to_facts,
            BTreeMap::from([("model_name".to_owned(), serde_json::json!(SECOND))])
        );
        assert!(admitted.runtime_facts.contains_key("context_window"));
    }

    #[test]
    fn a_rebind_refuses_any_change_to_a_field_a_continuation_keeps() {
        let admitted = prepared(FIRST, 200_000);
        let model = names(&["model"]);
        let contract = fact_contract(None);
        let refused = |fresh: PreparedRuntimeLaunch| {
            refusal_code(
                join_rebound_prepared_launch(&contract, &admitted, fresh, &model).unwrap_err(),
            )
        };

        // A dependency the fresh preparation adds or drops. The execution,
        // content and environment dependency maps are each compared whole in
        // the same way, which is what refuses a change into or out of a
        // binding whose provider runs a local worker.
        let contribution =
            || crate::execution::launch_preparation::PreparedEnvironmentContribution {
                targets: vec!["worker".to_owned()],
                variables: BTreeMap::new(),
            };
        let mut gained = prepared(SECOND, 200_000);
        gained
            .environment_contributions
            .insert("toolchain".to_owned(), contribution());
        assert_eq!(refused(gained), REBIND_CHANGES_SEALED_LAUNCH);
        let mut source_with_dependency = admitted.clone();
        source_with_dependency
            .environment_contributions
            .insert("toolchain".to_owned(), contribution());
        assert_eq!(
            refusal_code(
                join_rebound_prepared_launch(
                    &contract,
                    &source_with_dependency,
                    prepared(SECOND, 200_000),
                    &model
                )
                .unwrap_err()
            ),
            REBIND_CHANGES_SEALED_LAUNCH
        );

        let mut other_requirement = prepared(SECOND, 200_000);
        other_requirement.project_result_requirement = ProjectResultRequirement::RetainedGeneration;
        assert_eq!(refused(other_requirement), REBIND_CHANGES_SEALED_LAUNCH);

        // A binding that was not named cannot move with the rebind.
        let mut other_binding = prepared(SECOND, 200_000);
        other_binding.binding_records.insert(
            "guard".to_owned(),
            record("directive:test/other-guard", "c"),
        );
        assert_eq!(refused(other_binding), REBIND_CHANGES_SEALED_LAUNCH);
        let mut edited_binding = prepared(SECOND, 200_000);
        edited_binding
            .binding_records
            .insert("guard".to_owned(), record("directive:test/guard", "f"));
        assert_eq!(refused(edited_binding), REBIND_CHANGES_SEALED_LAUNCH);
        let mut extra_binding = prepared(SECOND, 200_000);
        extra_binding
            .binding_records
            .insert("extra".to_owned(), record("directive:test/extra", "c"));
        assert_eq!(refused(extra_binding), REBIND_CHANGES_SEALED_LAUNCH);
        let mut dropped_binding = prepared(SECOND, 200_000);
        dropped_binding.binding_records.remove("guard");
        assert_eq!(refused(dropped_binding), REBIND_CHANGES_SEALED_LAUNCH);

        // Naming nothing is not a rebind.
        assert!(
            join_rebound_prepared_launch(
                &contract,
                &admitted,
                prepared(SECOND, 200_000),
                &names(&[])
            )
            .is_err()
        );
    }

    #[test]
    fn a_rebind_keeps_the_effect_class_and_cannot_raise_the_retry_ceiling() {
        use EffectClass::{Recorded, Sealed};
        let authority = |class: Option<EffectClass>, ceiling: u32, family: &str| {
            ryeos_effect_contract::AdmittedExternalEffectAuthority {
                authority_family: family.to_owned(),
                admitted_effect_class: class,
                unknown_outcome_retry_ceiling: ceiling,
            }
        };
        // The class must stay exactly the source's: not wider, and not
        // dropped, since no class means provider calls are not recorded.
        for (source, rebound, admitted) in [
            (None, None, true),
            (Some(Recorded), Some(Recorded), true),
            (Some(Sealed), Some(Sealed), true),
            (Some(Recorded), None, false),
            (Some(Sealed), None, false),
            (Some(Sealed), Some(Recorded), false),
            (None, Some(Recorded), false),
            (None, Some(Sealed), false),
            (Some(Recorded), Some(Sealed), false),
        ] {
            assert_eq!(
                effect_authority_unchanged(
                    Some(&authority(source, 1, "fixture")),
                    Some(&authority(rebound, 1, "fixture")),
                )
                .is_ok(),
                admitted,
                "{source:?} -> {rebound:?}"
            );
        }
        // The retry ceiling may stay or fall, never rise.
        for (source, rebound, admitted) in
            [(2, 2, true), (2, 0, true), (0, 1, false), (2, 3, false)]
        {
            assert_eq!(
                effect_authority_unchanged(
                    Some(&authority(Some(Recorded), source, "fixture")),
                    Some(&authority(Some(Recorded), rebound, "fixture")),
                )
                .is_ok(),
                admitted,
                "{source} -> {rebound}"
            );
        }
        // The family follows the bound provider and is not compared.
        assert!(
            effect_authority_unchanged(
                Some(&authority(None, 0, "first_family")),
                Some(&authority(None, 0, "second_family")),
            )
            .is_ok()
        );
        // The authority itself cannot appear or disappear.
        assert!(effect_authority_unchanged(None, None).is_ok());
        assert!(effect_authority_unchanged(None, Some(&authority(None, 0, "fixture"))).is_err());
        assert!(effect_authority_unchanged(Some(&authority(None, 0, "fixture")), None).is_err());

        let model = names(&["model"]);
        let mut admitted = prepared(FIRST, 200_000);
        admitted.external_effect_authority = Some(authority(Some(Recorded), 1, "fixture"));
        for refused in [
            authority(Some(Sealed), 1, "fixture"),
            authority(None, 1, "fixture"),
            authority(Some(Recorded), 2, "fixture"),
        ] {
            let mut fresh = prepared(SECOND, 200_000);
            fresh.external_effect_authority = Some(refused);
            assert_eq!(
                refusal_code(
                    join_rebound_prepared_launch(&fact_contract(None), &admitted, fresh, &model)
                        .unwrap_err()
                ),
                REBIND_CHANGES_EFFECT_AUTHORITY
            );
        }
        let mut same = prepared(SECOND, 200_000);
        same.external_effect_authority = Some(authority(Some(Recorded), 0, "other_family"));
        let rebound =
            join_rebound_prepared_launch(&fact_contract(None), &admitted, same, &model).unwrap();
        assert_eq!(
            rebound.external_effect_authority,
            Some(authority(Some(Recorded), 0, "other_family"))
        );
    }

    #[test]
    fn a_rebind_neither_adds_nor_removes_financial_authority() {
        assert!(financial_authority_presence_held(true, true).is_ok());
        assert!(financial_authority_presence_held(false, false).is_ok());
        assert!(financial_authority_presence_held(false, true).is_err());
        assert!(financial_authority_presence_held(true, false).is_err());

        let authority = |digest: &str| ryeos_accounting::AdmittedFinancialAuthority {
            kind: "accounting".to_owned(),
            authority: serde_json::json!({"tariff": digest}),
            authority_digest: digest.repeat(64),
            spend_bound: ryeos_accounting::SpendBoundClass::Paid,
        };
        let model = names(&["model"]);
        let join = |source: Option<&str>, rebound: Option<&str>| {
            let mut admitted = prepared(FIRST, 200_000);
            admitted.financial_authority = source.map(authority);
            let mut fresh = prepared(SECOND, 200_000);
            fresh.financial_authority = rebound.map(authority);
            join_rebound_prepared_launch(&fact_contract(None), &admitted, fresh, &model)
        };

        // A source with no financial authority minted no item budget: a
        // rebound launch that gained one would have nothing to reserve against.
        assert_eq!(
            refusal_code(join(None, Some("a")).unwrap_err()),
            REBIND_CHANGES_FINANCIAL_AUTHORITY
        );
        // A rebound launch that lost it would run unaccounted.
        assert_eq!(
            refusal_code(join(Some("a"), None).unwrap_err()),
            REBIND_CHANGES_FINANCIAL_AUTHORITY
        );
        // Which authority it is follows the bound provider.
        let rebound = join(Some("a"), Some("b")).unwrap();
        assert_eq!(
            rebound
                .financial_authority
                .map(|authority| authority.authority_digest),
            Some("b".repeat(64))
        );
        assert!(join(None, None).unwrap().financial_authority.is_none());
    }

    #[test]
    fn a_rebind_requires_the_sealed_runtime_to_be_the_installed_one() {
        let model = names(&["model"]);
        let sealed = "1".repeat(64);
        require_sealed_runtime_installed("runtime:test/fixture", &sealed, Some(&sealed), &model)
            .unwrap();
        // An upgraded runtime under the same ref, or none at all.
        for installed in [Some("2".repeat(64)), None] {
            let error = require_sealed_runtime_installed(
                "runtime:test/fixture",
                &sealed,
                installed.as_deref(),
                &model,
            )
            .unwrap_err();
            let message = error.to_string();
            assert_eq!(refusal_code(error), REBIND_RUNTIME_NOT_INSTALLED);
            assert!(message.contains("no longer installed"), "{message}");
            assert!(message.contains("start a new conversation"), "{message}");
        }
    }

    #[test]
    fn only_a_new_successor_of_a_local_source_is_ever_prepared_again() {
        // The rule looks only at which capsules exist, not inside them.
        let capsule = "capsule";
        let own = Some(&capsule);
        // The first preparation of a successor: nothing rooted for it yet,
        // nothing transferred, the source's capsule inherited.
        assert!(rebind_source_capsule(None, None, own, true).is_ok());
        // A row that already has its own capsule, such as a stranded
        // successor being re-driven, is never prepared again.
        assert!(rebind_source_capsule(own, None, None, true).is_err());
        assert!(rebind_source_capsule(own, None, own, true).is_err());
        // Nor is a capsule transferred from another site, a launch with no
        // source, or one with no continuation seed.
        assert!(rebind_source_capsule(None, own, None, true).is_err());
        assert!(rebind_source_capsule::<&str>(None, None, None, true).is_err());
        assert!(rebind_source_capsule(None, None, own, false).is_err());
    }

    #[test]
    fn a_rebind_meets_the_signed_minimum_of_each_runtime_fact() {
        let admitted = prepared(FIRST, 200_000);
        let model = names(&["model"]);
        let join = |minimum: Option<u64>, fresh: PreparedRuntimeLaunch| {
            join_rebound_prepared_launch(&fact_contract(minimum), &admitted, fresh, &model)
        };

        assert!(join(Some(16_000), prepared(SECOND, 16_000)).is_ok());
        assert_eq!(
            refusal_code(join(Some(16_000), prepared(SECOND, 15_999)).unwrap_err()),
            REBIND_FACT_BELOW_MINIMUM
        );
        // A contract that signs no minimum admits any value.
        assert!(join(None, prepared(SECOND, 500)).is_ok());
        // A fact the preparation did not state cannot meet a minimum.
        let mut unstated = prepared(SECOND, 200_000);
        unstated.runtime_facts.remove("context_window");
        assert_eq!(
            refusal_code(join(Some(16_000), unstated).unwrap_err()),
            REBIND_FACT_BELOW_MINIMUM
        );
        let mut mistyped = prepared(SECOND, 200_000);
        mistyped
            .runtime_facts
            .insert("context_window".to_owned(), serde_json::json!("large"));
        assert_eq!(
            refusal_code(join(Some(16_000), mistyped).unwrap_err()),
            REBIND_FACT_BELOW_MINIMUM
        );
    }

    #[test]
    fn the_admitted_executor_identity_follows_the_admitted_artifacts() {
        let identity = |executor_hash: &str| {
            ryeos_state::objects::AdmittedLaunchArtifactIdentity::ManagedRuntime {
                runtime_ref: "runtime:test/fixture".to_owned(),
                runtime_content_hash: "1".repeat(64),
                runtime_signer_fingerprint: "2".repeat(64),
                protocol_ref: "protocol:test/fixture".to_owned(),
                protocol_content_hash: "3".repeat(64),
                protocol_signer_fingerprint: "4".repeat(64),
                executor_ref: "native:fixture".to_owned(),
                executor_content_hash: executor_hash.repeat(64),
                executor_bundle_manifest_hash: "6".repeat(64),
                executor_bundle_signer_fingerprint: "7".repeat(64),
            }
        };
        let first = admitted_executor_chain_identity(&identity("5")).unwrap();
        assert_eq!(
            first,
            admitted_executor_chain_identity(&identity("5")).unwrap()
        );
        assert_ne!(
            first,
            admitted_executor_chain_identity(&identity("8")).unwrap()
        );
    }
}

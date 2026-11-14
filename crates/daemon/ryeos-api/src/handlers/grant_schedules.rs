//! What a change to an authorized-key grant does to the schedules pinned to
//! it.
//!
//! A schedule pins the exact grant generation (the digest of the signed grant
//! file) it was admitted under. Two grant operations reach schedules in the
//! same operation, not at the next fire:
//!
//! - Renewal supersedes the generation. Every schedule pinned to the
//!   superseded generation whose declared capabilities the renewed grant
//!   still covers is re-sealed to the new generation, so renewing a grant
//!   never pauses work it still authorizes. A schedule the renewed grant no
//!   longer covers is paused with `authority_narrowed`. Nothing is revived:
//!   only the superseded generation's own pin is moved.
//! - Revocation pauses every enabled schedule pinned to a revoked generation
//!   with `authority_revoked`. The pin keeps naming the tombstoned digest, so
//!   a new grant for the same key (a different digest) does not revive the
//!   schedule: the owner re-registers it or re-seals it with
//!   `scheduler.reauthorize`.
//!
//! Both read every schedule under the scheduler mutation gate; schedules are
//! few and are not indexed by grant digest.

use serde::Serialize;

/// A schedule the operation paused, or found already paused.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PausedSchedule {
    pub schedule_id: String,
    /// The pause reason now recorded on the schedule.
    pub reason: &'static str,
    /// The schedule was paused before this operation and stays as it was.
    pub already_paused: bool,
}

/// The schedule changes one grant operation made.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct ScheduleAuthorityChanges {
    /// Schedules re-sealed to the renewed generation.
    pub resealed: Vec<String>,
    /// Schedules paused for want of authority.
    pub paused: Vec<PausedSchedule>,
}

#[cfg(feature = "scheduling")]
pub(crate) use enabled::{pause_for_revocation, reseal_for_renewal};

#[cfg(not(feature = "scheduling"))]
pub(crate) async fn reseal_for_renewal(
    _state: &ryeos_app::state::AppState,
    _renewal: &Renewal<'_>,
) -> anyhow::Result<ScheduleAuthorityChanges> {
    Ok(ScheduleAuthorityChanges::default())
}

#[cfg(not(feature = "scheduling"))]
pub(crate) async fn pause_for_revocation(
    _state: &ryeos_app::state::AppState,
    _revoked_grant_digests: &[String],
) -> anyhow::Result<ScheduleAuthorityChanges> {
    Ok(ScheduleAuthorityChanges::default())
}

/// The renewal a re-seal follows.
pub(crate) struct Renewal<'a> {
    /// Fingerprint of the renewed key.
    pub fingerprint: &'a str,
    /// Digest of the superseded generation.
    pub previous_digest: &'a str,
    /// Digest of the renewed generation.
    pub renewed_digest: &'a str,
    /// Scopes of the renewed grant.
    pub scopes: &'a [String],
}

#[cfg(feature = "scheduling")]
mod enabled {
    use anyhow::Result;
    use ryeos_app::state::AppState;
    use ryeos_runtime::authorizer::{AuthorizationPolicy, Authorizer};
    use ryeos_scheduler::types::ScheduleSpecRecord;
    use ryeos_scheduler_contract::{ScheduleAutoPauseReason, ScheduleExecutionAuthority};

    use super::{PausedSchedule, Renewal, ScheduleAuthorityChanges};
    use ryeos_node_runtime::identity::FORWARDED_OPERATOR_ATTESTATION_SCOPE;

    /// One write the scheduler must make.
    #[derive(Debug, PartialEq)]
    pub(super) enum Change {
        Reseal {
            schedule_id: String,
            authority: ScheduleExecutionAuthority,
        },
        Pause {
            schedule_id: String,
            reason: ScheduleAutoPauseReason,
            already_paused: bool,
        },
    }

    fn pins_principal_generation(
        spec: &ScheduleSpecRecord,
        fingerprint: &str,
        digest: &str,
    ) -> bool {
        matches!(
            &spec.execution.authority,
            ScheduleExecutionAuthority::Authenticated { principal_id, grant_authority, .. }
                if principal_id.strip_prefix("fp:") == Some(fingerprint)
                    && grant_authority.principal_grant_hash == digest
        )
    }

    fn pins_forwarding_generation(
        spec: &ScheduleSpecRecord,
        fingerprint: &str,
        digest: &str,
    ) -> bool {
        matches!(
            &spec.execution.authority,
            ScheduleExecutionAuthority::Authenticated { grant_authority, .. }
                if grant_authority.forwarding.as_ref().is_some_and(|forwarding| {
                    forwarding.source_node_fingerprint == fingerprint
                        && forwarding.source_node_grant_hash == digest
                })
        )
    }

    /// The writes a renewal calls for. Pure: it only reads `specs`.
    pub(super) fn plan_renewal(
        specs: &[ScheduleSpecRecord],
        renewal: &Renewal<'_>,
        authorizer: &Authorizer,
    ) -> Vec<Change> {
        let covers = |capability: &str| {
            authorizer
                .authorize(renewal.scopes, &AuthorizationPolicy::require(capability))
                .is_ok()
        };
        let mut changes = Vec::new();
        for spec in specs {
            let principal_pinned =
                pins_principal_generation(spec, renewal.fingerprint, renewal.previous_digest);
            let forwarding_pinned =
                pins_forwarding_generation(spec, renewal.fingerprint, renewal.previous_digest);
            if !principal_pinned && !forwarding_pinned {
                continue;
            }
            let covered = (!principal_pinned
                || spec.execution.capabilities.iter().all(|cap| covers(cap)))
                && (!forwarding_pinned || covers(FORWARDED_OPERATOR_ATTESTATION_SCOPE));
            if !covered {
                changes.push(Change::Pause {
                    schedule_id: spec.schedule_id.clone(),
                    reason: ScheduleAutoPauseReason::AuthorityNarrowed,
                    already_paused: !spec.enabled,
                });
                continue;
            }
            let ScheduleExecutionAuthority::Authenticated {
                principal_id,
                principal_class,
                effective_origin_site_id,
                grant_authority,
                registration_request_hash,
            } = &spec.execution.authority
            else {
                continue;
            };
            let mut grant_authority = grant_authority.clone();
            if principal_pinned {
                grant_authority.principal_grant_hash = renewal.renewed_digest.to_owned();
            }
            if forwarding_pinned && let Some(forwarding) = grant_authority.forwarding.as_mut() {
                forwarding.source_node_grant_hash = renewal.renewed_digest.to_owned();
            }
            changes.push(Change::Reseal {
                schedule_id: spec.schedule_id.clone(),
                authority: ScheduleExecutionAuthority::Authenticated {
                    principal_id: principal_id.clone(),
                    principal_class: *principal_class,
                    effective_origin_site_id: effective_origin_site_id.clone(),
                    grant_authority,
                    registration_request_hash: registration_request_hash.clone(),
                },
            });
        }
        changes
    }

    /// The writes a revocation calls for: a pause for every schedule pinned
    /// to a revoked generation, as principal grant or forwarding grant.
    pub(super) fn plan_revocation(specs: &[ScheduleSpecRecord], revoked: &[String]) -> Vec<Change> {
        specs
            .iter()
            .filter_map(|spec| {
                let ScheduleExecutionAuthority::Authenticated {
                    grant_authority, ..
                } = &spec.execution.authority
                else {
                    return None;
                };
                let pinned = revoked.contains(&grant_authority.principal_grant_hash)
                    || grant_authority
                        .forwarding
                        .as_ref()
                        .is_some_and(|forwarding| {
                            revoked.contains(&forwarding.source_node_grant_hash)
                        });
                pinned.then(|| Change::Pause {
                    schedule_id: spec.schedule_id.clone(),
                    reason: ScheduleAutoPauseReason::AuthorityRevoked,
                    already_paused: !spec.enabled,
                })
            })
            .collect()
    }

    /// Apply `changes` under the scheduler mutation gate the caller holds.
    fn apply(
        scheduler: &ryeos_scheduler_service::SchedulerService,
        changes: Vec<Change>,
    ) -> Result<ScheduleAuthorityChanges> {
        let db = scheduler.scheduler_db();
        let mut done = ScheduleAuthorityChanges::default();
        for change in changes {
            match change {
                Change::Reseal {
                    schedule_id,
                    authority,
                } => {
                    let Some(spec) = db.get_spec(&schedule_id)? else {
                        continue;
                    };
                    let rec = super::super::scheduler_register::rewrite_schedule_authority(
                        scheduler, &spec, authority,
                    )?;
                    db.upsert_spec(&rec)?;
                    scheduler.notify_reload(Some(schedule_id.clone()));
                    done.resealed.push(schedule_id);
                }
                Change::Pause {
                    schedule_id,
                    reason,
                    already_paused,
                } => {
                    if !already_paused {
                        let Some(spec) = db.get_spec(&schedule_id)? else {
                            continue;
                        };
                        let rec = super::super::scheduler_register::rewrite_schedule_enabled(
                            scheduler,
                            &spec,
                            false,
                            Some(reason),
                        )?;
                        db.upsert_spec(&rec)?;
                        scheduler.notify_reload(Some(schedule_id.clone()));
                    }
                    done.paused.push(PausedSchedule {
                        schedule_id,
                        reason: reason.as_str(),
                        already_paused,
                    });
                }
            }
        }
        Ok(done)
    }

    /// The scheduler, when this node runs one.
    fn scheduler(
        state: &AppState,
    ) -> Option<std::sync::Arc<ryeos_scheduler_service::SchedulerService>> {
        state
            .extensions
            .get::<ryeos_scheduler_service::SchedulerService>()
    }

    /// Re-seal or pause every schedule pinned to the generation `renewal`
    /// superseded.
    pub(crate) async fn reseal_for_renewal(
        state: &AppState,
        renewal: &Renewal<'_>,
    ) -> Result<ScheduleAuthorityChanges> {
        let Some(scheduler) = scheduler(state) else {
            return Ok(ScheduleAuthorityChanges::default());
        };
        let _guard = scheduler.acquire_mutation().await?;
        let specs = scheduler.scheduler_db().list_specs(false, None)?;
        let changes = plan_renewal(&specs, renewal, &state.authorizer);
        apply(&scheduler, changes)
    }

    /// Pause every enabled schedule pinned to a revoked generation.
    pub(crate) async fn pause_for_revocation(
        state: &AppState,
        revoked_grant_digests: &[String],
    ) -> Result<ScheduleAuthorityChanges> {
        let Some(scheduler) = scheduler(state) else {
            return Ok(ScheduleAuthorityChanges::default());
        };
        let _guard = scheduler.acquire_mutation().await?;
        let specs = scheduler.scheduler_db().list_specs(false, None)?;
        let changes = plan_revocation(&specs, revoked_grant_digests);
        apply(&scheduler, changes)
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use ryeos_engine::execution_contract::{ExecutionPolicy, ExecutionResponse};
        use ryeos_engine::principal_contract::{
            AuthenticatedGrantAuthority, AuthorizedKeyPrincipalClass,
        };
        use ryeos_scheduler_contract::ScheduleExecution;

        const OWNER: &str = "ab";
        const OWNER_FP: &str = "abababababababababababababababababababababababababababababababab";
        const CAP: &str = "ryeos.execute.tool.test/job";

        fn digest(seed: &str) -> String {
            seed.repeat(32)
        }

        fn spec(
            id: &str,
            pinned: &str,
            capabilities: &[&str],
            enabled: bool,
        ) -> ScheduleSpecRecord {
            ScheduleSpecRecord {
                schedule_id: id.to_owned(),
                item_ref: "directive:test/job".to_owned(),
                ref_bindings: Default::default(),
                params: "{}".to_owned(),
                schedule_type: "cron".to_owned(),
                expression: "* * * * * *".to_owned(),
                timezone: "UTC".to_owned(),
                misfire_policy: "skip".to_owned(),
                overlap_policy: "skip".to_owned(),
                lateness_grace_secs: 60,
                enabled,
                project_root: None,
                signer_fingerprint: digest("11"),
                spec_hash: digest("22"),
                registered_at: 0,
                execution: ScheduleExecution {
                    authority: ScheduleExecutionAuthority::Authenticated {
                        principal_id: format!("fp:{}", digest(OWNER)),
                        principal_class: AuthorizedKeyPrincipalClass::LocalClient,
                        effective_origin_site_id: "site:test".to_owned(),
                        grant_authority: AuthenticatedGrantAuthority {
                            principal_grant_hash: pinned.to_owned(),
                            forwarding: None,
                        },
                        registration_request_hash: digest("88"),
                    },
                    capabilities: capabilities.iter().map(|c| (*c).to_owned()).collect(),
                    policy: ExecutionPolicy::projectless(ExecutionResponse::Accepted),
                    limits: None,
                    max_fires: None,
                    expires_at: None,
                },
            }
        }

        fn renewal<'a>(scopes: &'a [String], previous: &'a str, renewed: &'a str) -> Renewal<'a> {
            Renewal {
                fingerprint: OWNER_FP,
                previous_digest: previous,
                renewed_digest: renewed,
                scopes,
            }
        }

        #[test]
        fn renew_reseals_covered_schedules() {
            let old = digest("c1");
            let new = digest("c2");
            let scopes = vec![CAP.to_owned()];
            let specs = vec![
                spec("covered", &old, &[CAP], true),
                spec("paused-by-owner", &old, &[CAP], false),
                spec("other-generation", &digest("c3"), &[CAP], true),
            ];
            let changes = plan_renewal(&specs, &renewal(&scopes, &old, &new), &Authorizer::new());
            let sealed: Vec<(&str, &str)> = changes
                .iter()
                .map(|change| match change {
                    Change::Reseal {
                        schedule_id,
                        authority:
                            ScheduleExecutionAuthority::Authenticated {
                                grant_authority, ..
                            },
                    } => (
                        schedule_id.as_str(),
                        grant_authority.principal_grant_hash.as_str(),
                    ),
                    other => panic!("expected a re-seal, got {other:?}"),
                })
                .collect();
            assert_eq!(
                sealed,
                vec![("covered", new.as_str()), ("paused-by-owner", new.as_str())],
                "covered schedules move to the renewed generation, paused or not; \
                 a schedule pinned to another generation is untouched"
            );
        }

        #[test]
        fn renew_pauses_schedules_no_longer_covered() {
            let old = digest("c1");
            let new = digest("c2");
            let scopes = vec![CAP.to_owned()];
            let specs = vec![
                spec(
                    "narrowed",
                    &old,
                    &[CAP, "ryeos.execute.tool.test/other"],
                    true,
                ),
                spec(
                    "already-paused",
                    &old,
                    &["ryeos.execute.tool.test/other"],
                    false,
                ),
            ];
            let changes = plan_renewal(&specs, &renewal(&scopes, &old, &new), &Authorizer::new());
            assert_eq!(
                changes,
                vec![
                    Change::Pause {
                        schedule_id: "narrowed".to_owned(),
                        reason: ScheduleAutoPauseReason::AuthorityNarrowed,
                        already_paused: false,
                    },
                    Change::Pause {
                        schedule_id: "already-paused".to_owned(),
                        reason: ScheduleAutoPauseReason::AuthorityNarrowed,
                        already_paused: true,
                    },
                ]
            );
        }

        #[test]
        fn revoke_then_regrant_does_not_revive_schedule() {
            let revoked = digest("c1");
            let regranted = digest("c2");
            let renewed = digest("c3");
            let scopes = vec![CAP.to_owned()];
            let specs = vec![spec("victim", &revoked, &[CAP], true)];
            // Revocation pauses it.
            assert_eq!(
                plan_revocation(&specs, std::slice::from_ref(&revoked)),
                vec![Change::Pause {
                    schedule_id: "victim".to_owned(),
                    reason: ScheduleAutoPauseReason::AuthorityRevoked,
                    already_paused: false,
                }]
            );
            // A new grant for the same key is a new generation, and renewing
            // that generation moves only schedules pinned to it: the victim
            // still names the tombstoned digest.
            let changes = plan_renewal(
                &specs,
                &renewal(&scopes, &regranted, &renewed),
                &Authorizer::new(),
            );
            assert!(changes.is_empty(), "{changes:?}");
        }

        #[test]
        fn forwarding_pin_follows_the_renewed_source_node_grant() {
            let old = digest("c1");
            let new = digest("c2");
            let source = digest("d1");
            let mut spec = spec("forwarded", &digest("e1"), &[CAP], true);
            if let ScheduleExecutionAuthority::Authenticated {
                principal_class,
                grant_authority,
                ..
            } = &mut spec.execution.authority
            {
                *principal_class = AuthorizedKeyPrincipalClass::RemotePrincipal;
                grant_authority.forwarding = Some(
                    ryeos_engine::principal_contract::ForwardingAuthorityEvidence {
                        source_node_fingerprint: source.clone(),
                        source_node_grant_hash: old.clone(),
                        delegation_digest: None,
                    },
                );
            }
            let scopes = vec![FORWARDED_OPERATOR_ATTESTATION_SCOPE.to_owned()];
            let renewal = Renewal {
                fingerprint: &source,
                previous_digest: &old,
                renewed_digest: &new,
                scopes: &scopes,
            };
            let changes = plan_renewal(&[spec], &renewal, &Authorizer::new());
            let [Change::Reseal { authority, .. }] = changes.as_slice() else {
                panic!("expected one re-seal, got {changes:?}");
            };
            let ScheduleExecutionAuthority::Authenticated {
                grant_authority, ..
            } = authority
            else {
                panic!("authenticated");
            };
            assert_eq!(grant_authority.principal_grant_hash, digest("e1"));
            assert_eq!(
                grant_authority
                    .forwarding
                    .as_ref()
                    .unwrap()
                    .source_node_grant_hash,
                new
            );
        }
    }
}

//! `ui.ryeos.dimension.get` — bounded read-only RyeOS UI Dimension projection.
//!
//! Aggregates daemon health, node identity, project context, spaces/bundles,
//! remotes (configured only, no probes), threads, schedules, GC state, and
//! service catalog into a single JSON response. Designed to render the
//! RyeOS UI Home/Workbench without any graph interaction.
//!
//! Read-only. No remote probes. No pseudo item kinds.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use serde::Serialize;
use serde_json::Value;

use ryeos_api::registry::ServiceDescriptor;
use ryeos_app::handler_context::HandlerContext;
use ryeos_app::state::AppState;
use ryeos_executor::executor::ServiceAvailability;

// ── Response view-model ─────────────────────────────────────────────

#[derive(Debug, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RyeOsDimensionProjection {
    pub schema_version: &'static str,
    pub generated_at: String,
    pub session: SessionInfo,
    pub local_node: LocalNode,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project: Option<ProjectInfo>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub remotes: Vec<RemoteSummary>,
    pub invalid_remotes: Vec<InvalidRemoteSummary>,
    pub threads: ThreadSummary,
    pub schedules: ScheduleSummary,
    pub gc: GcSummary,
}

#[derive(Debug, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SessionInfo {
    pub session_id: String,
    pub root_ref: String,
    pub title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user_principal_id: Option<String>,
    pub binding_digest: String,
    pub posture: crate::compiled_binding::EffectiveUiPosture,
}

#[derive(Debug, Serialize)]
#[serde(deny_unknown_fields)]
pub struct LocalNode {
    pub identity: IdentityInfo,
    pub status: Value,
    pub health: Value,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub spaces: Vec<SpaceSummary>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub bundles: Vec<BundleSummary>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub services: Vec<ServiceSummary>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub commands: Vec<CommandSummary>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub command_aliases: Vec<CommandSummary>,
}

#[derive(Debug, Serialize)]
#[serde(deny_unknown_fields)]
pub struct IdentityInfo {
    pub principal_id: String,
    pub fingerprint: String,
}

#[derive(Debug, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SpaceSummary {
    pub space: String,
    pub label: String,
    pub path: String,
}

#[derive(Debug, Serialize)]
#[serde(deny_unknown_fields)]
pub struct BundleSummary {
    pub name: String,
    pub path: String,
}

#[derive(Debug, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ServiceSummary {
    pub endpoint: String,
    pub service_ref: String,
    pub availability: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub required_caps: Vec<String>,
}

#[derive(Debug, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CommandSummary {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectInfo {
    pub path: String,
}

#[derive(Debug, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RemoteSummary {
    pub name: String,
    pub url: String,
    pub principal_id: String,
}

#[derive(Debug, Serialize)]
#[serde(deny_unknown_fields)]
pub struct InvalidRemoteSummary {
    pub name: String,
    pub scope: &'static str,
    pub error: String,
    pub repair_hint: String,
}

#[derive(Debug, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ThreadSummary {
    pub active_count: i64,
}

#[derive(Debug, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ScheduleSummary {
    pub identity_filtered: bool,
    pub owner_principal_id: String,
    pub coverage: &'static str,
    pub total: usize,
    pub enabled: usize,
}

#[derive(Debug, Serialize)]
#[serde(deny_unknown_fields)]
pub struct GcSummary {
    pub activity: &'static str,
    pub coverage: &'static str,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub recent_events: Vec<Value>,
}

// ── Handler ────────────────────────────────────────────────────────

pub async fn handle(_params: Value, ctx: HandlerContext, state: Arc<AppState>) -> Result<Value> {
    let (session, attachment) = crate::seat_auth::require_compiled_ui_session(&ctx, &state)?;

    let project_access = crate::seat_auth::attachment_project_access(&attachment)?;
    let project_path = project_access
        .as_ref()
        .map(|access| access.path().to_path_buf());
    let project_identity = crate::seat_auth::attachment_project_query_identity(&attachment)?;

    let projection = build_dimension_projection(
        &state,
        &session,
        &attachment,
        project_path.as_ref(),
        project_identity.as_ref(),
    )?;

    serde_json::to_value(projection).map_err(Into::into)
}

fn build_dimension_projection(
    state: &AppState,
    session: &crate::browser_session::BrowserSession,
    attachment: &crate::browser_session::AdmittedBindingAttachment,
    project_path: Option<&PathBuf>,
    project_identity: Option<&PathBuf>,
) -> Result<RyeOsDimensionProjection> {
    // ── Identity ──
    let identity = IdentityInfo {
        principal_id: state.identity.principal_id(),
        fingerprint: state.identity.fingerprint().to_string(),
    };

    // ── Status + Health (reuse existing logic inline) ──
    let status = serde_json::to_value(state.status()?)?;
    let health_status = if state.catalog_health.missing_services.is_empty() {
        "healthy"
    } else {
        "degraded"
    };
    let health = serde_json::json!({
        "status": health_status,
        "operational_services": state.catalog_health.status,
        "missing_services": state.catalog_health.missing_services,
    });

    // ── Spaces (from resolution roots) ──
    let roots = state.engine.resolution_roots(project_path.cloned());
    let spaces: Vec<SpaceSummary> = roots
        .ordered
        .iter()
        .map(|r| SpaceSummary {
            space: match r.space {
                ryeos_engine::contracts::ItemSpace::Bundle => "system".to_string(),
                ryeos_engine::contracts::ItemSpace::Project => "project".to_string(),
                ryeos_engine::contracts::ItemSpace::Node => "node".to_string(),
            },
            label: r.label.clone(),
            path: r.ai_root.display().to_string(),
        })
        .collect();

    // ── Bundles (from node config) ──
    let bundles: Vec<BundleSummary> = state
        .node_config
        .bundles
        .iter()
        .map(|b| BundleSummary {
            name: b.name.clone(),
            path: b.path.display().to_string(),
        })
        .collect();

    // ── Services (from service descriptors) ──
    let services: Vec<ServiceSummary> = state
        .service_descriptors
        .iter()
        .map(|d| ServiceSummary {
            endpoint: d.endpoint.to_string(),
            service_ref: d.service_ref.to_string(),
            availability: format!("{:?}", d.availability),
            required_caps: d.required_caps.iter().map(|s| s.to_string()).collect(),
        })
        .collect();

    // ── Commands ──
    let commands: Vec<CommandSummary> = state
        .command_registry
        .all_commands()
        .iter()
        .map(|command| {
            let target = match &command.dispatch {
                ryeos_runtime::CommandDispatch::ExecuteRef { execute, .. } => Some(execute.clone()),
                _ => None,
            };
            CommandSummary {
                name: command.name.clone(),
                target,
            }
        })
        .collect();

    // ── Command aliases ──
    let command_aliases: Vec<CommandSummary> = state
        .command_registry
        .all_commands()
        .iter()
        .flat_map(|command| {
            command.aliases.iter().map(move |alias| CommandSummary {
                name: alias.tokens.join(" "),
                target: Some(command.name.clone()),
            })
        })
        .collect();

    // ── Project ──
    let project = project_identity.map(|path| ProjectInfo {
        path: path.to_string_lossy().into_owned(),
    });

    // ── Remotes (configured only, no probes) ──
    let (remotes, invalid_remotes) = load_remotes(state, project_path)?;

    // ── Threads ──
    let active_threads = state.state_store.active_thread_count()?;

    // ── Schedules ──
    let schedules = load_schedule_summary(state, &session.principal_id)?;

    // ── GC ──
    let gc = load_gc_summary(state)?;

    Ok(RyeOsDimensionProjection {
        schema_version: "ryeos.ui.dimension.v3",
        generated_at: lillux::time::iso8601_now(),
        session: SessionInfo {
            session_id: session.session_id.clone(),
            root_ref: attachment.root_ref.clone(),
            title: attachment.presentation.definition.title().to_owned(),
            user_principal_id: session.user_principal_id.clone(),
            binding_digest: attachment.compiled_binding.binding_digest.clone(),
            posture: attachment.compiled_binding.posture,
        },
        local_node: LocalNode {
            identity,
            status,
            health,
            spaces,
            bundles,
            services,
            commands,
            command_aliases,
        },
        project,
        remotes,
        invalid_remotes,
        threads: ThreadSummary {
            active_count: active_threads,
        },
        schedules,
        gc,
    })
}

// ── Data-loading helpers ────────────────────────────────────────────

/// Load configured remotes without probing; configuration errors are not absence.
fn load_remotes(
    state: &AppState,
    project_path: Option<&PathBuf>,
) -> Result<(Vec<RemoteSummary>, Vec<InvalidRemoteSummary>)> {
    let project = project_path.map(|p| p.as_path());
    let report =
        ryeos_api::remote::config::load_remotes_layered_report(&state.config.app_root, project)?;
    Ok(remote_summaries(report))
}

fn remote_summaries(
    report: ryeos_api::remote::config::RemotesLoadReport,
) -> (Vec<RemoteSummary>, Vec<InvalidRemoteSummary>) {
    let mut entries: Vec<RemoteSummary> = report
        .remotes
        .values()
        .map(|loaded| RemoteSummary {
            name: loaded.config.name.clone(),
            url: loaded.config.url.clone(),
            principal_id: loaded.config.principal_id.clone(),
        })
        .collect();
    entries.sort_by(|a, b| a.name.cmp(&b.name));
    let mut invalid: Vec<_> = report
        .invalid
        .into_iter()
        .map(|entry| InvalidRemoteSummary {
            name: entry.name,
            scope: entry.scope.label(),
            error: entry.error,
            repair_hint: entry.repair_hint,
        })
        .collect();
    invalid.sort_by(|a, b| (a.name.as_str(), a.scope).cmp(&(b.name.as_str(), b.scope)));
    (entries, invalid)
}

/// Current caller-owned schedules, using the scheduler's execution authority.
fn load_schedule_summary(state: &AppState, principal: &str) -> Result<ScheduleSummary> {
    let specs = state
        .scheduler_db
        .list_specs_filtered(false, None, Some(principal))?;
    Ok(schedule_summary(
        principal,
        specs.iter().map(|spec| spec.enabled),
    ))
}

fn schedule_summary(principal: &str, enabled: impl Iterator<Item = bool>) -> ScheduleSummary {
    let (total, enabled) = enabled.fold((0, 0), |(total, count), enabled| {
        (total + 1, count + usize::from(enabled))
    });
    ScheduleSummary {
        identity_filtered: true,
        owner_principal_id: principal.into(),
        coverage: "current_caller_owned_schedules_not_node_or_project_totals",
        total,
        enabled,
    }
}

/// Read bounded historical observations, never infer live ownership from files.
fn load_gc_summary(state: &AppState) -> Result<GcSummary> {
    let runtime_state_dir = state.config.runtime_state_dir();
    let log_path = runtime_state_dir.join("logs").join("gc.jsonl");
    gc_summary_from_log(&log_path)
}

fn gc_summary_from_log(log_path: &std::path::Path) -> Result<GcSummary> {
    Ok(GcSummary {
        activity: "unknown",
        coverage: "bounded_log_observations_not_live_or_exhaustive",
        recent_events: super::ui_gc::read_recent_gc_events(log_path, 5)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schedule_summary_is_explicitly_scoped_to_execution_owner() {
        let db = ryeos_scheduler::SchedulerDb::new_in_memory().unwrap();
        let own = format!("fp:{}", "11".repeat(32));
        let foreign = format!("fp:{}", "33".repeat(32));
        for (id, principal, enabled) in [
            ("own-enabled", &own, true),
            ("own-disabled", &own, false),
            ("foreign", &foreign, true),
        ] {
            db.upsert_spec(&super::super::ui_schedules::owner_fixture_spec(
                id, principal, enabled,
            ))
            .unwrap();
        }
        let specs = db.list_specs_filtered(false, None, Some(&own)).unwrap();
        let summary = schedule_summary(&own, specs.iter().map(|spec| spec.enabled));
        assert!(summary.identity_filtered);
        assert_eq!(summary.owner_principal_id, own);
        assert_eq!((summary.total, summary.enabled), (2, 1));
        assert_eq!(
            summary.coverage,
            "current_caller_owned_schedules_not_node_or_project_totals"
        );
        let other = format!("fp:{}", "55".repeat(32));
        let specs = db.list_specs_filtered(false, None, Some(&other)).unwrap();
        let empty = schedule_summary(&other, specs.iter().map(|spec| spec.enabled));
        assert_eq!((empty.total, empty.enabled), (0, 0));
        assert!(empty.identity_filtered);
    }

    #[test]
    fn invalid_only_remote_configuration_is_not_reported_as_absence() {
        use ryeos_api::remote::config::{InvalidRemote, RemoteConfigScope, RemotesLoadReport};
        let report = RemotesLoadReport {
            remotes: Default::default(),
            invalid: vec![InvalidRemote {
                name: "broken".into(),
                scope: RemoteConfigScope::Operator,
                config_path: PathBuf::from("unused"),
                error: "missing trust pin".into(),
                url: None,
                repair_hint: "configure the remote".into(),
            }],
        };
        let (valid, invalid) = remote_summaries(report);
        assert!(valid.is_empty());
        assert_eq!(invalid.len(), 1);
        assert_eq!(invalid[0].name, "broken");
        assert_eq!(invalid[0].scope, "operator");
        assert_eq!(invalid[0].error, "missing trust pin");
        assert_eq!(invalid[0].repair_hint, "configure the remote");
    }

    #[test]
    fn gc_summary_does_not_claim_liveness_or_hide_corrupt_observations() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("gc.jsonl");
        let absent = serde_json::to_value(gc_summary_from_log(&path).unwrap()).unwrap();
        assert_eq!(absent["activity"], "unknown");
        assert!(absent.get("running").is_none());
        assert_eq!(
            absent["coverage"],
            "bounded_log_observations_not_live_or_exhaustive"
        );
        std::fs::write(&path, "not json\n").unwrap();
        assert!(gc_summary_from_log(&path).is_err());
        std::fs::write(&path, "{\"phase\":\"finished\"}\n").unwrap();
        let observed = gc_summary_from_log(&path).unwrap();
        assert_eq!(observed.activity, "unknown");
        assert_eq!(observed.recent_events[0]["phase"], "finished");
    }
}

// ── Descriptor ──────────────────────────────────────────────────────

pub const DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:ui/ryeos-ui/dimension/get",
    endpoint: "ui.ryeos.dimension.get",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &[],
    handler: |params, ctx, state| Box::pin(async move { handle(params, ctx, state).await }),
};

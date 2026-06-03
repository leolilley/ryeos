//! `ui.studio.dimension.get` — bounded read-only Studio Dimension projection.
//!
//! Aggregates daemon health, node identity, project context, spaces/bundles,
//! remotes (configured only, no probes), threads, schedules, GC state, and
//! service catalog into a single JSON response. Designed to render the
//! Studio Home/Workbench without any graph interaction.
//!
//! Read-only. No remote probes. No pseudo item kinds.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use serde::Serialize;
use serde_json::Value;

use ryeos_api::registry::ServiceDescriptor;
use ryeos_app::handler_context::HandlerContext;
use ryeos_app::handler_error::HandlerError;
use ryeos_app::state::AppState;
use ryeos_executor::executor::ServiceAvailability;

use crate::state::get_ui_state;

// ── Response view-model ─────────────────────────────────────────────

#[derive(Debug, Serialize)]
#[serde(deny_unknown_fields)]
pub struct StudioDimensionProjection {
    pub schema_version: &'static str,
    pub generated_at: String,
    pub session: SessionInfo,
    pub local_node: LocalNode,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project: Option<ProjectInfo>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub remotes: Vec<RemoteSummary>,
    pub threads: ThreadSummary,
    pub schedules: ScheduleSummary,
    pub gc: GcSummary,
}

#[derive(Debug, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SessionInfo {
    pub session_id: String,
    pub surface_ref: String,
    pub read_only: bool,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub granted_caps: Vec<String>,
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
    pub verbs: Vec<VerbAliasSummary>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub aliases: Vec<VerbAliasSummary>,
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
pub struct VerbAliasSummary {
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
pub struct ThreadSummary {
    pub active_count: i64,
}

#[derive(Debug, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ScheduleSummary {
    pub total: usize,
    pub enabled: usize,
}

#[derive(Debug, Serialize)]
#[serde(deny_unknown_fields)]
pub struct GcSummary {
    pub running: bool,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub recent_events: Vec<Value>,
}

// ── Handler ────────────────────────────────────────────────────────

fn session_id_from_context(ctx: &HandlerContext) -> Option<String> {
    ctx.fingerprint.strip_prefix("session:").map(String::from)
}

pub async fn handle(_params: Value, ctx: HandlerContext, state: Arc<AppState>) -> Result<Value> {
    let session_id = session_id_from_context(&ctx).ok_or_else(|| {
        HandlerError::Forbidden("browser session required for studio dimension".into())
    })?;

    let session = get_ui_state(&state)
        .expect("UiState not set")
        .browser_sessions
        .get_session(&session_id)
        .ok_or(HandlerError::Forbidden("session expired or invalid".into()))?;

    let project_path = session.project_root.clone().map(PathBuf::from);

    let projection = build_dimension_projection(&state, &session, project_path.as_ref());

    serde_json::to_value(projection).map_err(Into::into)
}

fn build_dimension_projection(
    state: &AppState,
    session: &crate::browser_session::BrowserSession,
    project_path: Option<&PathBuf>,
) -> StudioDimensionProjection {
    // ── Identity ──
    let identity = IdentityInfo {
        principal_id: state.identity.principal_id(),
        fingerprint: state.identity.fingerprint().to_string(),
    };

    // ── Status + Health (reuse existing logic inline) ──
    let status = serde_json::to_value(state.status()).unwrap_or_default();
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
                ryeos_engine::contracts::ItemSpace::System => "system".to_string(),
                ryeos_engine::contracts::ItemSpace::User => "user".to_string(),
                ryeos_engine::contracts::ItemSpace::Project => "project".to_string(),
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

    // ── Verbs ──
    let verbs: Vec<VerbAliasSummary> = state
        .verb_registry
        .verb_names()
        .map(|name| {
            let target = state
                .verb_registry
                .get_verb(name)
                .and_then(|v| v.execute.clone());
            VerbAliasSummary {
                name: name.to_string(),
                target,
            }
        })
        .collect();

    // ── Aliases ──
    let aliases: Vec<VerbAliasSummary> = state
        .alias_registry
        .all_aliases()
        .iter()
        .map(|def| {
            let name = def.tokens.join(" ");
            VerbAliasSummary {
                name,
                target: Some(def.verb.clone()),
            }
        })
        .collect();

    // ── Project ──
    let project = session
        .project_root
        .as_ref()
        .map(|p| ProjectInfo { path: p.clone() });

    // ── Remotes (configured only, no probes) ──
    let remotes = load_remotes(state, project_path);

    // ── Threads ──
    let active_threads = state.state_store.active_thread_count().unwrap_or(0);

    // ── Schedules ──
    let (total_schedules, enabled_schedules) = load_schedule_summary(state);

    // ── GC ──
    let gc = load_gc_summary(state);

    StudioDimensionProjection {
        schema_version: "ryeos.studio.dimension.v0",
        generated_at: lillux::time::iso8601_now(),
        session: SessionInfo {
            session_id: session.session_id.clone(),
            surface_ref: session.surface_ref.clone(),
            read_only: session.read_only,
            granted_caps: session.granted_caps.clone(),
        },
        local_node: LocalNode {
            identity,
            status,
            health,
            spaces,
            bundles,
            services,
            verbs,
            aliases,
        },
        project,
        remotes,
        threads: ThreadSummary {
            active_count: active_threads,
        },
        schedules: ScheduleSummary {
            total: total_schedules,
            enabled: enabled_schedules,
        },
        gc,
    }
}

// ── Data-loading helpers ────────────────────────────────────────────

/// Load configured remotes without probing. Returns empty on error.
fn load_remotes(state: &AppState, project_path: Option<&PathBuf>) -> Vec<RemoteSummary> {
    // Import remote config loading from ryeos-api's remote module.
    // We can't directly depend on ryeos-api's remote module from ryeos-ui,
    // so we load remotes using the layered config function exposed via
    // the AppState's system_space_dir.
    let project = project_path.map(|p| p.as_path());
    let system_space = &state.config.system_space_dir;

    // Try to load remotes — the config module is in ryeos-api which we
    // depend on transitively. Use the public config loading function.
    match ryeos_api::remote::config::load_remotes_layered(system_space, project) {
        Ok(remotes_map) => {
            let mut entries: Vec<RemoteSummary> = remotes_map
                .values()
                .map(|r| RemoteSummary {
                    name: r.name.clone(),
                    url: r.url.clone(),
                    principal_id: r.principal_id.clone(),
                })
                .collect();
            entries.sort_by(|a, b| a.name.cmp(&b.name));
            entries
        }
        Err(_) => Vec::new(),
    }
}

/// Load schedule summary counts. Returns (total, enabled).
fn load_schedule_summary(state: &AppState) -> (usize, usize) {
    match state.scheduler_db.list_specs_filtered(false, None, None) {
        Ok(specs) => {
            let total = specs.len();
            let enabled = specs.iter().filter(|s| s.enabled).count();
            (total, enabled)
        }
        Err(_) => (0, 0),
    }
}

/// Load GC read-only status: check lock file, read recent event log.
fn load_gc_summary(state: &AppState) -> GcSummary {
    let state_root = state.config.system_space_dir.join(".ai").join("state");

    // Check if GC is currently running (lock file + state sidecar exist).
    let lock_path = state_root.join("gc.lock");
    let state_sidecar = state_root.join("gc.state.json");
    let running = lock_path.exists() && state_sidecar.exists();

    // Read recent GC events from the JSONL log.
    let log_path = state_root.join("logs").join("gc.jsonl");
    let recent_events = read_recent_gc_events(&log_path, 5);

    GcSummary {
        running,
        recent_events,
    }
}

/// Read the last N lines from the GC event log as JSON values.
fn read_recent_gc_events(log_path: &std::path::Path, limit: usize) -> Vec<Value> {
    let content = match std::fs::read_to_string(log_path) {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };

    let mut events: Vec<Value> = Vec::new();
    for line in content.lines().rev() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Ok(event) = serde_json::from_str::<Value>(trimmed) {
            events.push(event);
            if events.len() >= limit {
                break;
            }
        }
    }
    // Reverse so events are in chronological order (oldest first).
    events.reverse();
    events
}

// ── Descriptor ──────────────────────────────────────────────────────

pub const DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:ui/studio/dimension/get",
    endpoint: "ui.studio.dimension.get",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &[],
    handler: |params, ctx, state| Box::pin(async move { handle(params, ctx, state).await }),
};

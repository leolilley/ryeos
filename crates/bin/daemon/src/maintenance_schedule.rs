//! Bundle-authored maintenance schedule application.
//!
//! The system has no daemon-internal GC trigger. Rather than inventing one, GC
//! becomes a normal scheduled thread: the standard bundle authors a maintenance
//! schedule *declaration* (cadence, item ref, params, granted capabilities), and
//! this module reconciles it into a signed node schedule spec at daemon init —
//! the same shape `scheduler.register` / project sync produce, so the scheduler
//! reconcile that runs immediately afterwards projects and fires it like any
//! other schedule.
//!
//! Why a declaration + init-time reconcile instead of shipping the final spec:
//! the node spec must carry `execution.requester_fingerprint` (the acting
//! principal at dispatch) and a node signature. Those are per-install, so they
//! can't be baked into a portable bundle artifact — the daemon fills its own
//! identity here.
//!
//! Idempotency & operator control: a spec is written only if the target file is
//! absent. Once applied, `scheduler pause` (which flips `enabled` in the node
//! YAML) and any operator cadence edits survive every restart untouched.

use std::path::Path;

use anyhow::{Context, Result};
use serde::Deserialize;
use serde_json::Value;

use ryeos_app::identity::NodeIdentity;
use ryeos_app::node_config::writer;
use ryeos_app::state::AppState;

/// Relative location (under `.ai/node/`) of the bundle-authored declaration.
const DECLARATION_REL: &str = "maintenance/schedules.yaml";

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct MaintenanceDeclarationFile {
    /// Format guard — only `1` is understood.
    spec_version: u32,
    schedules: Vec<MaintenanceScheduleDeclaration>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct MaintenanceScheduleDeclaration {
    schedule_id: String,
    item_ref: String,
    schedule_type: String,
    expression: String,
    #[serde(default)]
    timezone: Option<String>,
    #[serde(default)]
    misfire_policy: Option<String>,
    #[serde(default)]
    overlap_policy: Option<String>,
    #[serde(default)]
    lateness_grace_secs: Option<i64>,
    #[serde(default = "default_enabled")]
    enabled: bool,
    #[serde(default)]
    params: Value,
    /// Capabilities the schedule dispatches with — least privilege. The node
    /// signs the resulting spec, so these are trusted at fire time.
    capabilities: Vec<String>,
}

fn default_enabled() -> bool {
    true
}

/// Reconcile the bundle-authored maintenance schedule into a signed node spec.
///
/// A no-op when the standard bundle's declaration is absent. Safe to call on
/// every boot; existing specs are never overwritten (operator pause/edits win).
pub fn ensure_maintenance_schedule(state: &AppState) -> Result<()> {
    let node_dir = state
        .config
        .app_root
        .join(ryeos_engine::AI_DIR)
        .join("node");
    apply_maintenance_schedules(&node_dir, &state.identity)
}

fn apply_maintenance_schedules(node_dir: &Path, identity: &NodeIdentity) -> Result<()> {
    let Some(file) = load_declaration(node_dir)? else {
        return Ok(());
    };

    if file.spec_version != 1 {
        anyhow::bail!(
            "maintenance declaration has unsupported spec_version {} (only 1)",
            file.spec_version
        );
    }

    let schedules_dir = node_dir.join("schedules");
    for decl in &file.schedules {
        let target = schedules_dir.join(format!("{}.yaml", decl.schedule_id));
        if target.exists() {
            tracing::debug!(
                schedule_id = %decl.schedule_id,
                "maintenance schedule already applied — preserving operator state"
            );
            continue;
        }
        write_maintenance_spec(node_dir, decl, identity).with_context(|| {
            format!("apply maintenance schedule '{}'", decl.schedule_id)
        })?;
        tracing::info!(
            schedule_id = %decl.schedule_id,
            item_ref = %decl.item_ref,
            expression = %decl.expression,
            "applied bundle-authored maintenance schedule"
        );
    }

    Ok(())
}

/// Load the maintenance declaration at `.ai/node/maintenance/schedules.yaml`.
///
/// `None` when absent — and absent means NOTHING is scheduled. The
/// declaration is the only source of maintenance cadence: no declaration,
/// no scheduled GC. Deliberate; a node whose bundle doesn't declare
/// maintenance must never grow a background job it can't see in data.
fn load_declaration(node_dir: &Path) -> Result<Option<MaintenanceDeclarationFile>> {
    let declaration_path = node_dir.join(DECLARATION_REL);
    if !declaration_path.is_file() {
        tracing::info!(
            path = %declaration_path.display(),
            "no maintenance declaration; nothing scheduled"
        );
        return Ok(None);
    }
    let raw = std::fs::read_to_string(&declaration_path)
        .with_context(|| format!("read maintenance declaration {}", declaration_path.display()))?;
    let body_str = lillux::signature::strip_signature_lines(&raw);
    serde_yaml::from_str(&body_str)
        .map(Some)
        .with_context(|| format!("parse maintenance declaration {}", declaration_path.display()))
}

fn write_maintenance_spec(
    node_dir: &Path,
    decl: &MaintenanceScheduleDeclaration,
    identity: &NodeIdentity,
) -> Result<()> {
    // Defensive validation — the projection re-validates on rebuild, but fail
    // loudly at apply time rather than silently emitting a spec it rejects.
    ryeos_scheduler::crontab::validate_schedule_id(&decl.schedule_id)?;
    ryeos_engine::canonical_ref::CanonicalRef::parse(&decl.item_ref)
        .with_context(|| format!("invalid item_ref '{}'", decl.item_ref))?;
    ryeos_scheduler::crontab::validate_expression(&decl.schedule_type, &decl.expression)?;
    let timezone = decl.timezone.as_deref().unwrap_or("UTC");
    ryeos_scheduler::crontab::validate_timezone(timezone)?;

    if decl.capabilities.is_empty() {
        anyhow::bail!(
            "maintenance schedule '{}' declares no capabilities; it could never dispatch",
            decl.schedule_id
        );
    }
    if !decl.params.is_null() && !decl.params.is_object() {
        anyhow::bail!(
            "maintenance schedule '{}' params must be a mapping",
            decl.schedule_id
        );
    }

    let normalized_misfire = match decl.misfire_policy.as_deref() {
        Some(p) if !p.is_empty() => p.to_string(),
        _ => match decl.schedule_type.as_str() {
            "interval" => "fire_once_now".to_string(),
            _ => "skip".to_string(),
        },
    };
    let overlap_policy = decl
        .overlap_policy
        .clone()
        .unwrap_or_else(|| "skip".to_string());
    let lateness_grace_secs = decl.lateness_grace_secs.unwrap_or(60);
    let params = if decl.params.is_null() {
        serde_json::json!({})
    } else {
        decl.params.clone()
    };

    // The node is both signer and acting principal for its own maintenance.
    let body = serde_json::json!({
        "spec_version": 1,
        "schedule_id": decl.schedule_id,
        "item_ref": decl.item_ref,
        "schedule_type": decl.schedule_type,
        "expression": decl.expression,
        "timezone": timezone,
        "enabled": decl.enabled,
        "registered_at": lillux::time::timestamp_millis(),
        "misfire_policy": normalized_misfire,
        "overlap_policy": overlap_policy,
        "lateness_grace_secs": lateness_grace_secs,
        "params": params,
        "execution": {
            "requester_fingerprint": identity.fingerprint(),
            "capabilities": decl.capabilities,
        },
    });

    writer::write_signed_node_item(node_dir, "schedules", &decl.schedule_id, &body, identity)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use lillux::crypto::EncodePrivateKey;
    use rand::rngs::OsRng;

    fn identity() -> NodeIdentity {
        let tmp = tempfile::tempdir().unwrap();
        let key_path = tmp.path().join("identity/private_key.pem");
        std::fs::create_dir_all(key_path.parent().unwrap()).unwrap();
        let key = lillux::crypto::SigningKey::generate(&mut OsRng);
        std::fs::write(
            &key_path,
            key.to_pkcs8_pem(Default::default()).unwrap().as_bytes(),
        )
        .unwrap();
        NodeIdentity::load(&key_path).unwrap()
    }

    fn write_declaration(node_dir: &Path, body: &str) {
        let dir = node_dir.join("maintenance");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("schedules.yaml"), body).unwrap();
    }

    const DECL: &str = r#"spec_version: 1
schedules:
  - schedule_id: maintenance-gc
    item_ref: "service:maintenance/gc"
    schedule_type: cron
    expression: "0 0 4 * * *"
    overlap_policy: skip
    misfire_policy: skip
    params:
      deep: true
    capabilities:
      - "ryeos.execute.service.maintenance/gc"
"#;

    #[test]
    fn applies_declaration_into_signed_node_spec() {
        let tmp = tempfile::tempdir().unwrap();
        let node_dir = tmp.path().join(".ai").join("node");
        std::fs::create_dir_all(&node_dir).unwrap();
        write_declaration(&node_dir, DECL);
        let id = identity();

        apply_maintenance_schedules(&node_dir, &id).unwrap();

        let spec_path = node_dir.join("schedules").join("maintenance-gc.yaml");
        let content = std::fs::read_to_string(&spec_path).unwrap();
        // Signed by the node.
        assert!(content.starts_with("# ryeos:signed:"), "spec must be signed");
        let body = lillux::signature::strip_signature_lines(&content);
        let parsed: serde_json::Value = serde_yaml::from_str(&body).unwrap();
        assert_eq!(parsed["item_ref"], "service:maintenance/gc");
        assert_eq!(parsed["overlap_policy"], "skip");
        assert_eq!(parsed["params"]["deep"], true);
        assert_eq!(
            parsed["execution"]["requester_fingerprint"],
            id.fingerprint()
        );
        assert_eq!(
            parsed["execution"]["capabilities"][0],
            "ryeos.execute.service.maintenance/gc"
        );
        assert!(parsed["registered_at"].is_i64());
    }

    #[test]
    fn does_not_overwrite_existing_spec() {
        let tmp = tempfile::tempdir().unwrap();
        let node_dir = tmp.path().join(".ai").join("node");
        std::fs::create_dir_all(node_dir.join("schedules")).unwrap();
        write_declaration(&node_dir, DECL);
        let id = identity();

        // Pre-existing (operator-paused) spec must be preserved verbatim.
        let existing = node_dir.join("schedules").join("maintenance-gc.yaml");
        std::fs::write(&existing, "# operator-managed\nenabled: false\n").unwrap();

        apply_maintenance_schedules(&node_dir, &id).unwrap();

        assert_eq!(
            std::fs::read_to_string(&existing).unwrap(),
            "# operator-managed\nenabled: false\n",
            "existing schedule must not be clobbered"
        );
    }

    #[test]
    fn absent_declaration_schedules_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let node_dir = tmp.path().join(".ai").join("node");
        std::fs::create_dir_all(&node_dir).unwrap();
        let id = identity();

        // No declaration → no schedule. The declaration is the only source
        // of maintenance cadence; nothing is scheduled implicitly.
        apply_maintenance_schedules(&node_dir, &id).unwrap();

        let spec_path = node_dir.join("schedules").join("maintenance-gc.yaml");
        assert!(
            !spec_path.exists(),
            "no declaration must mean no schedule is written"
        );
    }

    #[test]
    fn rejects_empty_capabilities() {
        let tmp = tempfile::tempdir().unwrap();
        let node_dir = tmp.path().join(".ai").join("node");
        std::fs::create_dir_all(&node_dir).unwrap();
        write_declaration(
            &node_dir,
            r#"spec_version: 1
schedules:
  - schedule_id: maintenance-gc
    item_ref: "service:maintenance/gc"
    schedule_type: cron
    expression: "0 0 4 * * *"
    capabilities: []
"#,
        );
        let id = identity();
        assert!(apply_maintenance_schedules(&node_dir, &id).is_err());
    }
}

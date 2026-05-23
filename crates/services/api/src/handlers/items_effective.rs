//! `items.effective` — resolve an item by canonical ref and return its effective content.
//!
//! Works for any item kind (executable or not). Returns the parsed document
//! content as JSON, along with resolution metadata (space, trust status, path).

use std::sync::Arc;

use anyhow::Result;
use serde_json::Value;

use crate::handler_context::HandlerContext;
use crate::registry::ServiceDescriptor;
use ryeos_app::state::AppState;
use ryeos_engine::canonical_ref::CanonicalRef;
use ryeos_engine::item_resolution;
use ryeos_engine::trust;
use ryeos_executor::executor::ServiceAvailability;

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    /// Canonical item ref to resolve, e.g. "surface:ryeos/cockpit/base".
    pub canonical_ref: String,

    /// Optional project path for project-space resolution.
    /// When absent, only system and user spaces are searched.
    #[serde(default)]
    pub project_path: Option<String>,
}

pub async fn handle(req: Request, _ctx: HandlerContext, state: Arc<AppState>) -> Result<Value> {
    let item_ref = CanonicalRef::parse(&req.canonical_ref)
        .map_err(|e| anyhow::anyhow!("invalid canonical ref '{}': {e}", req.canonical_ref))?;

    let kind_schema = state
        .engine
        .kinds
        .get(&item_ref.kind)
        .ok_or_else(|| anyhow::anyhow!("unknown kind '{}'", item_ref.kind))?;

    // Build resolution roots: system + user + optional project
    let project_root = req.project_path.as_ref().map(|p| std::path::PathBuf::from(p));
    let roots = state.engine.resolution_roots(project_root);

    // Resolve to file path
    let result = item_resolution::resolve_item_full(&roots, kind_schema, &item_ref)
        .map_err(|e| anyhow::anyhow!("resolution failed for '{}': {e}", item_ref))?;

    // Read file content
    let content = std::fs::read_to_string(&result.winner_path).map_err(|e| {
        anyhow::anyhow!("failed to read {}: {e}", result.winner_path.display())
    })?;

    // Strip signature header (if signed)
    let body = trust::strip_signature_lines(&content);

    // Determine format from matched extension
    let ext = result.matched_ext.as_str();
    let parsed: Value = if ext == ".yaml" || ext == ".yml" {
        serde_yaml::from_str(&body)
            .map_err(|e| anyhow::anyhow!("YAML parse error: {e}"))?
    } else if ext == ".toml" {
        toml::from_str(&body)
            .map_err(|e| anyhow::anyhow!("TOML parse error: {e}"))?
    } else {
        serde_json::from_str(&body)
            .map_err(|e| anyhow::anyhow!("JSON parse error: {e}"))?
    };

    // Check trust: signed if signature header was present
    let is_signed = content.starts_with("# ryeos:signed:");
    // TODO: actual signature verification when trust store integration is available

    let shadowed_count = result.shadowed.len();
    let shadowed: Vec<Value> = result
        .shadowed
        .iter()
        .map(|s| {
            serde_json::json!({
                "space": format!("{:?}", s.space),
                "path": s.path.display().to_string(),
            })
        })
        .collect();

    Ok(serde_json::json!({
        "requested_ref": item_ref.to_string(),
        "canonical_ref": item_ref.to_string(),
        "kind": item_ref.kind,
        "bare_id": item_ref.bare_id,
        "resolved_space": format!("{:?}", result.winner_space),
        "resolved_path": result.winner_path.display().to_string(),
        "signed": is_signed,
        "composed": parsed,
        "shadowed_count": shadowed_count,
        "shadowed": shadowed,
    }))
}

pub const DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:items/effective",
    endpoint: "items.effective",
    availability: ServiceAvailability::Both,
    required_caps: &[],
    handler: |params, ctx, state| {
        Box::pin(async move {
            let req: Request = crate::handler_error::parse_request(params)?;
            handle(req, ctx, state).await
        })
    },
};

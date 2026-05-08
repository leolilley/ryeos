//! `ryos-core-tools fetch` — resolve and read an item through the engine.

use std::path::Path;

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use ryeos_engine::canonical_ref::CanonicalRef;
use ryeos_engine::contracts::{
    EffectivePrincipal, ExecutionHints, PlanContext, Principal, ProjectContext, TrustClass,
};
use ryeos_engine::engine::Engine;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FetchParams {
    pub item_ref: String,
    #[serde(default)]
    pub with_content: bool,
    #[serde(default)]
    pub verify: bool,
    #[serde(default)]
    pub project_path: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct FetchReport {
    pub item_ref: String,
    pub kind: String,
    pub resolved_path: String,
    pub resolved_from: String,
    pub space: String,
    pub content_hash: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub signature_status: Option<String>,
    pub shadowed_count: usize,
    pub fetch_status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

pub fn run_fetch(params: FetchParams, engine: &Engine) -> Result<Value> {
    let canonical_ref = CanonicalRef::parse(&params.item_ref)
        .map_err(|e| anyhow!("failed to parse item ref `{}`: {e}", params.item_ref))?;

    let project_path = params.project_path.as_deref().map(Path::new);

    let plan_ctx = PlanContext {
        requested_by: EffectivePrincipal::Local(Principal {
            fingerprint: "inspect-tool".to_string(),
            scopes: vec!["bundle.read".to_string()],
        }),
        project_context: project_path
            .map(|p| ProjectContext::LocalPath { path: p.to_path_buf() })
            .unwrap_or(ProjectContext::None),
        current_site_id: "site:local".into(),
        origin_site_id: "site:local".into(),
        execution_hints: ExecutionHints::default(),
        validate_only: false,
    };

    let resolved = match engine.resolve(&plan_ctx, &canonical_ref) {
        Ok(item) => item,
        Err(e) => {
            return serde_json::to_value(FetchReport {
                item_ref: params.item_ref,
                kind: canonical_ref.kind.clone(),
                resolved_path: String::new(),
                resolved_from: String::new(),
                space: String::new(),
                content_hash: String::new(),
                signature_status: None,
                shadowed_count: 0,
                fetch_status: "FAILED".into(),
                content: None,
                error: Some(format!("{e}")),
            })
            .map_err(Into::into);
        }
    };

    let signature_status = if params.verify {
        match engine.verify(&plan_ctx, resolved.clone()) {
            Ok(verified) => Some(
                match verified.trust_class {
                    TrustClass::Trusted => "TRUSTED",
                    TrustClass::Untrusted => "UNTRUSTED",
                    TrustClass::Unsigned => "UNSIGNED",
                }
                .to_string(),
            ),
            Err(e) => Some(format!("VERIFICATION_FAILED: {e}")),
        }
    } else {
        None
    };

    let content = std::fs::read_to_string(&resolved.source_path)
        .with_context(|| format!("failed to read item content from {:?}", resolved.source_path))?;

    let report = FetchReport {
        item_ref: params.item_ref,
        kind: resolved.kind.clone(),
        resolved_path: resolved.source_path.display().to_string(),
        resolved_from: resolved.resolved_from.clone(),
        space: resolved.source_space.as_str().to_string(),
        content_hash: resolved.content_hash.clone(),
        signature_status,
        shadowed_count: resolved.shadowed.len(),
        fetch_status: "SUCCESS".into(),
        content: if params.with_content { Some(content) } else { None },
        error: None,
    };

    serde_json::to_value(report).map_err(Into::into)
}

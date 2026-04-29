//! `rye-inspect verify` — resolve and trust-verify an item through the engine.

use std::path::Path;

use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use ryeos_engine::canonical_ref::CanonicalRef;
use ryeos_engine::contracts::{
    EffectivePrincipal, ExecutionHints, PlanContext, Principal, ProjectContext, TrustClass,
};
use ryeos_engine::engine::Engine;

#[derive(Debug, Deserialize)]
pub struct VerifyParams {
    pub item_ref: String,
    #[serde(default)]
    pub project_path: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct VerifyReport {
    pub item_ref: String,
    pub kind: String,
    pub resolved_path: String,
    pub space: String,
    pub content_hash: String,
    pub trust_class: String,
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

pub fn run_verify(params: VerifyParams, engine: &Engine) -> Result<Value> {
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
            return serde_json::to_value(VerifyReport {
                item_ref: params.item_ref,
                kind: canonical_ref.kind.clone(),
                resolved_path: String::new(),
                space: String::new(),
                content_hash: String::new(),
                trust_class: String::new(),
                status: "FAILED".into(),
                error: Some(format!("{e}")),
            })
            .map_err(Into::into);
        }
    };

    let resolved_path = resolved.source_path.display().to_string();
    let space = resolved.source_space.as_str().to_string();
    let content_hash = resolved.content_hash.clone();
    let kind = resolved.kind.clone();

    let report = match engine.verify(&plan_ctx, resolved) {
        Ok(verified) => {
            let trust_class = match verified.trust_class {
                TrustClass::Trusted => "TRUSTED",
                TrustClass::Untrusted => "UNTRUSTED",
                TrustClass::Unsigned => "UNSIGNED",
            };
            VerifyReport {
                item_ref: params.item_ref,
                kind,
                resolved_path,
                space,
                content_hash,
                trust_class: trust_class.to_string(),
                status: "SUCCESS".into(),
                error: None,
            }
        }
        Err(e) => VerifyReport {
            item_ref: params.item_ref,
            kind,
            resolved_path,
            space,
            content_hash,
            trust_class: String::new(),
            status: "VERIFICATION_FAILED".into(),
            error: Some(format!("{e}")),
        },
    };

    serde_json::to_value(report).map_err(Into::into)
}

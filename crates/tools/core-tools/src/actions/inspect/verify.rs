//! `ryeos-core-tools verify` — resolve and trust-verify an item through the engine.

use std::path::{Path, PathBuf};

use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use ryeos_engine::canonical_ref::CanonicalRef;
use ryeos_engine::contracts::{
    EffectivePrincipal, ExecutionHints, PlanContext, Principal, ProjectContext, TrustClass,
};
use ryeos_engine::engine::Engine;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VerifyParams {
    #[serde(
        default,
        deserialize_with = "ryeos_runtime::scalar_or_vec::deserialize"
    )]
    pub item_refs: Vec<String>,
    #[serde(default)]
    pub item_ref: Option<String>,
    #[serde(default, alias = "project")]
    pub project_path: Option<String>,
    #[serde(default)]
    pub no_project: bool,
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

#[derive(Debug, Serialize)]
pub struct VerifyBatchReport {
    pub status: String,
    pub verified: Vec<VerifyReport>,
    pub failed: Vec<VerifyReport>,
}

pub struct VerifyRun {
    pub report: Value,
    pub failed: usize,
    pub total: usize,
}

impl VerifyParams {
    fn into_targets(self) -> Result<(Vec<String>, Option<String>, bool)> {
        let targets = if self.item_refs.is_empty() {
            self.item_ref.into_iter().collect::<Vec<_>>()
        } else {
            self.item_refs
        };
        if targets.is_empty() {
            return Err(anyhow!("ITEM_REF required (pass item_ref or item_refs)"));
        }
        if targets.iter().any(|target| target.trim().is_empty()) {
            return Err(anyhow!("verify item refs must not be empty"));
        }
        Ok((targets, self.project_path, self.no_project))
    }
}

pub fn run_verify(params: VerifyParams, engine: &Engine) -> Result<VerifyRun> {
    let (targets, project_path, no_project) = params.into_targets()?;
    let project_path = if no_project {
        None
    } else {
        project_path.as_deref().map(Path::new)
    };

    let total = targets.len();
    let batch_mode = total > 1;

    let plan_ctx = PlanContext {
        requested_by: EffectivePrincipal::Local(Principal {
            fingerprint: "inspect-tool".to_string(),
            scopes: vec!["bundle.read".to_string()],
        }),
        project_context: project_path
            .map(|p| ProjectContext::LocalPath {
                path: p.to_path_buf(),
            })
            .unwrap_or(ProjectContext::None),
        current_site_id: "site:local".into(),
        origin_site_id: "site:local".into(),
        execution_hints: ExecutionHints::default(),
        validate_only: false,
    };

    let mut verified = Vec::new();
    let mut failed = Vec::new();
    for target in targets {
        let report = match normalize_target(&target, project_path, engine) {
            Ok(item_ref) => verify_one(item_ref, &plan_ctx, engine),
            Err(error) => VerifyReport {
                item_ref: target,
                kind: String::new(),
                resolved_path: String::new(),
                space: String::new(),
                content_hash: String::new(),
                trust_class: String::new(),
                status: "FAILED".into(),
                error: Some(format!("{error:#}")),
            },
        };
        if report.error.is_none() {
            verified.push(report);
        } else {
            failed.push(report);
        }
    }

    let failed_count = failed.len();
    let report = if batch_mode {
        serde_json::to_value(VerifyBatchReport {
            status: if failed_count == 0 {
                "SUCCESS".into()
            } else {
                "VERIFICATION_FAILED".into()
            },
            verified,
            failed,
        })?
    } else {
        serde_json::to_value(
            verified
                .into_iter()
                .chain(failed)
                .next()
                .expect("nonempty verify targets produce one report"),
        )?
    };

    Ok(VerifyRun {
        report,
        failed: failed_count,
        total,
    })
}

fn verify_one(item_ref: String, plan_ctx: &PlanContext, engine: &Engine) -> VerifyReport {
    let canonical_ref = match CanonicalRef::parse(&item_ref) {
        Ok(canonical_ref) => canonical_ref,
        Err(error) => {
            return VerifyReport {
                item_ref,
                kind: String::new(),
                resolved_path: String::new(),
                space: String::new(),
                content_hash: String::new(),
                trust_class: String::new(),
                status: "FAILED".into(),
                error: Some(format!("failed to parse item ref: {error}")),
            };
        }
    };

    let resolved = match engine.resolve(plan_ctx, &canonical_ref) {
        Ok(item) => item,
        Err(e) => {
            return VerifyReport {
                item_ref,
                kind: canonical_ref.kind.clone(),
                resolved_path: String::new(),
                space: String::new(),
                content_hash: String::new(),
                trust_class: String::new(),
                status: "FAILED".into(),
                error: Some(format!("{e}")),
            };
        }
    };

    let resolved_path = resolved.source_path.display().to_string();
    let space = resolved.source_space.as_str().to_string();
    let content_hash = resolved.content_hash.clone();
    let kind = resolved.kind.clone();

    match engine.verify(plan_ctx, resolved) {
        Ok(verified) => {
            let trust_class = match verified.trust_class {
                TrustClass::Trusted => "TRUSTED",
                TrustClass::Untrusted => "UNTRUSTED",
                TrustClass::Unsigned => "UNSIGNED",
            };
            VerifyReport {
                item_ref,
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
            item_ref,
            kind,
            resolved_path,
            space,
            content_hash,
            trust_class: String::new(),
            status: "VERIFICATION_FAILED".into(),
            error: Some(format!("{e}")),
        },
    }
}

fn normalize_target(target: &str, project_path: Option<&Path>, engine: &Engine) -> Result<String> {
    if CanonicalRef::parse(target).is_ok() {
        return Ok(target.to_string());
    }

    let project_path = project_path.ok_or_else(|| {
        anyhow!("verify path `{target}` requires a project; pass --project <DIR>")
    })?;
    let project_root = project_path.canonicalize().map_err(|error| {
        anyhow!(
            "canonicalize project root {}: {error}",
            project_path.display()
        )
    })?;
    let target_path = PathBuf::from(target);
    let target_path = if target_path.is_absolute() {
        target_path
    } else {
        project_root.join(target_path)
    };
    let target_path = target_path
        .canonicalize()
        .map_err(|error| anyhow!("canonicalize verify path `{target}`: {error}"))?;
    let ai_root = project_root
        .join(ryeos_engine::AI_DIR)
        .canonicalize()
        .map_err(|error| {
            anyhow!(
                "canonicalize project item root {}: {error}",
                project_root.join(ryeos_engine::AI_DIR).display()
            )
        })?;
    let relative = target_path.strip_prefix(&ai_root).map_err(|_| {
        anyhow!(
            "verify path `{target}` is outside the project item root {}",
            ai_root.display()
        )
    })?;
    let relative = relative.to_string_lossy().replace('\\', "/");

    for kind in engine.kinds.kinds() {
        let Some(directory) = engine.kinds.directory(kind) else {
            continue;
        };
        let Some(rest) = relative.strip_prefix(&format!("{directory}/")) else {
            continue;
        };
        for extension in engine.kinds.extension_strs(kind).unwrap_or_default() {
            if let Some(bare_id) = rest.strip_suffix(extension) {
                return Ok(format!("{kind}:{bare_id}"));
            }
        }
    }

    Err(anyhow!(
        "verify path `{target}` does not map to a registered RyeOS item kind"
    ))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use ryeos_engine::handlers::HandlerRegistry;
    use ryeos_engine::kind_registry::KindRegistry;
    use ryeos_engine::parsers::{ParserDispatcher, ParserRegistry};

    fn empty_engine() -> Engine {
        Engine::new(
            KindRegistry::empty(),
            ParserDispatcher::new(ParserRegistry::empty(), Arc::new(HandlerRegistry::empty())),
            Vec::new(),
        )
    }

    #[test]
    fn verify_params_accept_legacy_single_and_scalar_or_array_item_refs() {
        let legacy: VerifyParams = serde_json::from_value(serde_json::json!({
            "item_ref": "tool:one"
        }))
        .unwrap();
        assert_eq!(legacy.into_targets().unwrap().0, ["tool:one"]);

        let scalar: VerifyParams = serde_json::from_value(serde_json::json!({
            "item_refs": "tool:two"
        }))
        .unwrap();
        assert_eq!(scalar.into_targets().unwrap().0, ["tool:two"]);

        let batch: VerifyParams = serde_json::from_value(serde_json::json!({
            "item_refs": ["tool:three", ".ai/tools/four.yaml"]
        }))
        .unwrap();
        assert_eq!(
            batch.into_targets().unwrap().0,
            ["tool:three", ".ai/tools/four.yaml"]
        );
    }

    #[test]
    fn one_target_preserves_the_legacy_report_shape() {
        let run = run_verify(
            VerifyParams {
                item_refs: vec!["tool:missing".into()],
                item_ref: None,
                project_path: None,
                no_project: true,
            },
            &empty_engine(),
        )
        .unwrap();

        assert_eq!(run.total, 1);
        assert_eq!(run.failed, 1);
        assert_eq!(run.report["item_ref"], "tool:missing");
        assert!(run.report.get("failed").is_none());
    }

    #[test]
    fn multiple_targets_return_an_ordered_batch_report() {
        let run = run_verify(
            VerifyParams {
                item_refs: vec!["tool:first".into(), "graph:second".into()],
                item_ref: None,
                project_path: None,
                no_project: true,
            },
            &empty_engine(),
        )
        .unwrap();

        assert_eq!(run.total, 2);
        assert_eq!(run.failed, 2);
        assert_eq!(run.report["status"], "VERIFICATION_FAILED");
        assert_eq!(run.report["failed"][0]["item_ref"], "tool:first");
        assert_eq!(run.report["failed"][1]["item_ref"], "graph:second");
        assert_eq!(run.report["verified"], serde_json::json!([]));
    }

    #[test]
    fn canonical_target_does_not_require_a_project_but_path_does() {
        let engine = empty_engine();
        assert_eq!(
            normalize_target("tool:one", None, &engine).unwrap(),
            "tool:one"
        );
        let error = normalize_target(".ai/tools/one.yaml", None, &engine).unwrap_err();
        assert!(error.to_string().contains("requires a project"));
    }
}

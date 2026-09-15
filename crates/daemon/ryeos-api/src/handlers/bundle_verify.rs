//! Node-owned bundle preflight using the already admitted engine generation.
//!
//! This is not a workload execution or another node bootstrap. The registered
//! context stays with the service; preflight's parser subprocesses still use
//! its exact isolation runtime. Never expose node keys/state to an offline
//! Tool merely so it can recreate this context.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::handler_context::HandlerContext;
use crate::registry::ServiceDescriptor;
use ryeos_app::state::AppState;
use ryeos_engine::item_resolution::RegisteredBundleRoot;
use ryeos_executor::executor::ServiceAvailability;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub source: PathBuf,
    #[serde(default)]
    pub registry_root: Option<PathBuf>,
    #[serde(default)]
    pub registry_roots: Vec<PathBuf>,
}

impl Request {
    fn validate(&self) -> Result<()> {
        if !self.source.is_absolute() {
            bail!("bundle verification source must be an absolute path");
        }
        if self.registry_root.is_some() && !self.registry_roots.is_empty() {
            bail!("select registry_root or registry_roots, not both");
        }
        for root in self.registry_root.iter().chain(self.registry_roots.iter()) {
            if !root.is_absolute() {
                bail!("bundle verification registry roots must be absolute paths");
            }
        }
        Ok(())
    }
}

pub async fn handle(
    request: Request,
    context: HandlerContext,
    state: Arc<AppState>,
) -> Result<Value> {
    // This endpoint inspects caller-selected host paths. Keep the historical
    // local-operator boundary even though normal service transport is used.
    // A remote operator or workload grant does not authorize arbitrary node
    // filesystem inspection. Admission must precede every path observation.
    ryeos_app::operator_authority::require_local_configured_operator(&state, &context)
        .context("bundle verification requires the configured local operator")?;
    request.validate()?;
    tokio::task::spawn_blocking(move || {
        state.engine.with_checked_bundle_generation(|_| {
            let registered = state
                .isolation
                .registered_generation_roots()
                .context("bundle verification requires a retained registered generation")?;
            let source = canonical_directory(&request.source)?;
            let name = source_bundle_name(&source)?;
            let dependencies = dependency_roots(&request, &source, name.as_deref(), registered)?;
            let report = ryeos_bundle::preflight::preflight_verify_bundle_report_in_context(
                &source,
                &dependencies,
                &state.config.runtime_root().config(),
                Arc::clone(&state.isolation),
            )
            .context("bundle verify failed")?;
            let warnings: Vec<Value> = report
                .warnings
                .iter()
                .map(|warning| {
                    json!({
                        "item_path": warning.item_path,
                        "severity": "warning",
                        "code": warning.code.to_string(),
                        "path": warning.path,
                        "expected": warning.expected,
                        "found": warning.found,
                    })
                })
                .collect();
            Ok(json!({
                "source": source,
                "status": "verified",
                "detail": "all items pass signature, metadata, and applicable contract validation",
                "warnings": warnings,
            }))
        })
    })
    .await
    .context("bundle verification worker stopped")?
}

fn canonical_directory(path: &Path) -> Result<PathBuf> {
    let canonical = lillux::canonicalize_existing_path(path)
        .with_context(|| format!("resolve bundle verification directory {}", path.display()))?;
    lillux::PinnedDirectory::open(&canonical)?.with_context(|| {
        format!(
            "bundle verification directory disappeared: {}",
            path.display()
        )
    })?;
    Ok(canonical)
}

fn source_bundle_name(source: &Path) -> Result<Option<String>> {
    let root = lillux::PinnedDirectory::open(source)?.context("bundle source disappeared")?;
    let ai = root
        .open_child_directory(ryeos_engine::AI_DIR.as_ref())?
        .context("bundle source has no control directory")?;
    let Some(manifest) = ai.open_pinned_regular("manifest.source.yaml".as_ref(), false)? else {
        return Ok(None);
    };
    // This name selects which installed dependency is shadowed by the
    // candidate. It grants no trust: preflight verifies the actual manifest
    // and every selected item under the retained node trust.
    let bytes = manifest.read_stable_bounded(
        &manifest.observation()?,
        ryeos_engine::item_resolution::MAX_ITEM_SOURCE_BYTES,
    )?;
    let source: ryeos_bundle::manifest::BundleManifestSource =
        serde_yaml::from_slice(&bytes).context("parse bundle manifest source")?;
    Ok(Some(source.name))
}

fn dependency_roots(
    request: &Request,
    source: &Path,
    source_name: Option<&str>,
    registered: &[RegisteredBundleRoot],
) -> Result<Vec<PathBuf>> {
    let explicit = request
        .registry_root
        .iter()
        .chain(request.registry_roots.iter());
    let mut selected = Vec::new();
    if request.registry_root.is_some() || !request.registry_roots.is_empty() {
        for root in explicit {
            let root = canonical_directory(root)?;
            if root != source && !selected.contains(&root) {
                selected.push(root);
            }
        }
    } else {
        selected.extend(
            registered
                .iter()
                .filter(|record| {
                    record.canonical_root != source && Some(record.name.as_str()) != source_name
                })
                .map(|record| record.canonical_root.clone()),
        );
    }
    Ok(selected)
}

pub const DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:bundle/verify",
    endpoint: "bundle.verify",
    availability: ServiceAvailability::Both,
    required_caps: &["ryeos.execute.service.bundle/verify"],
    handler: |params, context, state| {
        Box::pin(async move {
            let request = crate::handler_error::parse_request(params)?;
            handle(request, context, state).await
        })
    },
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_refuses_relative_and_ambiguous_host_paths() {
        for value in [
            json!({"source":"relative"}),
            json!({"source":"/source","registry_root":"relative"}),
            json!({"source":"/source","registry_roots":["relative"]}),
            json!({"source":"/source","registry_root":"/one","registry_roots":["/two"]}),
        ] {
            let request: Request = serde_json::from_value(value).unwrap();
            assert!(request.validate().is_err());
        }
        assert!(
            serde_json::from_value::<Request>(json!({"source":"/source","isolation":"disabled"}))
                .is_err()
        );
    }

    #[test]
    fn registered_dependency_selection_keeps_order_and_excludes_candidate_identity() {
        let request: Request = serde_json::from_value(json!({"source":"/candidate"})).unwrap();
        let registered = [
            RegisteredBundleRoot {
                name: "core".into(),
                canonical_root: "/core".into(),
            },
            RegisteredBundleRoot {
                name: "candidate".into(),
                canonical_root: "/installed-old".into(),
            },
            RegisteredBundleRoot {
                name: "same-path".into(),
                canonical_root: "/candidate".into(),
            },
            RegisteredBundleRoot {
                name: "standard".into(),
                canonical_root: "/standard".into(),
            },
        ];
        assert_eq!(
            dependency_roots(
                &request,
                Path::new("/candidate"),
                Some("candidate"),
                &registered
            )
            .unwrap(),
            [PathBuf::from("/core"), PathBuf::from("/standard")]
        );
    }

    #[test]
    fn explicit_dependencies_are_canonical_deduplicated_and_do_not_add_installed_roots() {
        let tmp = tempfile::tempdir().unwrap();
        let source = tmp.path().join("source");
        let dependency = tmp.path().join("dependency");
        std::fs::create_dir(&source).unwrap();
        std::fs::create_dir(&dependency).unwrap();
        let request: Request = serde_json::from_value(json!({
            "source":source,
            "registry_roots":[source,dependency,dependency],
        }))
        .unwrap();
        let registered = [RegisteredBundleRoot {
            name: "unused".into(),
            canonical_root: "/not-selected".into(),
        }];
        assert_eq!(
            dependency_roots(&request, &source, None, &registered).unwrap(),
            [dependency]
        );
    }
}

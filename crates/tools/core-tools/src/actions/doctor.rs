//! `ryeos doctor` — offline project/bundle preflight checklist.
//!
//! Composes the deterministic checks the other build-UX work added, run
//! entirely offline (no daemon): manifest present + name-consistent, bundle
//! verify (headers / parsers / signatures), and a python-tool import dry-run
//! (the SAME shared probe `tool/env-check` uses, via an offline engine). The
//! advisory checks report `unknown` rather than a false green.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::Serialize;
use serde_json::{json, Value};

use ryeos_engine::canonical_ref::CanonicalRef;
use ryeos_engine::contracts::{EffectivePrincipal, PlanContext, Principal, ProjectContext};
use ryeos_engine::engine::Engine;
use ryeos_engine::sandbox::{
    SandboxLaunchContext, SandboxProjectAuthority, SandboxRuntime, SandboxVerifiedCode,
};

/// Status of a single doctor check.
pub const OK: &str = "ok";
pub const FAIL: &str = "fail";
/// Non-OK but non-blocking — a real risk a human/script should see, that the
/// deterministic `ok` verdict does not fail on (e.g. a name/directory mismatch
/// that publish, not doctor, is the authority for).
pub const WARN: &str = "warning";
pub const UNKNOWN: &str = "unknown";
pub const NA: &str = "n/a";

#[derive(Debug, Serialize)]
pub struct CheckResult {
    pub name: String,
    pub status: String,
    pub detail: Value,
}

impl CheckResult {
    fn new(name: &str, status: &str, detail: Value) -> Self {
        Self {
            name: name.to_string(),
            status: status.to_string(),
            detail,
        }
    }
}

#[derive(Debug, Serialize)]
pub struct DoctorReport {
    pub source: PathBuf,
    /// `false` if any check is `fail`. Advisory `unknown` checks never fail it.
    pub ok: bool,
    pub checks: Vec<CheckResult>,
}

/// Run the full offline preflight checklist over `source`.
///
/// `engine` is `Err(reason)` when an offline engine could not be built; the
/// static checks still run and the import check reports `unavailable` rather
/// than the whole command failing. `sandbox` follows the same rule when the
/// immutable node policy snapshot cannot be loaded.
pub fn run_doctor(
    engine: Result<&Engine, &str>,
    sandbox: Result<Arc<SandboxRuntime>, &str>,
    source: &Path,
    dependency_roots: &[PathBuf],
    operator_config_root: &Path,
) -> DoctorReport {
    let mut checks = vec![
        check_manifest(source),
        check_verify(
            source,
            dependency_roots,
            operator_config_root,
            sandbox.as_ref().map(Arc::clone).map_err(|reason| *reason),
        ),
    ];
    checks.extend(check_imports(
        engine,
        sandbox.as_deref().map_err(|reason| *reason),
        source,
        operator_config_root,
    ));
    checks.push(advisory_bundle_events(source));

    let ok = checks.iter().all(|c| c.status != FAIL);
    DoctorReport {
        source: source.to_path_buf(),
        ok,
        checks,
    }
}

/// Manifest present + name-consistent. Signatures are covered by `verify`.
fn check_manifest(source: &Path) -> CheckResult {
    let ai_dir = source.join(ryeos_engine::AI_DIR);
    let source_path = ai_dir.join("manifest.source.yaml");
    match std::fs::symlink_metadata(&source_path) {
        Ok(metadata) if metadata.file_type().is_file() && !metadata.file_type().is_symlink() => {}
        Ok(_) => {
            return CheckResult::new(
                "manifest",
                FAIL,
                json!({
                    "error": ".ai/manifest.source.yaml must be a regular non-symlink file",
                }),
            );
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return CheckResult::new(
                "manifest",
                FAIL,
                json!({
                    "error": "required .ai/manifest.source.yaml is missing",
                    "remedy": "add the bundle manifest source, then run `ryeos bundle publish`",
                }),
            );
        }
        Err(error) => {
            return CheckResult::new(
                "manifest",
                FAIL,
                json!({ "error": format!("cannot inspect manifest.source.yaml: {error}") }),
            );
        }
    }
    let raw = match std::fs::read_to_string(&source_path) {
        Ok(raw) => raw,
        Err(e) => {
            return CheckResult::new(
                "manifest",
                FAIL,
                json!({ "error": format!("manifest.source.yaml is unreadable: {e}") }),
            );
        }
    };
    let source_manifest = match serde_yaml::from_str::<ryeos_bundle::manifest::BundleManifestSource>(
        &raw,
    ) {
        Ok(src) => src,
        Err(e) => {
            let msg = e.to_string();
            // `deny_unknown_fields` rejects a manifest still carrying the flat,
            // pre-nesting runtime-authority fields. Name the exact fix instead
            // of swallowing the error behind a generic "malformed" string.
            let old_shape_field = ["bundle_events", "runtime_vault", "item_authoring"]
                .into_iter()
                .find(|f| msg.contains(&format!("unknown field `{f}`")));
            if let Some(field) = old_shape_field {
                return CheckResult::new(
                    "manifest",
                    FAIL,
                    json!({
                        "error": format!(
                            "manifest.source.yaml declares `{field}` as a top-level field"
                        ),
                        "remedy": "nest bundle_events / runtime_vault / item_authoring under a single `runtime_authority:` block",
                        "serde_error": msg,
                    }),
                );
            }
            return CheckResult::new(
                "manifest",
                FAIL,
                json!({ "error": format!("manifest.source.yaml is malformed: {msg}") }),
            );
        }
    };
    let declared_name = source_manifest.name.clone();
    let declares_runtime_authority = !source_manifest.runtime_authority.is_empty();

    let generated = ai_dir.join("manifest.yaml");
    match std::fs::symlink_metadata(&generated) {
        Ok(metadata) if metadata.file_type().is_file() && !metadata.file_type().is_symlink() => {}
        Ok(_) => {
            return CheckResult::new(
                "manifest",
                FAIL,
                json!({ "error": ".ai/manifest.yaml must be a regular non-symlink file" }),
            );
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return CheckResult::new(
                "manifest",
                FAIL,
                json!({
                    "error": "manifest.source.yaml present but .ai/manifest.yaml is not generated",
                    "remedy": "run `ryeos bundle manifest-sign` (or `bundle publish`)",
                    "declared_name": declared_name,
                }),
            );
        }
        Err(error) => {
            return CheckResult::new(
                "manifest",
                FAIL,
                json!({ "error": format!("cannot inspect manifest.yaml: {error}") }),
            );
        }
    }
    let dir_name = source
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default();
    let name_consistent = declared_name == dir_name;
    // A name/directory mismatch is non-OK so a human/script can see it, but not
    // a hard FAIL: the directory is only a heuristic for the effective bundle
    // id, and publish is the authority that hard-fails a real item-level
    // mismatch (especially for runtime-authority bundles, where the daemon
    // rejects cap minting).
    let (status, note) = if name_consistent {
        (OK, Value::Null)
    } else if declares_runtime_authority {
        (
            WARN,
            json!("manifest name differs from the directory AND this bundle declares runtime authority — if the items' effective bundle id does not equal the manifest name, the daemon will reject runtime-cap minting. Verify with `ryeos bundle publish` (it hard-fails a real mismatch)."),
        )
    } else {
        (
            WARN,
            json!("manifest name differs from the directory; pass --name on publish if the effective bundle id differs"),
        )
    };
    CheckResult::new(
        "manifest",
        status,
        json!({
            "declared_name": declared_name,
            "directory": dir_name,
            "name_matches_directory": name_consistent,
            "declares_runtime_authority": declares_runtime_authority,
            "note": note,
        }),
    )
}

/// Bundle verify — headers, parsers, and signatures of every item.
fn check_verify(
    source: &Path,
    dependency_roots: &[PathBuf],
    operator_config_root: &Path,
    sandbox: Result<Arc<SandboxRuntime>, &str>,
) -> CheckResult {
    let sandbox = match sandbox {
        Ok(sandbox) => sandbox,
        Err(reason) => {
            return CheckResult::new(
                "verify",
                FAIL,
                json!({ "error": format!("sandbox policy unavailable: {reason}") }),
            )
        }
    };
    match ryeos_bundle::preflight::preflight_verify_bundle_report_in_context(
        source,
        dependency_roots,
        operator_config_root,
        sandbox,
    ) {
        Ok(report) => {
            let warnings: Vec<String> = report.warnings.iter().map(|w| format!("{w:?}")).collect();
            CheckResult::new("verify", OK, json!({ "warnings": warnings }))
        }
        Err(e) => CheckResult::new("verify", FAIL, json!({ "error": format!("{e:#}") })),
    }
}

/// Python-tool import dry-run via the shared probe over an offline engine.
fn check_imports(
    engine: Result<&Engine, &str>,
    sandbox: Result<&SandboxRuntime, &str>,
    source: &Path,
    operator_config_root: &Path,
) -> Vec<CheckResult> {
    let refs = python_tool_refs(source);
    if refs.is_empty() {
        return vec![CheckResult::new(
            "imports",
            NA,
            json!({ "note": "no python tools found under .ai/tools" }),
        )];
    }
    let engine = match engine {
        Ok(e) => e,
        Err(reason) => {
            return vec![CheckResult::new(
                "imports",
                NA,
                json!({
                    "import_check": "unavailable",
                    "import_check_reason": format!("offline engine unavailable: {reason}"),
                    "note": "static checks ran; python imports were not dry-run",
                }),
            )];
        }
    };
    let sandbox = match sandbox {
        Ok(sandbox) => sandbox,
        Err(reason) => {
            return vec![CheckResult::new(
                "imports",
                NA,
                json!({
                    "import_check": "unavailable",
                    "import_check_reason": format!("sandbox policy unavailable: {reason}"),
                    "note": "static checks ran; python imports were not dry-run",
                }),
            )];
        }
    };
    let project_path = match std::fs::canonicalize(source) {
        Ok(path) if path.is_dir() => path,
        Ok(path) => {
            return vec![CheckResult::new(
                "imports",
                NA,
                json!({
                    "import_check": "unavailable",
                    "import_check_reason": format!("project path is not a directory: {}", path.display()),
                    "note": "static checks ran; python imports were not dry-run",
                }),
            )];
        }
        Err(error) => {
            return vec![CheckResult::new(
                "imports",
                NA,
                json!({
                    "import_check": "unavailable",
                    "import_check_reason": format!("could not canonicalize project path {}: {error}", source.display()),
                    "note": "static checks ran; python imports were not dry-run",
                }),
            )];
        }
    };
    let sandbox_bundle_roots = engine
        .resolution_roots(Some(project_path.clone()))
        .ordered
        .iter()
        .filter(|root| root.space == ryeos_engine::contracts::ItemSpace::Bundle)
        .filter_map(|root| root.ai_root.parent().map(Path::to_path_buf))
        .collect::<Vec<_>>();
    let sandbox_node_trusted_keys_dir = operator_config_root.join("keys/trusted");

    let plan_ctx = PlanContext {
        requested_by: EffectivePrincipal::Local(Principal {
            fingerprint: "fp:doctor".into(),
            scopes: vec!["execute".into()],
        }),
        project_context: ProjectContext::LocalPath {
            path: project_path.clone(),
        },
        current_site_id: "site:doctor".into(),
        origin_site_id: "site:doctor".into(),
        execution_hints: Default::default(),
        validate_only: true,
    };

    refs.into_iter()
        .map(|item_ref| {
            let detail = match import_one(
                engine,
                sandbox,
                &plan_ctx,
                &project_path,
                &sandbox_bundle_roots,
                &sandbox_node_trusted_keys_dir,
                &item_ref,
            ) {
                Ok(report) => report,
                Err(e) => json!({ "import_check": "unavailable", "import_check_reason": e }),
            };
            // A python tool whose import fails is a hard `fail`; anything else
            // (n/a, unavailable) is non-blocking.
            let status = match detail.get("import_check").and_then(Value::as_str) {
                Some("python_function") => {
                    if detail.get("import_ok").and_then(Value::as_bool) == Some(true) {
                        OK
                    } else {
                        FAIL
                    }
                }
                _ => NA,
            };
            CheckResult::new(&format!("import:{item_ref}"), status, detail)
        })
        .collect()
}

fn import_one(
    engine: &Engine,
    sandbox: &SandboxRuntime,
    plan_ctx: &PlanContext,
    project_path: &Path,
    sandbox_bundle_roots: &[PathBuf],
    sandbox_node_trusted_keys_dir: &Path,
    item_ref: &str,
) -> Result<Value, String> {
    let canonical = CanonicalRef::parse(item_ref).map_err(|e| format!("invalid ref: {e}"))?;
    let resolved = engine
        .resolve(plan_ctx, &canonical)
        .map_err(|e| format!("resolve: {e}"))?;
    let verified = engine
        .verify(plan_ctx, resolved)
        .map_err(|e| format!("verify: {e}"))?;
    let sandbox_verified_code = [SandboxVerifiedCode {
        source_path: verified.resolved.source_path.clone(),
        content_hash: verified.resolved.content_hash.clone(),
    }];
    Ok(ryeos_app::env_probe::import_dry_run(
        engine,
        plan_ctx,
        &verified,
        &[],
        sandbox,
        SandboxLaunchContext {
            project_path,
            project_authority: SandboxProjectAuthority::External,
            state_root: None,
            checkpoint_dir: None,
            daemon_socket_path: None,
            bundle_roots: sandbox_bundle_roots,
            node_trusted_keys_dir: Some(sandbox_node_trusted_keys_dir),
            verified_code: &sandbox_verified_code,
            item_ref,
            thread_id: "offline-doctor",
        },
    ))
}

/// Refs of the python tools in `source/.ai/tools` (`tool:<bare-id>`), skipping
/// `lib/` support modules.
fn python_tool_refs(source: &Path) -> Vec<String> {
    let tools_dir = source.join(ryeos_engine::AI_DIR).join("tools");
    let mut refs = Vec::new();
    collect_py(&tools_dir, &tools_dir, &mut refs);
    refs.sort();
    refs
}

fn collect_py(dir: &Path, tools_root: &Path, out: &mut Vec<String>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if path.file_name().and_then(|n| n.to_str()) == Some("lib") {
                continue;
            }
            collect_py(&path, tools_root, out);
        } else if path.extension().and_then(|e| e.to_str()) == Some("py") {
            if let Ok(rel) = path.strip_prefix(tools_root) {
                let bare = rel.with_extension("");
                out.push(format!("tool:{}", bare.to_string_lossy()));
            }
        }
    }
}

/// Advisory: whether declared `runtime_authority.bundle_events:` cover the kinds
/// tools actually use is inherently best-effort, so report `unknown` — never a
/// green pass.
fn advisory_bundle_events(source: &Path) -> CheckResult {
    let declared = std::fs::read_to_string(
        source
            .join(ryeos_engine::AI_DIR)
            .join("manifest.source.yaml"),
    )
    .ok()
    .and_then(|raw| serde_yaml::from_str::<ryeos_bundle::manifest::BundleManifestSource>(&raw).ok())
    .map(|src| {
        src.runtime_authority
            .bundle_events
            .iter()
            .map(|e| e.event_kind.clone())
            .collect::<Vec<_>>()
    })
    .unwrap_or_default();
    CheckResult::new(
        "bundle_events_coverage",
        UNKNOWN,
        json!({
            "declared_event_kinds": declared,
            "note": "whether declared bundle_events cover the kinds tools actually append/scan is not determined statically; verify against tool source",
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &Path, body: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, body).unwrap();
    }

    #[test]
    fn python_tool_refs_skips_lib_and_derives_refs() {
        let tmp = tempfile::tempdir().unwrap();
        write(&tmp.path().join(".ai/tools/arc/play.py"), "x=1\n");
        write(&tmp.path().join(".ai/tools/arc/solve.py"), "x=1\n");
        write(&tmp.path().join(".ai/tools/arc/lib/helper.py"), "x=1\n");
        let refs = python_tool_refs(tmp.path());
        assert_eq!(refs, vec!["tool:arc/play", "tool:arc/solve"]);
    }

    #[test]
    fn manifest_check_fails_when_no_source() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".ai")).unwrap();
        let r = check_manifest(tmp.path());
        assert_eq!(r.status, FAIL);
        assert!(r.detail["error"]
            .as_str()
            .is_some_and(|error| error.contains("required")));
    }

    #[test]
    fn manifest_check_fails_when_source_but_no_generated() {
        let tmp = tempfile::tempdir().unwrap();
        write(
            &tmp.path().join(".ai/manifest.source.yaml"),
            "name: arc\nversion: \"0.1.0\"\n",
        );
        let r = check_manifest(tmp.path());
        assert_eq!(r.status, FAIL, "{:?}", r.detail);
        assert!(r.detail["remedy"]
            .as_str()
            .unwrap()
            .contains("manifest-sign"));
    }

    #[test]
    fn manifest_check_hints_old_shape_runtime_authority_fields() {
        let tmp = tempfile::tempdir().unwrap();
        // A manifest still carrying the flat pre-nesting field. `deny_unknown_fields`
        // rejects it; doctor must name the field and the nesting fix, not a
        // generic "malformed" string.
        write(
            &tmp.path().join(".ai/manifest.source.yaml"),
            "name: arc\nversion: \"0.1.0\"\nbundle_events:\n  - event_kind: ev\n    operations: [append]\n",
        );
        let r = check_manifest(tmp.path());
        assert_eq!(r.status, FAIL);
        assert!(
            r.detail["error"]
                .as_str()
                .unwrap()
                .contains("bundle_events"),
            "error should name the offending field: {:?}",
            r.detail
        );
        assert!(r.detail["remedy"]
            .as_str()
            .unwrap()
            .contains("runtime_authority:"));
        // The raw serde error is preserved for context.
        assert!(r.detail["serde_error"].is_string());
    }

    #[test]
    fn manifest_check_surfaces_raw_serde_error_for_other_malformations() {
        let tmp = tempfile::tempdir().unwrap();
        // An unknown field that is NOT one of the old-shape runtime-authority
        // fields: the raw serde error must surface rather than a generic string,
        // and no nesting remedy applies.
        write(
            &tmp.path().join(".ai/manifest.source.yaml"),
            "name: arc\nversion: \"0.1.0\"\nbogus_field: true\n",
        );
        let r = check_manifest(tmp.path());
        assert_eq!(r.status, FAIL);
        let err = r.detail["error"].as_str().unwrap();
        assert!(err.contains("malformed"), "unexpected: {err}");
        assert!(
            err.contains("bogus_field"),
            "raw serde error preserved: {err}"
        );
        assert!(r.detail.get("remedy").is_none());
    }

    #[test]
    fn manifest_check_ok_and_flags_name_mismatch() {
        let tmp = tempfile::tempdir().unwrap();
        // dir basename is a random temp name, declared name is "arc" -> mismatch noted.
        write(
            &tmp.path().join(".ai/manifest.source.yaml"),
            "name: arc\nversion: \"0.1.0\"\n",
        );
        write(&tmp.path().join(".ai/manifest.yaml"), "name: arc\n");
        let r = check_manifest(tmp.path());
        // A name/directory mismatch is a non-OK warning (not OK, not FAIL).
        assert_eq!(r.status, WARN);
        assert_eq!(r.detail["name_matches_directory"], false);
        assert!(r.detail["note"].is_string());
    }
}

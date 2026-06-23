//! `ryeos doctor` — offline project/bundle preflight checklist.
//!
//! Composes the deterministic checks the other build-UX work added, run
//! entirely offline (no daemon): manifest present + name-consistent, bundle
//! verify (headers / parsers / signatures), and a python-tool import dry-run
//! (the SAME shared probe `tool/env-check` uses, via an offline engine). The
//! advisory checks report `unknown` rather than a false green.

use std::path::{Path, PathBuf};

use serde::Serialize;
use serde_json::{json, Value};

use ryeos_engine::canonical_ref::CanonicalRef;
use ryeos_engine::contracts::{EffectivePrincipal, PlanContext, Principal, ProjectContext};
use ryeos_engine::engine::Engine;

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
/// than the whole command failing.
pub fn run_doctor(
    engine: Result<&Engine, &str>,
    source: &Path,
    dependency_roots: &[PathBuf],
    operator_config_root: &Path,
) -> DoctorReport {
    let mut checks = vec![
        check_manifest(source),
        check_verify(source, dependency_roots, operator_config_root),
    ];
    checks.extend(check_imports(engine, source));
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
    if !source_path.exists() {
        return CheckResult::new(
            "manifest",
            NA,
            json!({ "note": "no .ai/manifest.source.yaml — manifests are optional" }),
        );
    }
    let source_manifest = match std::fs::read_to_string(&source_path).ok().and_then(|raw| {
        serde_yaml::from_str::<ryeos_bundle::manifest::BundleManifestSource>(&raw).ok()
    }) {
        Some(src) => src,
        None => {
            return CheckResult::new(
                "manifest",
                FAIL,
                json!({ "error": "manifest.source.yaml is unreadable or malformed" }),
            );
        }
    };
    let declared_name = source_manifest.name.clone();
    let declares_runtime_authority =
        !source_manifest.bundle_events.is_empty() || !source_manifest.runtime_vault.is_empty();

    let generated = ai_dir.join("manifest.yaml");
    if !generated.exists() {
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
) -> CheckResult {
    match ryeos_bundle::preflight::preflight_verify_bundle_report_in_context(
        source,
        dependency_roots,
        operator_config_root,
    ) {
        Ok(report) => {
            let warnings: Vec<String> = report.warnings.iter().map(|w| format!("{w:?}")).collect();
            CheckResult::new("verify", OK, json!({ "warnings": warnings }))
        }
        Err(e) => CheckResult::new("verify", FAIL, json!({ "error": format!("{e:#}") })),
    }
}

/// Python-tool import dry-run via the shared probe over an offline engine.
fn check_imports(engine: Result<&Engine, &str>, source: &Path) -> Vec<CheckResult> {
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

    let plan_ctx = PlanContext {
        requested_by: EffectivePrincipal::Local(Principal {
            fingerprint: "fp:doctor".into(),
            scopes: vec!["execute".into()],
        }),
        project_context: ProjectContext::LocalPath {
            path: source.to_path_buf(),
        },
        current_site_id: "site:doctor".into(),
        origin_site_id: "site:doctor".into(),
        execution_hints: Default::default(),
        validate_only: true,
    };

    refs.into_iter()
        .map(|item_ref| {
            let detail = match import_one(engine, &plan_ctx, &item_ref) {
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

fn import_one(engine: &Engine, plan_ctx: &PlanContext, item_ref: &str) -> Result<Value, String> {
    let canonical = CanonicalRef::parse(item_ref).map_err(|e| format!("invalid ref: {e}"))?;
    let resolved = engine
        .resolve(plan_ctx, &canonical)
        .map_err(|e| format!("resolve: {e}"))?;
    let verified = engine
        .verify(plan_ctx, resolved)
        .map_err(|e| format!("verify: {e}"))?;
    Ok(ryeos_app::env_probe::import_dry_run(
        engine,
        plan_ctx,
        &verified,
        &[],
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

/// Advisory: whether declared `bundle_events:` cover the kinds tools actually
/// use is inherently best-effort, so report `unknown` — never a green pass.
fn advisory_bundle_events(source: &Path) -> CheckResult {
    let declared = std::fs::read_to_string(
        source
            .join(ryeos_engine::AI_DIR)
            .join("manifest.source.yaml"),
    )
    .ok()
    .and_then(|raw| serde_yaml::from_str::<ryeos_bundle::manifest::BundleManifestSource>(&raw).ok())
    .map(|src| {
        src.bundle_events
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
    fn manifest_check_na_when_no_source() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".ai")).unwrap();
        let r = check_manifest(tmp.path());
        assert_eq!(r.status, NA);
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

//! `ConfigResolveHandler` — claims the top-level `config_resolve`
//! block on a tool/runtime item.
//!
//! Mirrors Python `PrimitiveExecutor._resolve_tool_config` /
//! `_resolve_single_config` / `_deep_merge_config` (see
//! `ryeos/ryeos/executor/primitive_executor.py` lines 1125-1229) and the
//! driver wiring at lines 257-285:
//!
//!   * On the root chain element (chain[0] / current_index == 0):
//!     write the fully resolved config under
//!     `ctx.params["resolved_config"]` so the tool body receives it.
//!   * On non-root chain elements (runtime / primitive hops):
//!     extract `defaults` + per-tool overrides
//!     (`tools.<root_executor_id>`), filter to keys in
//!     (universal `{"timeout"}` ∪ this element's `execution_params`),
//!     and set in `ctx.params` only if not already present (caller
//!     wins).
//!
//! Each loaded YAML is verified with "warn-if-unsigned, fail-loud
//! on tampered" semantics (`allow_unsigned=True` in Python). The
//! YAML is parsed via the `config` kind's parser dispatch entry.

use std::collections::BTreeSet;
use std::path::Path;

use serde::Deserialize;
use serde_json::{Map, Value};

use crate::contracts::ItemSpace;
use crate::error::EngineError;
use crate::item_resolution::parse_signature_header;
use crate::runtime::{CompileContext, RuntimeHandler};
use crate::trust::{content_hash_after_signature, verify_item_signature};

pub const KEY: &str = "config_resolve";

/// Universal execution-config keys allowed on every runtime element,
/// regardless of its declared `execution_params`.
const UNIVERSAL_EXEC_KEYS: &[&str] = &["timeout"];

/// One config spec — relative path under `.ai/config/` plus a
/// resolution mode. `#[serde(deny_unknown_fields)]` so typos surface
/// as hard errors rather than silent no-ops.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfigSpec {
    /// Relative path under `.ai/config/`, e.g. `"execution/execution.yaml"`.
    pub path: String,
    #[serde(default)]
    pub mode: ResolveMode,
}

/// Resolution strategy for a single `ConfigSpec`.
#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum ResolveMode {
    /// system → user → project; deep-merge each layer (later wins).
    /// `extends` is excluded from merging (Python parity).
    #[default]
    DeepMerge,
    /// project → user → system; first existing file wins, returned
    /// as-is.
    FirstMatch,
}

/// `config_resolve` block accepts either a single spec or a list of
/// specs (Python parity — `_resolve_tool_config` switches on type).
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ConfigResolveSpec {
    Single { spec: ConfigSpec },
    Multi { specs: Vec<ConfigSpec> },
}

pub struct ConfigResolveHandler;

impl RuntimeHandler for ConfigResolveHandler {
    fn key(&self) -> &'static str {
        KEY
    }

    fn phase(&self) -> crate::runtime::HandlerPhase {
        crate::runtime::HandlerPhase::ResolveContext
    }

    fn cardinality(&self) -> crate::runtime::HandlerCardinality {
        // Each chain element resolves its own config; chain[0] sets
        // `resolved_config`, chain[1..] inject execution overrides.
        crate::runtime::HandlerCardinality::All
    }

    #[tracing::instrument(
        name = "engine:config_resolve",
        skip(self, block, ctx),
        fields(
            item_ref = %ctx.chain[ctx.current_index].resolved_ref,
            chain_index = ctx.current_index,
        )
    )]
    fn apply(&self, block: &Value, ctx: &mut CompileContext<'_>) -> Result<(), EngineError> {
        let intermediate = &ctx.chain[ctx.current_index];
        let spec: ConfigResolveSpec =
            serde_json::from_value(block.clone()).map_err(|e| EngineError::InvalidRuntimeConfig {
                path: intermediate.source_path.display().to_string(),
                reason: format!("invalid config_resolve: {e}"),
            })?;

        // Resolve each declared spec into a JSON Value. A list-form
        // returns `{path: resolved}`; a single-form returns the
        // resolved config directly. Matches Python lines 1138-1146.
        let resolved: Value = match &spec {
            ConfigResolveSpec::Multi { specs } => {
                let mut map = Map::new();
                for s in specs {
                    let r = resolve_single(s, ctx)?;
                    map.insert(s.path.clone(), r);
                }
                Value::Object(map)
            }
            ConfigResolveSpec::Single { spec: s } => resolve_single(s, ctx)?,
        };

        // Driver wiring (Python primitive_executor.py:257-285).
        if ctx.current_index == 0 {
            // Root tool: hand the resolved config straight to the
            // tool body via parameters["resolved_config"].
            insert_param(&mut ctx.params, "resolved_config", resolved);
        } else {
            // Runtime / primitive element: extract execution overrides
            // and conditionally inject into params.
            //
            // Python uses `chain[0].item_id` — in this engine that is
            // `executor_id` on the root ChainIntermediate (the same
            // identifier used to look up the tool through resolution).
            let root_tool_id = ctx
                .chain
                .first()
                .map(|c| c.executor_id.as_str())
                .unwrap_or("");

            let exec_defaults = resolved
                .get("defaults")
                .and_then(Value::as_object)
                .cloned()
                .unwrap_or_default();
            let tool_overrides = resolved
                .get("tools")
                .and_then(|t| t.get(root_tool_id))
                .and_then(Value::as_object)
                .cloned()
                .unwrap_or_default();

            // Merge defaults + per-tool overrides (per-tool wins).
            let mut exec_config = exec_defaults;
            for (k, v) in tool_overrides {
                exec_config.insert(k, v);
            }

            // Allowed keys = universal ∪ this element's
            // `execution_params`. Shape is type-validated by
            // `ExecutionParamsHandler` in the `ValidateInput`
            // phase before this handler runs, so the shared
            // `parse_execution_params` helper here will only fail
            // on a genuine engine bug (e.g. handler ordering
            // regression).
            let mut allowed: BTreeSet<String> =
                UNIVERSAL_EXEC_KEYS.iter().map(|s| (*s).to_owned()).collect();
            if let Some(raw) = intermediate.parsed.get("execution_params") {
                let list = crate::runtime::handlers::execution_params::parse_execution_params(
                    raw,
                    &intermediate.source_path,
                )?;
                for s in list {
                    allowed.insert(s);
                }
            }

            for (ek, ev) in exec_config {
                if !allowed.contains(&ek) {
                    continue;
                }
                if param_already_present(&ctx.params, &ek) {
                    continue;
                }
                insert_param(&mut ctx.params, &ek, ev);
            }
        }

        Ok(())
    }
}

// ── Helpers ──────────────────────────────────────────────────────────────

fn insert_param(params: &mut Value, key: &str, value: Value) {
    if !params.is_object() {
        *params = Value::Object(Map::new());
    }
    if let Some(map) = params.as_object_mut() {
        map.insert(key.to_owned(), value);
    }
}

fn param_already_present(params: &Value, key: &str) -> bool {
    params
        .as_object()
        .map(|m| m.contains_key(key))
        .unwrap_or(false)
}

/// Resolve one `ConfigSpec` against the full set of resolution roots.
fn resolve_single(spec: &ConfigSpec, ctx: &CompileContext<'_>) -> Result<Value, EngineError> {
    match spec.mode {
        ResolveMode::DeepMerge => {
            // system(s) → user → project (lowest to highest priority).
            // `roots.ordered` is already in that order.
            let mut merged = Value::Object(Map::new());
            for root in &ctx.roots.ordered {
                let candidate = root.ai_root.join("config").join(&spec.path);
                if candidate.exists() {
                    let layer = load_and_verify_config_file(&candidate, ctx)?;
                    merged = deep_merge(merged, layer);
                }
            }
            Ok(merged)
        }
        ResolveMode::FirstMatch => {
            // project → user → system. Reverse-walk roots so
            // project wins. `ordered` is system-first, so we
            // partition by space then check project, user, system in
            // turn.
            for target in &[ItemSpace::Project, ItemSpace::User, ItemSpace::System] {
                for root in ctx.roots.ordered.iter().filter(|r| r.space == *target) {
                    let candidate = root.ai_root.join("config").join(&spec.path);
                    if candidate.exists() {
                        return load_and_verify_config_file(&candidate, ctx);
                    }
                }
            }
            Ok(Value::Object(Map::new()))
        }
    }
}

/// Read, verify (warn-if-unsigned, fail-loud on tampered), and parse
/// a config YAML.
fn load_and_verify_config_file(
    path: &Path,
    ctx: &CompileContext<'_>,
) -> Result<Value, EngineError> {
    let content = std::fs::read_to_string(path).map_err(|e| EngineError::InvalidRuntimeConfig {
        path: path.display().to_string(),
        reason: format!("could not read config file: {e}"),
    })?;

    // Look up the `config` kind's `.yaml` extension spec for the
    // signature envelope + parser ref.
    let kind_schema = ctx
        .kinds
        .get("config")
        .ok_or_else(|| EngineError::InvalidRuntimeConfig {
            path: path.display().to_string(),
            reason: "config kind not registered — required for config_resolve".to_string(),
        })?;
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| format!(".{e}"))
        .unwrap_or_else(|| ".yaml".to_owned());
    let ext_spec = kind_schema
        .spec_for(&ext)
        .or_else(|| kind_schema.spec_for(".yaml"))
        .ok_or_else(|| EngineError::InvalidRuntimeConfig {
            path: path.display().to_string(),
            reason: format!("config kind has no extension spec for `{ext}`"),
        })?;
    let envelope = &ext_spec.signature;

    // Verify integrity. Mirrors `verify_item(... allow_unsigned=True)`:
    //   * No header     → warn, continue (unsigned is allowed).
    //   * Header present → recompute hash; mismatch is fatal.
    //                      Untrusted signer is logged but not fatal.
    match parse_signature_header(&content, envelope) {
        None => {
            tracing::warn!(
                config_path = %path.display(),
                "config file is unsigned (allow_unsigned=true)"
            );
        }
        Some(header) => {
            let recomputed = content_hash_after_signature(&content, envelope).ok_or_else(|| {
                EngineError::InvalidRuntimeConfig {
                    path: path.display().to_string(),
                    reason: "could not locate signature line in config file".to_string(),
                }
            })?;
            if recomputed != header.content_hash {
                return Err(EngineError::ContentHashMismatch {
                    canonical_ref: path.display().to_string(),
                    expected: header.content_hash.clone(),
                    actual: recomputed,
                });
            }
            // Also try full sig verification — trust class is
            // logged, never fatal (matches `allow_unsigned=True`).
            match verify_item_signature(&content, &header, envelope, ctx.trust_store) {
                Ok((trust, _fp)) => {
                    tracing::debug!(
                        config_path = %path.display(),
                        ?trust,
                        "config file signature verified"
                    );
                }
                Err(EngineError::ContentHashMismatch { expected, actual, .. }) => {
                    // Re-raise with the file path attached.
                    return Err(EngineError::ContentHashMismatch {
                        canonical_ref: path.display().to_string(),
                        expected,
                        actual,
                    });
                }
                Err(e) => {
                    tracing::warn!(
                        config_path = %path.display(),
                        error = %e,
                        "config file signature trust check failed (allow_unsigned=true)"
                    );
                }
            }
        }
    }

    // Parse via the engine's parser dispatcher using the config
    // kind's parser ref.
    let parsed = ctx
        .parsers
        .dispatch(&ext_spec.parser, &content, Some(path), envelope)?;

    // Match Python: `_yaml.safe_load(f) or {}` — null/empty yaml
    // becomes an empty mapping so the deep-merge stays sane.
    if parsed.is_null() {
        Ok(Value::Object(Map::new()))
    } else {
        Ok(parsed)
    }
}

/// Recursive deep merge: `override` wins. Dicts merge key-by-key;
/// scalars and lists are replaced wholesale. The `"extends"` key is
/// excluded (Python `_deep_merge_config` line 1219).
pub(crate) fn deep_merge(base: Value, override_: Value) -> Value {
    match (base, override_) {
        (Value::Object(mut b), Value::Object(o)) => {
            for (k, v) in o {
                if k == "extends" {
                    continue;
                }
                let existing = b.remove(&k);
                let merged = match existing {
                    Some(existing_val) => deep_merge(existing_val, v),
                    None => v,
                };
                b.insert(k, merged);
            }
            Value::Object(b)
        }
        // override is non-object OR base is non-object → override wins.
        (_, o) => o,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::item_resolution::{ResolutionRoot, ResolutionRoots};
    use crate::kind_registry::KindRegistry;
    use crate::parsers::test_helpers::dispatcher_with_canonical_bundle_descriptors;
    use crate::runtime::{ChainIntermediate, HostEnvBindings, SpecOverrides, TemplateContext};
    use crate::trust::TrustStore;
    use lillux::crypto::SigningKey;
    use serde_json::json;
    use std::collections::HashMap;
    use std::fs;
    use std::path::PathBuf;

    fn test_signing_key() -> SigningKey {
        SigningKey::from_bytes(&[42u8; 32])
    }

    fn test_trust_store() -> TrustStore {
        let sk = test_signing_key();
        let vk = sk.verifying_key();
        TrustStore::from_signers(vec![crate::trust::TrustedSigner {
            fingerprint: crate::trust::compute_fingerprint(&vk),
            verifying_key: vk,
            label: None,
        }])
    }

    fn tempdir(label: &str) -> PathBuf {
        use std::time::SystemTime;
        let nanos = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .subsec_nanos() as u64;
        let dir = std::env::temp_dir().join(format!(
            "rye_cfgres_{}_{}_{}",
            label,
            std::process::id(),
            nanos
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn sign_yaml(yaml: &str) -> String {
        lillux::signature::sign_content(yaml, &test_signing_key(), "#", None)
    }

    /// Write a `config` kind schema to `kinds_dir/config/config.kind-schema.yaml`.
    fn write_config_kind_schema(kinds_dir: &std::path::Path) {
        let yaml = "\
location:
  directory: config
formats:
  - extensions: [\".yaml\", \".yml\"]
    parser: parser:ryeos/core/yaml/yaml
    signature:
      prefix: \"#\"
composer: handler:ryeos/core/identity
composed_value_contract:
  root_type: mapping
  required: {}
metadata:
  rules: {}
";
        let dir = kinds_dir.join("config");
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("config.kind-schema.yaml"),
            sign_yaml(yaml),
        )
        .unwrap();
    }

    /// Create `<ai_root>/config/<rel_path>` containing signed YAML.
    fn write_signed_config(ai_root: &std::path::Path, rel_path: &str, body: &str) -> PathBuf {
        let p = ai_root.join("config").join(rel_path);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(&p, sign_yaml(body)).unwrap();
        p
    }

    /// Build a (kinds, parsers, trust, roots) test rig with three
    /// space tiers all rooted at unique tempdirs.
    struct TestRig {
        kinds: KindRegistry,
        parsers: crate::parsers::dispatcher::ParserDispatcher,
        trust: TrustStore,
        roots: ResolutionRoots,
        system_ai: PathBuf,
        user_ai: PathBuf,
        project_ai: PathBuf,
    }

    fn build_rig() -> TestRig {
        let trust = test_trust_store();

        // System .ai/ also hosts the kind schemas the registry loader scans.
        let system_root = tempdir("sys");
        let user_root = tempdir("user");
        let project_root = tempdir("proj");
        let system_ai = system_root.join(".ai");
        let user_ai = user_root.join(".ai");
        let project_ai = project_root.join(".ai");
        fs::create_dir_all(&system_ai).unwrap();
        fs::create_dir_all(&user_ai).unwrap();
        fs::create_dir_all(&project_ai).unwrap();

        // Kind schema lives only in the system tier (loader scans
        // `<root>/node/engine/kinds`).
        let kinds_dir = system_ai.join("node").join("engine").join("kinds");
        fs::create_dir_all(&kinds_dir).unwrap();
        write_config_kind_schema(&kinds_dir);

        let kinds = KindRegistry::load_base(std::slice::from_ref(&kinds_dir), &trust).unwrap();
        assert!(kinds.contains("config"), "config kind must be registered");

        let parsers = dispatcher_with_canonical_bundle_descriptors();

        let roots = ResolutionRoots {
            ordered: vec![
                ResolutionRoot {
                    space: crate::contracts::ItemSpace::System,
                    label: "system(node)".into(),
                    ai_root: system_ai.clone(),
                },
                ResolutionRoot {
                    space: crate::contracts::ItemSpace::User,
                    label: "user".into(),
                    ai_root: user_ai.clone(),
                },
                ResolutionRoot {
                    space: crate::contracts::ItemSpace::Project,
                    label: "project".into(),
                    ai_root: project_ai.clone(),
                },
            ],
        };

        TestRig {
            kinds,
            parsers,
            trust,
            roots,
            system_ai,
            user_ai,
            project_ai,
        }
    }

    /// Helper to build a CompileContext over a synthetic chain.
    static NULL_PARAMS: Value = Value::Null;
    static EMPTY_HOST_ENV: std::sync::LazyLock<HostEnvBindings> =
        std::sync::LazyLock::new(HostEnvBindings::default);

    fn run_handler(
        rig: &TestRig,
        chain: Vec<ChainIntermediate>,
        current_index: usize,
        block: Value,
        initial_params: Value,
    ) -> Result<Value, EngineError> {
        let mut ctx = CompileContext {
            template_ctx: TemplateContext::new(PathBuf::from("/dev/null")),
            env: HashMap::new(),
            spec_overrides: SpecOverrides::default(),
            params: initial_params,
            original_params: &NULL_PARAMS,
            chain: &chain,
            current_index,
            roots: &rig.roots,
            parsers: &rig.parsers,
            kinds: &rig.kinds,
            trust_store: &rig.trust,
            project_root: None,
            root_trust_class: crate::resolution::TrustClass::TrustedSystem,
            host_env: &EMPTY_HOST_ENV,
        };
        ConfigResolveHandler.apply(&block, &mut ctx)?;
        Ok(ctx.params)
    }

    fn fake_intermediate(executor_id: &str, parsed: Value) -> ChainIntermediate {
        ChainIntermediate {
            executor_id: executor_id.into(),
            resolved_ref: format!("tool:{executor_id}"),
            kind: "tool".into(),
            source_path: PathBuf::from("/tmp/fake"),
            parsed,
        }
    }

    // ── Tests ────────────────────────────────────────────────────────

    #[test]
    fn deep_merge_recursive_dicts() {
        let base = json!({ "a": { "x": 1, "y": 2 }, "b": 1 });
        let over = json!({ "a": { "y": 99, "z": 3 }, "c": 4 });
        let out = deep_merge(base, over);
        assert_eq!(
            out,
            json!({ "a": { "x": 1, "y": 99, "z": 3 }, "b": 1, "c": 4 })
        );
    }

    #[test]
    fn deep_merge_excludes_extends_key() {
        let base = json!({ "x": 1 });
        let over = json!({ "x": 2, "extends": "should_drop" });
        let out = deep_merge(base, over);
        assert_eq!(out, json!({ "x": 2 }));
    }

    #[test]
    fn deep_merge_override_replaces_lists() {
        let base = json!({ "xs": [1, 2, 3] });
        let over = json!({ "xs": [4] });
        let out = deep_merge(base, over);
        assert_eq!(out, json!({ "xs": [4] }));
    }

    // ── Integration: deep_merge across two layers ────────────────────

    #[test]
    fn single_spec_deep_merge_project_overrides_system() {
        let rig = build_rig();
        write_signed_config(
            &rig.system_ai,
            "execution/execution.yaml",
            "defaults:\n  timeout: 10\n  max_steps: 5\nshared: from_system\n",
        );
        write_signed_config(
            &rig.project_ai,
            "execution/execution.yaml",
            "defaults:\n  timeout: 99\nproject_only: yes\n",
        );

        let chain = vec![fake_intermediate("git", json!({}))];
        let block = json!({ "type": "single", "spec": { "path": "execution/execution.yaml" } });

        let params = run_handler(&rig, chain, 0, block, json!({})).unwrap();
        let resolved = params.get("resolved_config").unwrap();

        // defaults.timeout overridden by project (99), max_steps from
        // system survives, shared kept from system, project_only
        // added.
        assert_eq!(resolved["defaults"]["timeout"], json!(99));
        assert_eq!(resolved["defaults"]["max_steps"], json!(5));
        assert_eq!(resolved["shared"], json!("from_system"));
        assert_eq!(resolved["project_only"], json!("yes"));
    }

    #[test]
    fn first_match_returns_project_version() {
        let rig = build_rig();
        write_signed_config(
            &rig.system_ai,
            "alpha.yaml",
            "winner: system\n",
        );
        write_signed_config(&rig.user_ai, "alpha.yaml", "winner: user\n");
        write_signed_config(&rig.project_ai, "alpha.yaml", "winner: project\n");

        let chain = vec![fake_intermediate("git", json!({}))];
        let block = json!({ "type": "single", "spec": { "path": "alpha.yaml", "mode": "first_match" } });

        let params = run_handler(&rig, chain, 0, block, json!({})).unwrap();
        assert_eq!(
            params["resolved_config"]["winner"],
            json!("project"),
            "first_match must walk project → user → system"
        );
    }

    #[test]
    fn tampered_config_file_returns_content_hash_mismatch() {
        let rig = build_rig();
        let path = write_signed_config(
            &rig.project_ai,
            "tamper.yaml",
            "defaults:\n  timeout: 1\n",
        );
        // Tamper: append after signing so content_hash no longer
        // matches what's in the signature header.
        let mut tampered = fs::read_to_string(&path).unwrap();
        tampered.push_str("evil: true\n");
        fs::write(&path, tampered).unwrap();

        let chain = vec![fake_intermediate("git", json!({}))];
        let block = json!({ "type": "single", "spec": { "path": "tamper.yaml" } });

        let err = run_handler(&rig, chain, 0, block, json!({})).unwrap_err();
        assert!(
            matches!(err, EngineError::ContentHashMismatch { .. }),
            "expected ContentHashMismatch, got {err:?}"
        );
    }

    #[test]
    fn multi_spec_returns_path_keyed_map() {
        let rig = build_rig();
        write_signed_config(&rig.project_ai, "a.yaml", "from: a\n");
        write_signed_config(&rig.project_ai, "sub/b.yaml", "from: b\n");

        let chain = vec![fake_intermediate("git", json!({}))];
        let block = json!({ "type": "multi", "specs": [
            { "path": "a.yaml" },
            { "path": "sub/b.yaml", "mode": "first_match" },
        ] });

        let params = run_handler(&rig, chain, 0, block, json!({})).unwrap();
        let resolved = params.get("resolved_config").unwrap();
        assert_eq!(resolved["a.yaml"]["from"], json!("a"));
        assert_eq!(resolved["sub/b.yaml"]["from"], json!("b"));
    }

    #[test]
    fn chain_root_index_zero_writes_resolved_config_param() {
        let rig = build_rig();
        write_signed_config(&rig.project_ai, "x.yaml", "k: v\n");

        let chain = vec![fake_intermediate("mytool", json!({}))];
        let block = json!({ "type": "single", "spec": { "path": "x.yaml" } });
        let params = run_handler(&rig, chain, 0, block, json!({})).unwrap();
        assert!(params.get("resolved_config").is_some());
        assert_eq!(params["resolved_config"]["k"], json!("v"));
    }

    #[test]
    fn chain_non_root_filters_to_execution_params_plus_universal_timeout() {
        let rig = build_rig();
        // Config has timeout (universal), max_steps (declared),
        // and forbidden_field (not in execution_params, not universal).
        // Plus a per-tool override that bumps timeout for `mytool`.
        write_signed_config(
            &rig.project_ai,
            "exec.yaml",
            "defaults:\n  timeout: 30\n  max_steps: 5\n  forbidden_field: nope\n\
             tools:\n  mytool:\n    timeout: 60\n",
        );

        // chain[0] = the root tool; chain[1] = a runtime element
        // whose `execution_params` declares only `max_steps`.
        let chain = vec![
            fake_intermediate("mytool", json!({})),
            fake_intermediate(
                "ryeos/core/subprocess/execute",
                json!({
                    "execution_params": ["max_steps"],
                    "config_resolve": { "path": "exec.yaml" },
                }),
            ),
        ];
        let block = json!({ "type": "single", "spec": { "path": "exec.yaml" } });

        let params = run_handler(&rig, chain, 1, block, json!({})).unwrap();
        // Per-tool override wins for timeout.
        assert_eq!(params["timeout"], json!(60));
        // Declared execution_param surfaces.
        assert_eq!(params["max_steps"], json!(5));
        // Non-allowed field MUST NOT leak in.
        assert!(
            params.get("forbidden_field").is_none(),
            "forbidden_field bled through filter: {params:?}"
        );
        // Root-tool sentinel must NOT appear on a non-root call.
        assert!(params.get("resolved_config").is_none());
    }

    #[test]
    fn chain_non_root_does_not_overwrite_caller_provided_params() {
        let rig = build_rig();
        write_signed_config(
            &rig.project_ai,
            "exec.yaml",
            "defaults:\n  timeout: 30\n  max_steps: 5\n",
        );

        let chain = vec![
            fake_intermediate("mytool", json!({})),
            fake_intermediate(
                "ryeos/core/subprocess/execute",
                json!({ "execution_params": ["max_steps"] }),
            ),
        ];
        let block = json!({ "type": "single", "spec": { "path": "exec.yaml" } });

        // Caller already specified timeout=7 and max_steps=2 — these
        // must win over the resolved config (Python's `if not in
        // parameters` guard).
        let initial = json!({ "timeout": 7, "max_steps": 2 });
        let params = run_handler(&rig, chain, 1, block, initial).unwrap();
        assert_eq!(params["timeout"], json!(7));
        assert_eq!(params["max_steps"], json!(2));
    }

    #[test]
    fn unsigned_config_warns_but_does_not_fail() {
        let rig = build_rig();
        // Write file WITHOUT a signature header.
        let path = rig
            .project_ai
            .join("config")
            .join("plain.yaml");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "k: v\n").unwrap();

        let chain = vec![fake_intermediate("git", json!({}))];
        let block = json!({ "type": "single", "spec": { "path": "plain.yaml" } });

        let params = run_handler(&rig, chain, 0, block, json!({})).unwrap();
        assert_eq!(params["resolved_config"]["k"], json!("v"));
    }

}

//! `EnvConfigHandler` — claims the top-level `env_config` block.
//!
//! Owns local interpreter resolution and symbolic realization-member selection,
//! and merges declared env entries into the compile context. Sets
//! `template_ctx.interpreter` so downstream templates like `${interpreter}` resolve.
//! The daemon owns admission/materialization of realization commands; selecting
//! one here does not prove its availability or grant access to its bytes.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde_json::Value;

use crate::contracts::RuntimeEnvSource;
use crate::error::EngineError;
use crate::runtime::{
    CompileContext, HostEnvBindings, RuntimeHandler, expand_env_value, is_reserved_env_name,
};

pub const KEY: &str = "env_config";

/// Per-platform path separator. The bundle YAMLs target Unix-style
/// hosts; matches Python `os.pathsep`.
const PATH_SEP: &str = ":";

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EnvConfig {
    #[serde(default)]
    pub interpreter: Option<InterpreterConfig>,
    #[serde(default)]
    pub env: HashMap<String, String>,
    /// PATH-style env-var mutations:
    /// `{VAR_NAME: {prepend: [...], append: [...]}}`. Templated
    /// values are deduplicated against existing entries (current
    /// `ctx.env[var]`, falling back to the host env).
    #[serde(default)]
    pub env_paths: HashMap<String, EnvPathMutation>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EnvPathMutation {
    #[serde(default)]
    pub prepend: Vec<String>,
    #[serde(default)]
    pub append: Vec<String>,
}

/// Local resolution and explicit selection from the execution's own admitted
/// external realizations share the ordinary runtime command/argument pipeline.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum InterpreterConfig {
    LocalBinary {
        binary: String,
        #[serde(default)]
        candidates: Vec<String>,
        #[serde(default)]
        search_paths: Vec<String>,
        var: Option<String>,
        /// Bare names tried at the end and resolved against the engine
        /// process's PATH before the isolation boundary.
        #[serde(default)]
        path_candidates: Vec<String>,
    },
    /// Select one direct executable without inspecting host paths/environment.
    /// Its loader/library closure must still be qualified, and its exact member
    /// is admitted by the existing daemon realization-command owner.
    RealizationMember {
        realization_id: String,
        relative_path: String,
    },
}

pub fn resolve_interpreter(
    config: &InterpreterConfig,
    project_root: Option<&Path>,
) -> Result<String, EngineError> {
    resolve_interpreter_with_env(config, project_root, |name| std::env::var(name).ok())
}

fn resolve_interpreter_with_env(
    config: &InterpreterConfig,
    project_root: Option<&Path>,
    mut read_env: impl FnMut(&str) -> Option<String>,
) -> Result<String, EngineError> {
    match config {
        InterpreterConfig::RealizationMember {
            realization_id,
            relative_path,
        } => {
            let command = format!(
                "{}{realization_id}/{relative_path}",
                crate::external_content::REALIZATION_COMMAND_PREFIX,
            );
            let invalid = |reason: String| EngineError::InvalidRuntimeConfig {
                path: "env_config.interpreter".to_owned(),
                reason,
            };
            let selected = crate::external_content::parse_realization_command_ref(&command)
                .map_err(|error| invalid(error.to_string()))?
                .ok_or_else(|| {
                    invalid("interpreter must select a realization member".to_owned())
                })?;
            // Parsing the combined command also validates the canonical path.
            // Check its split against both authored fields so a slash inside
            // realization_id cannot silently become part of relative_path.
            if selected.realization_id != *realization_id
                || selected.relative_path != *relative_path
            {
                return Err(invalid(
                    "interpreter realization coordinates are not canonical".to_owned(),
                ));
            }
            Ok(command)
        }
        InterpreterConfig::LocalBinary {
            binary,
            candidates,
            search_paths,
            var,
            path_candidates,
        } => {
            // 1. Env-var override
            if let Some(v) = var
                && let Some(val) = read_env(v)
            {
                return Ok(val);
            }
            // 2. Project-local search paths × {binary, ...candidates}
            if let Some(root) = project_root {
                let binaries = std::iter::once(binary).chain(candidates.iter());
                for search_path in search_paths {
                    for b in binaries.clone() {
                        let candidate = root.join(search_path).join(b);
                        if candidate.exists() {
                            return Ok(candidate.to_string_lossy().to_string());
                        }
                    }
                }
            }
            // 3. Resolve PATH candidates now. Isolationed subprocess specs require
            // an absolute executable and cannot defer lookup to spawn time.
            for name in path_candidates {
                if let Some(path) = resolve_path_candidate(name) {
                    return Ok(path.to_string_lossy().into_owned());
                }
            }
            Err(EngineError::RuntimeBinaryNotFound {
                binary: binary.clone(),
            })
        }
    }
}

fn resolve_path_candidate(name: &str) -> Option<PathBuf> {
    let candidate = Path::new(name);
    if candidate.components().count() > 1 {
        return if candidate.is_file() {
            std::fs::canonicalize(candidate).ok()
        } else {
            None
        };
    }
    let search_path = std::env::var_os("PATH")?;
    std::env::split_paths(&search_path)
        .map(|directory| directory.join(candidate))
        .find(|path| path.is_file())
        .and_then(|path| std::fs::canonicalize(path).ok())
}

pub struct EnvConfigHandler;

impl RuntimeHandler for EnvConfigHandler {
    fn phase(&self) -> crate::runtime::HandlerPhase {
        crate::runtime::HandlerPhase::ResolveContext
    }

    fn cardinality(&self) -> crate::runtime::HandlerCardinality {
        // env layers across the chain
        crate::runtime::HandlerCardinality::All
    }

    fn key(&self) -> &'static str {
        KEY
    }

    #[tracing::instrument(
        name = "engine:env_config",
        skip(self, block, ctx),
        fields(
            item_ref = %ctx.chain[ctx.current_index].resolved_ref,
            chain_index = ctx.current_index,
        )
    )]
    fn apply(&self, block: &Value, ctx: &mut CompileContext<'_>) -> Result<(), EngineError> {
        let intermediate = &ctx.chain[ctx.current_index];
        let env_config: EnvConfig = serde_json::from_value(block.clone()).map_err(|e| {
            EngineError::InvalidRuntimeConfig {
                path: intermediate.source_path.display().to_string(),
                reason: format!("invalid env_config: {e}"),
            }
        })?;

        // Always-present extra: this element's directory. Templates
        // may reference `${runtime_dir}` to locate sibling files
        // (e.g. PATH entries or runtime launcher args) without
        // cross-element peeking.
        // Last-write-wins across the chain, matching env_paths
        // layering.
        if let Some(parent) = intermediate.source_path.parent() {
            ctx.template_ctx.extra.insert(
                "runtime_dir".to_owned(),
                parent.to_string_lossy().into_owned(),
            );
        }

        // Chains run from the invoked item outwards. The nearest explicit
        // interpreter owns selection; later shared-runtime declarations are
        // defaults. Decode and validate every block, but never resolve a losing
        // local default (including its host env/PATH reads and var injection).
        // Other env/env_paths entries retain their normal layering below.
        if let Some(ic) = env_config.interpreter.as_ref() {
            if let InterpreterConfig::LocalBinary { var: Some(v), .. } = ic {
                if is_reserved_env_name(v) && v != "RYEOS_PYTHON" {
                    return Err(EngineError::ReservedEnvKey { key: v.clone() });
                }
            }
            // Realization coordinate validation is pure and must still reject
            // malformed losing declarations. Local resolution, by contrast,
            // inspects the host and belongs only to the selected declaration.
            let symbolic = if matches!(ic, InterpreterConfig::RealizationMember { .. }) {
                Some(resolve_interpreter(ic, ctx.project_root)?)
            } else {
                None
            };
            if ctx.template_ctx.interpreter.is_none() {
                let resolved = match symbolic {
                    Some(command) => command,
                    None => resolve_interpreter(ic, ctx.project_root)?,
                };
                ctx.template_ctx.interpreter = Some(resolved.clone());
                if let InterpreterConfig::LocalBinary { var: Some(v), .. } = ic {
                    ctx.env.insert(v.clone(), resolved);
                    ctx.env_sources
                        .insert(v.clone(), RuntimeEnvSource::RuntimeInterpreter);
                }
            }
        }

        for (k, v) in env_config.env {
            if is_reserved_env_name(&k) {
                return Err(EngineError::ReservedEnvKey { key: k });
            }
            ctx.env_sources
                .insert(k.clone(), RuntimeEnvSource::RuntimeDescriptor);
            ctx.env.insert(k, v);
        }

        // PATH-style mutations. Templated values are expanded
        // against the same `template_ctx` that `tool_dir`,
        // `runtime_dir`, `interpreter`, etc. already populated, so
        // bundle YAMLs can write entries like
        // `{prepend: ["${runtime_dir}/bin"]}` directly.
        apply_env_paths(
            &env_config.env_paths,
            &mut ctx.env,
            &mut ctx.env_sources,
            &ctx.template_ctx,
            ctx.host_env,
        )?;

        Ok(())
    }
}

/// Apply `env_paths` mutations: prepend/append templated values to
/// the existing `VAR` (from `env`, falling back to the host-env
/// bindings), deduplicating against entries already present.
fn apply_env_paths(
    mutations: &HashMap<String, EnvPathMutation>,
    env: &mut HashMap<String, String>,
    env_sources: &mut HashMap<String, RuntimeEnvSource>,
    template_ctx: &crate::runtime::TemplateContext,
    host_env: &HostEnvBindings,
) -> Result<(), EngineError> {
    for (var_name, mutation) in mutations {
        let existing = env
            .get(var_name)
            .cloned()
            .or_else(|| host_env.values.get(var_name).cloned())
            .unwrap_or_default();
        let mut parts: Vec<String> = if existing.is_empty() {
            Vec::new()
        } else {
            existing.split(PATH_SEP).map(str::to_owned).collect()
        };
        parts.retain(|p| !p.is_empty());

        // Reverse so the first listed prepend ends up at index 0
        // (matches Python `for path in reversed(prepend): parts.insert(0, ...)`).
        for tmpl in mutation.prepend.iter().rev() {
            let resolved = expand_env_value(tmpl, template_ctx, host_env)?;
            if resolved.is_empty() || parts.iter().any(|p| p == &resolved) {
                continue;
            }
            parts.insert(0, resolved);
        }
        for tmpl in &mutation.append {
            let resolved = expand_env_value(tmpl, template_ctx, host_env)?;
            if resolved.is_empty() || parts.iter().any(|p| p == &resolved) {
                continue;
            }
            parts.push(resolved);
        }

        env.insert(var_name.clone(), parts.join(PATH_SEP));
        env_sources.insert(var_name.clone(), RuntimeEnvSource::RuntimePathMutation);
    }
    Ok(())
}

#[cfg(test)]
mod interpreter_resolution_tests {
    //! Pins the Python (and any `local_binary`) interpreter resolution
    //! order: env-var override → project-local search paths → PATH
    //! candidate. This is the contract documented in
    //! `bundles/standard/.ai/knowledge/ryeos/core/runtimes/python-runtime-contract.md`.
    use super::*;
    use std::fs;

    fn shared_python_descriptors() -> [Value; 2] {
        [
            include_str!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../../bundles/core/.ai/tools/ryeos/core/runtimes/python/script.yaml",
            )),
            include_str!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../../bundles/core/.ai/tools/ryeos/core/runtimes/python/function.yaml",
            )),
        ]
        .map(|source| serde_yaml::from_str(source).unwrap())
    }

    fn realization_env(id: &str) -> Value {
        serde_json::json!({"interpreter": {
            "type": "realization_member",
            "realization_id": id,
            "relative_path": "python/bin/python3.14",
        }})
    }

    /// Exercise the real handler/compiler pipeline in plan-builder order:
    /// invoked item, optional wrapper, then the shared Python runtime. Only
    /// env_config/config blocks are selected; unrelated parser/source-admission
    /// contracts are deliberately outside this compiler unit fixture.
    fn compile_python_chain(
        root_env: Option<Value>,
        wrapper_env: Option<Value>,
        descriptor: Value,
    ) -> Result<crate::contracts::PlanSubprocessSpec, EngineError> {
        use crate::runtime::{ChainIntermediate, RuntimeHandlerRegistry, compile_with_handlers};
        use std::sync::Arc;

        let item = |name: &str, parsed: Value| ChainIntermediate {
            executor_id: format!("tool:test/{name}"),
            resolved_ref: format!("tool:test/{name}"),
            kind: "tool".to_owned(),
            source_path: PathBuf::from(format!("/sealed/.ai/tools/test/{name}.yaml")),
            source_space: crate::contracts::ItemSpace::Project,
            source_root: crate::contracts::ItemSourceRoot::Search {
                label: "compiler-fixture".to_owned(),
            },
            parsed,
        };
        let mut root = serde_json::json!({});
        if let Some(env) = root_env {
            root[KEY] = env;
        }
        let mut chain = vec![item("invoked", root)];
        if let Some(env) = wrapper_env {
            chain.push(item("wrapper", serde_json::json!({"env_config": env})));
        }
        chain.push(item(
            "python",
            serde_json::json!({
                "env_config": descriptor[KEY],
                "config": descriptor["config"],
            }),
        ));
        let chain_ids: Vec<_> = chain.iter().map(|item| item.executor_id.clone()).collect();
        let parsers = crate::parsers::ParserDispatcher::new(
            crate::parsers::ParserRegistry::empty(),
            Arc::new(crate::handlers::HandlerRegistry::empty()),
        );
        let kinds = crate::kind_registry::KindRegistry::empty();
        let trust = crate::trust::TrustStore::empty();
        compile_with_handlers(
            &chain,
            &chain[0].source_path,
            &chain_ids,
            &[],
            &RuntimeHandlerRegistry::with_builtins(),
            &serde_json::json!({"value": 7}),
            &HashMap::new(),
            &HostEnvBindings::default(),
            Some(Path::new("/sealed")),
            &parsers,
            &kinds,
            &trust,
            &trust,
            &crate::item_resolution::ResolutionRoots::from_flat(None, vec![]),
            crate::resolution::TrustClass::TrustedProject,
            None,
            None,
        )
    }

    #[test]
    fn invoked_realization_wins_over_wrapper_and_shared_python_interpreters() {
        for descriptor in shared_python_descriptors() {
            let expected_config: crate::runtime::handlers::runtime_config::RuntimeConfig =
                serde_json::from_value(descriptor["config"].clone()).unwrap();
            let spec = compile_python_chain(
                Some(realization_env("invoked")),
                Some(realization_env("wrapper")),
                descriptor.clone(),
            )
            .unwrap();
            assert_eq!(spec.cmd, "realization:invoked/python/bin/python3.14");
            assert!(spec.verified_command.is_none());
            let default_var = descriptor[KEY]["interpreter"]["var"].as_str().unwrap();
            assert!(!spec.env.contains_key(default_var));
            assert!(!spec.env_sources.contains_key(default_var));
            assert_eq!(spec.env["PYTHONUNBUFFERED"], "1");
            assert_eq!(spec.args.len(), expected_config.args.len());
            let source = expected_config
                .args
                .iter()
                .find_map(|argument| match argument {
                    crate::runtime::RuntimeArgument::Literal(value) => Some(&value.literal),
                    _ => None,
                })
                .unwrap();
            assert!(
                spec.args
                    .contains(&crate::contracts::PlanArgument::literal(source))
            );
            // These tool runtimes consume ${tool_path}, not the distinct
            // ${source.entry} persistent-session binding. The compiler must
            // preserve the invoked item's exact logical path; isolation's
            // existing verified-code rewrite/handoff later seals that path.
            let tool_path_positions: Vec<_> = expected_config
                .args
                .iter()
                .enumerate()
                .filter_map(|(index, argument)| {
                    (argument
                        == &crate::runtime::RuntimeArgument::Template("${tool_path}".to_owned()))
                        .then_some(index)
                })
                .collect();
            assert_eq!(tool_path_positions.len(), 1);
            assert_eq!(
                spec.args[tool_path_positions[0]],
                crate::contracts::PlanArgument::literal("/sealed/.ai/tools/test/invoked.yaml"),
            );
            assert!(
                spec.args
                    .contains(&crate::contracts::PlanArgument::literal("-B"))
            );
            assert!(spec.stdin.is_some());
        }
    }

    #[test]
    fn wrapper_interpreter_is_selected_when_invoked_item_has_only_environment() {
        for mut descriptor in shared_python_descriptors() {
            descriptor[KEY]["env_paths"] = serde_json::json!({
                "BUILD_PATH": {"prepend": ["${runtime_dir}/bin"]}
            });
            let mut wrapper = realization_env("wrapper");
            wrapper["env"] = serde_json::json!({"LAYER": "wrapper"});
            wrapper["env_paths"] = serde_json::json!({"BUILD_PATH": {"append": ["wrapper"]}});
            let spec = compile_python_chain(
                Some(serde_json::json!({"env": {"LAYER": "root", "BUILD_PATH": "root"}})),
                Some(wrapper),
                descriptor,
            )
            .unwrap();
            assert_eq!(spec.cmd, "realization:wrapper/python/bin/python3.14");
            assert_eq!(spec.env["LAYER"], "wrapper");
            assert_eq!(
                spec.env["BUILD_PATH"],
                "/sealed/.ai/tools/test/bin:root:wrapper"
            );
        }
    }

    #[test]
    fn losing_local_default_is_never_resolved_and_never_injects_its_var() {
        for mut descriptor in shared_python_descriptors() {
            // No possible local resolution: applying the shared default would
            // fail even on a host with Python installed. No process env edits.
            descriptor[KEY]["interpreter"]["var"] = Value::Null;
            descriptor[KEY]["interpreter"]["path_candidates"] = serde_json::json!([]);
            let spec =
                compile_python_chain(Some(realization_env("invoked")), None, descriptor).unwrap();
            assert_eq!(spec.cmd, "realization:invoked/python/bin/python3.14");
        }
    }

    #[test]
    fn shared_local_interpreter_remains_the_default_without_a_nearer_selection() {
        let directory = tempfile::tempdir().unwrap();
        let executable = directory.path().join("python-fixture");
        fs::write(&executable, b"not executed").unwrap();
        let expected = fs::canonicalize(&executable)
            .unwrap()
            .to_string_lossy()
            .into_owned();
        for mut descriptor in shared_python_descriptors() {
            // Keep the real descriptor's local selection lane, supplying one
            // deterministic fixture candidate rather than requiring host Python.
            descriptor[KEY]["interpreter"]["var"] = Value::Null;
            descriptor[KEY]["interpreter"]["path_candidates"] = serde_json::json!([executable]);
            let spec = compile_python_chain(None, None, descriptor.clone()).unwrap();
            assert_eq!(spec.cmd, expected);

            let spec = compile_python_chain(
                Some(serde_json::json!({"interpreter": descriptor[KEY]["interpreter"]})),
                Some(realization_env("wrapper")),
                descriptor,
            )
            .unwrap();
            assert_eq!(
                spec.cmd, expected,
                "nearest explicit local selection must also win"
            );
        }
    }

    #[test]
    fn losing_interpreter_blocks_still_receive_closed_validation() {
        for baseline in shared_python_descriptors() {
            for invalid in [
                serde_json::json!({"interpreter": {"type": "realization_member", "realization_id": "x", "relative_path": "../escape"}}),
                serde_json::json!({"interpreter": {"type": "local_binary", "binary": "python", "var": "RYEOS_SECRET"}}),
                serde_json::json!({"interpreter": {"type": "local_binary", "binary": "python", "fallback": "python3"}}),
                serde_json::json!({"interpreter": null, "unknown_environment_field": true}),
            ] {
                let mut descriptor = baseline.clone();
                descriptor[KEY] = invalid;
                assert!(
                    compile_python_chain(Some(realization_env("invoked")), None, descriptor)
                        .is_err()
                );
            }
        }
    }

    /// Mirrors the Python runtime descriptors' interpreter block.
    fn python_like(var: Option<&str>) -> InterpreterConfig {
        InterpreterConfig::LocalBinary {
            binary: "python".into(),
            candidates: vec!["python3".into()],
            search_paths: vec![".venv/bin".into(), ".venv/Scripts".into()],
            var: var.map(String::from),
            path_candidates: vec!["python3".into()],
        }
    }

    fn touch(path: &Path) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, "").unwrap();
    }

    #[test]
    fn resolves_project_venv_interpreter_over_path() {
        let root = tempfile::tempdir().unwrap();
        let py = root.path().join(".venv/bin/python3");
        touch(&py);
        let got = resolve_interpreter(&python_like(None), Some(root.path())).unwrap();
        assert_eq!(got, py.to_string_lossy());
    }

    #[test]
    fn prefers_venv_bin_over_scripts_and_binary_over_candidate() {
        // `.venv/bin` is searched before `.venv/Scripts`, and the primary
        // `binary` name before any `candidates`.
        let root = tempfile::tempdir().unwrap();
        touch(&root.path().join(".venv/Scripts/python3"));
        touch(&root.path().join(".venv/bin/python"));
        let got = resolve_interpreter(&python_like(None), Some(root.path())).unwrap();
        assert_eq!(got, root.path().join(".venv/bin/python").to_string_lossy());
    }

    #[test]
    fn falls_back_to_path_candidate_when_no_venv() {
        let root = tempfile::tempdir().unwrap();
        let got = resolve_interpreter(&python_like(None), Some(root.path())).unwrap();
        assert_eq!(Path::new(&got), resolve_path_candidate("python3").unwrap());
    }

    #[test]
    fn falls_back_to_path_candidate_when_no_project_root() {
        let got = resolve_interpreter(&python_like(None), None).unwrap();
        assert_eq!(Path::new(&got), resolve_path_candidate("python3").unwrap());
    }

    #[test]
    fn env_var_override_wins_over_venv_and_path() {
        let var = "RYE_PYTHON_OVERRIDE_INTERP_TEST";
        let root = tempfile::tempdir().unwrap();
        touch(&root.path().join(".venv/bin/python3"));
        let got =
            resolve_interpreter_with_env(&python_like(Some(var)), Some(root.path()), |name| {
                (name == var).then(|| "/custom/python".to_string())
            });
        assert_eq!(got.unwrap(), "/custom/python");
    }

    #[test]
    fn realization_interpreter_preserves_symbolic_command_without_host_resolution() {
        let config: InterpreterConfig = serde_json::from_value(serde_json::json!({
            "type": "realization_member",
            "realization_id": "python",
            "relative_path": "python/bin/python3.14",
        }))
        .unwrap();
        let resolved = resolve_interpreter_with_env(
            &config,
            Some(Path::new("/does-not-exist/project")),
            |_| panic!("realization selection must not read the host environment"),
        )
        .unwrap();
        assert_eq!(resolved, "realization:python/python/bin/python3.14");

        let mut context = crate::runtime::TemplateContext::new("/sealed/tool.py".into());
        context.interpreter = Some(resolved.clone());
        assert_eq!(
            crate::runtime::expand_template("${interpreter}", &context).unwrap(),
            resolved,
        );
        assert_eq!(
            crate::external_content::parse_realization_command_ref(&resolved)
                .unwrap()
                .unwrap(),
            crate::external_content::ExternalRealizationCommandRef {
                realization_id: "python".to_owned(),
                relative_path: "python/bin/python3.14".to_owned(),
            },
        );
    }

    #[test]
    fn realization_interpreter_rejects_noncanonical_coordinates() {
        for (realization_id, relative_path) in [
            ("", "bin/python"),
            ("Python", "bin/python"),
            ("python/other", "bin/python"),
            ("python", ""),
            ("python", "/usr/bin/python"),
            ("python", "../bin/python"),
            ("python", "bin/../python"),
            ("python", "bin//python"),
            ("python", "bin/python/"),
        ] {
            let config = InterpreterConfig::RealizationMember {
                realization_id: realization_id.to_owned(),
                relative_path: relative_path.to_owned(),
            };
            assert!(
                resolve_interpreter_with_env(&config, None, |_| {
                    panic!("invalid realization selection must not fall back to host resolution")
                })
                .is_err(),
                "accepted {realization_id:?}/{relative_path:?}",
            );
        }
    }

    #[test]
    fn realization_interpreter_has_no_local_fallback_or_argv_fields() {
        let baseline = serde_json::json!({
            "type": "realization_member",
            "realization_id": "python",
            "relative_path": "python/bin/python3.14",
        });
        for (field, value) in [
            ("binary", serde_json::json!("python3")),
            ("candidates", serde_json::json!(["python3"])),
            ("search_paths", serde_json::json!([".venv/bin"])),
            ("path_candidates", serde_json::json!(["python3"])),
            ("var", serde_json::json!("RYE_PYTHON")),
            ("args", serde_json::json!(["--library-path", "/usr/lib"])),
        ] {
            let mut config = baseline.clone();
            config[field] = value;
            assert!(serde_json::from_value::<InterpreterConfig>(config).is_err());
        }
        for field in ["realization_id", "relative_path"] {
            let mut config = baseline.clone();
            config.as_object_mut().unwrap().remove(field);
            assert!(serde_json::from_value::<InterpreterConfig>(config).is_err());
        }
    }

    #[test]
    fn shared_python_descriptors_disable_bytecode_in_isolated_mode() {
        // -I ignores Python environment options, so the direct invocation
        // must carry -B rather than relying on PYTHONDONTWRITEBYTECODE.
        for descriptor in shared_python_descriptors() {
            let runtime: crate::runtime::handlers::runtime_config::RuntimeConfig =
                serde_json::from_value(descriptor["config"].clone()).unwrap();
            assert_eq!(runtime.command, "${interpreter}");
            use crate::runtime::handlers::runtime_config::RuntimeArgument;
            let prefix: Vec<_> = runtime
                .args
                .iter()
                .take_while(|argument| **argument != RuntimeArgument::Template("-c".to_owned()))
                .collect();
            assert!(prefix.contains(&&RuntimeArgument::Template("-I".to_owned())));
            assert!(prefix.contains(&&RuntimeArgument::Template("-B".to_owned())));
            assert!(matches!(
                runtime.args.get(prefix.len() + 1),
                Some(RuntimeArgument::Literal(_)),
            ));
        }
    }
}

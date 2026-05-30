use std::path::PathBuf;

use ryeos_runtime::alias_registry::{AliasDef, ProjectResolution};
use serde_json::Value;

use crate::error::CliError;
use crate::local_verbs;

/// CLI struct for clap argument parsing.
#[derive(clap::Parser)]
#[command(
    name = "ryeos",
    about = "CLI for Rye OS",
    disable_help_subcommand = true,
    trailing_var_arg = true
)]
pub struct Cli {
    /// Project root (overrides cwd).
    #[arg(short, long)]
    project: Option<PathBuf>,

    /// Verbose tracing output.
    #[arg(long)]
    pub debug: bool,

    /// Verb tokens + tail (everything after globals).
    #[arg(trailing_var_arg = true)]
    pub rest: Vec<String>,
}

/// Main dispatch flow.
pub async fn run(cli: Cli) -> Result<(), CliError> {
    // 1. Project root
    let body_project_path = match &cli.project {
        Some(p) => p.to_string_lossy().into_owned(),
        None => ".".to_string(),
    };

    // 2. System space dir
    let system_space_dir = discover_system_space_dir();

    // 3. Hardcoded LOCAL verbs (must work before daemon exists):
    //      ryeos init                       — bootstrap operator state
    //      ryeos trust pin --from <trust>   — pin a publisher key
    //      ryeos publish <src>              — bundle author publish dance
    //      ryeos vault {put,list,remove,rewrap} — sealed secret management
    if local_verbs::try_dispatch(&cli.rest).await? {
        return Ok(());
    }

    // 4. No verb = help
    if cli.rest.is_empty() {
        crate::help::print_help(std::io::stdout())?;
        return Ok(());
    }

    // `ryeos help` → top-level help
    if cli.rest == ["help"] {
        crate::help::print_help(std::io::stdout())?;
        return Ok(());
    }

    // `ryeos help <verb...>` → verb help (queries daemon for alias info)
    if cli.rest.len() > 1 && cli.rest[0] == "help" {
        crate::help::print_verb_help(&cli.rest[1..], &system_space_dir, &body_project_path).await?;
        return Ok(());
    }

    // `ryeos <verb...> --help` / `-h` should feel like a normal CLI.
    // Without this guard the trailing help flag is bound as service
    // input and strict service schemas return noisy "unknown field help".
    if let Some(help_idx) = cli.rest.iter().position(|t| t == "--help" || t == "-h") {
        let verb_tokens = &cli.rest[..help_idx];
        if verb_tokens.is_empty() {
            crate::help::print_help(std::io::stdout())?;
        } else {
            crate::help::print_verb_help(verb_tokens, &system_space_dir, &body_project_path)
                .await?;
        }
        return Ok(());
    }

    // 5. Hardcoded `ryeos execute <item_ref>` — the universal escape hatch
    if cli.rest.first().map(|s| s.as_str()) == Some("execute") {
        if cli.rest.len() < 2 {
            return Err(CliError::UnknownVerb {
                argv: cli.rest.clone(),
            });
        }
        let item_ref = &cli.rest[1];
        // Validate it parses as a canonical ref
        let _canonical =
            ryeos_engine::canonical_ref::CanonicalRef::parse(item_ref).map_err(|_| {
                crate::error::CliConfigError::InvalidExecuteRef {
                    path: "<cli>".into(),
                    item_ref: item_ref.clone(),
                    detail: "not a valid canonical ref".into(),
                }
            })?;

        // Scan tail for --input <path> (mutually exclusive with flag-style binding)
        let tail = &cli.rest[2..];
        let parameters = if let Some(input_val) = crate::arg_bind::parse_input_arg(tail)? {
            input_val
        } else {
            crate::arg_bind::bind_tail(tail)?
        };

        let body = serde_json::json!({
            "item_ref": item_ref,
            "project_path": body_project_path,
            "parameters": parameters,
        });

        let result = post_to_daemon(&system_space_dir, &body).await?;
        print_result(result);
        return Ok(());
    }

    // 6. Descriptor-driven offline dispatch.
    //    For commands whose service descriptor declares availability: offline,
    //    run the in-process handler. Returns None to fall through to daemon.
    if let Some(outcome) = crate::offline_dispatch::try_offline_dispatch(
        &cli.rest,
        &system_space_dir,
        &body_project_path,
    )? {
        if let crate::offline_dispatch::OfflineDispatchOutcome::Json(result) = outcome {
            print_result(result);
        }
        return Ok(());
    }

    // 7. Token dispatch — send tokens to daemon, it resolves via alias
    //    registry and binds tail parameters server-side.
    //
    //    For remote verbs that take a project root, CLI-side rewrite injects a canonical
    //    `--project <abs>` or `--no-project` into the tail. The daemon
    //    cannot do this — its cwd is irrelevant to the caller. Accepting
    //    `--project` here is deliberate: project-aware aliases expose a
    //    service-schema `project` field, while global `-p/--project` before
    //    the verb remains supported by clap above.

    let normalized_kv = normalize_bare_key_value_args(&cli.rest);
    let tokens = canonicalize_tokens_from_alias_metadata(
        &normalized_kv,
        &system_space_dir,
        cli.project.as_deref(),
    )?;

    let body = serde_json::json!({
        "tokens": tokens,
        "project_path": body_project_path,
    });

    let result = post_to_daemon(&system_space_dir, &body).await?;
    print_result(result);
    Ok(())
}

/// CLI-side compatibility for Rye's historical `key=value` shorthand.
/// Token-mode binding happens inside the daemon; rewriting here means a
/// newer CLI remains pleasant even when talking to an older local daemon.
fn normalize_bare_key_value_args(rest: &[String]) -> Vec<String> {
    let mut out = Vec::with_capacity(rest.len());
    for token in rest {
        if token.starts_with('-') {
            out.push(token.clone());
            continue;
        }
        if let Some((key, value)) = token.split_once('=') {
            if !key.is_empty() && !value.is_empty() {
                out.push(format!("--{key}"));
                out.push(value.to_string());
                continue;
            }
        }
        out.push(token.clone());
    }
    out
}

fn canonicalize_tokens_from_alias_metadata(
    rest: &[String],
    system_space_dir: &std::path::Path,
    default_project: Option<&std::path::Path>,
) -> Result<Vec<String>, CliError> {
    let aliases = load_aliases_from_disk(system_space_dir);
    canonicalize_tokens_with_aliases_and_project(rest, &aliases, default_project)
}

#[cfg(test)]
fn canonicalize_tokens_with_aliases(
    rest: &[String],
    aliases: &[AliasDef],
) -> Result<Vec<String>, CliError> {
    canonicalize_tokens_with_aliases_and_project(rest, aliases, None)
}

fn canonicalize_tokens_with_aliases_and_project(
    rest: &[String],
    aliases: &[AliasDef],
    default_project: Option<&std::path::Path>,
) -> Result<Vec<String>, CliError> {
    let Some((alias, consumed)) = match_alias(rest, aliases) else {
        return Ok(rest.to_vec());
    };
    let tail = &rest[consumed..];

    if alias.positional_field.is_none()
        && alias.positional_forms.is_empty()
        && alias.project_resolution == ProjectResolution::None
    {
        return Ok(rest.to_vec());
    }

    let bound = ryeos_runtime::arg_binder::bind_argv_with_alias(tail, Some(alias))
        .map_err(CliError::ProjectResolution)?;
    let mut canonical_tail = params_to_tail(&bound);

    match alias.project_resolution {
        ProjectResolution::None => {}
        ProjectResolution::Optional => {
            canonical_tail = crate::project_resolve::rewrite_project_tail_with_default(
                &canonical_tail,
                default_project,
            )?;
        }
        ProjectResolution::Required => {
            if canonical_tail.iter().any(|t| t == "--no-project") {
                return Err(CliError::ProjectResolution(
                    "this command requires a project; do not pass --no-project".into(),
                ));
            }
            canonical_tail = crate::project_resolve::rewrite_project_tail_with_default(
                &canonical_tail,
                default_project,
            )?;
            if canonical_tail.iter().any(|t| t == "--no-project") {
                return Err(CliError::ProjectResolution(
                    "this command requires a project; run it from a directory containing .ai/ \
                     or pass --project <path>"
                        .into(),
                ));
            }
        }
    }

    let mut out = rest[..consumed].to_vec();
    out.extend(canonical_tail);
    Ok(out)
}

fn match_alias<'a>(rest: &[String], aliases: &'a [AliasDef]) -> Option<(&'a AliasDef, usize)> {
    for len in (1..=rest.len()).rev() {
        let prefix = &rest[..len];
        if let Some(alias) = aliases.iter().find(|a| a.tokens == prefix) {
            return Some((alias, len));
        }
    }
    None
}

fn params_to_tail(params: &Value) -> Vec<String> {
    let mut out = Vec::new();
    let Some(obj) = params.as_object() else {
        return out;
    };
    let mut keys: Vec<&String> = obj.keys().collect();
    keys.sort();
    for key in keys {
        emit_param(&mut out, key, &obj[key]);
    }
    out
}

fn emit_param(out: &mut Vec<String>, key: &str, value: &Value) {
    match value {
        Value::Bool(true) => out.push(format!("--{}", key.replace('_', "-"))),
        Value::Bool(false) | Value::Null => {}
        Value::Array(values) => {
            for v in values {
                emit_param(out, key, v);
            }
        }
        other => {
            out.push(format!("--{}", key.replace('_', "-")));
            out.push(match other {
                Value::String(s) => s.clone(),
                _ => other.to_string(),
            });
        }
    }
}

fn load_aliases_from_disk(system_space_dir: &std::path::Path) -> Vec<AliasDef> {
    let bundle_roots = crate::node_descriptors::direct_bundle_roots(system_space_dir);
    crate::node_descriptors::load_alias_descriptors(&bundle_roots)
        .map(|aliases| aliases.into_iter().map(|alias| alias.def).collect())
        .unwrap_or_default()
}

/// POST a JSON body to the daemon's /execute endpoint and return the response.
async fn post_to_daemon(
    system_space_dir: &std::path::Path,
    body: &Value,
) -> Result<Value, CliError> {
    lifecycle_preflight(system_space_dir).await?;
    let daemon_url = crate::transport::http::resolve_daemon_url(system_space_dir).await?;
    let signer = crate::transport::signing::Signer::resolve(system_space_dir)?;

    // Discover the daemon's principal_id for audience binding.
    let audience = crate::transport::discovery::discover_audience(&daemon_url).await?;

    let body_bytes = serde_json::to_vec(body).expect("infallible: Value serialization");
    let headers = signer.sign("POST", "/execute", &body_bytes, &audience)?;

    let url = format!("{}/execute", daemon_url);
    let payload = crate::transport::http::post_json(&url, &headers, &body_bytes).await?;
    Ok(payload)
}

async fn lifecycle_preflight(system_space_dir: &std::path::Path) -> Result<(), CliError> {
    // A deliberate remote override is still valid for normal daemon-backed
    // dispatch. Lifecycle reads/mutations themselves ignore this env var.
    if std::env::var_os("RYEOSD_URL").is_some() {
        return Ok(());
    }

    let env =
        ryeos_node::LocalLifecycleEnv::load(Some(system_space_dir.to_path_buf())).map_err(|e| {
            CliError::Local {
                detail: format!("resolve local node lifecycle env: {e:#}"),
            }
        })?;
    match ryeos_node::LifecycleController::from_env(env)
        .status()
        .await
        .map_err(|e| CliError::Local {
            detail: format!("read lifecycle status: {e:#}"),
        })? {
        ryeos_node::LifecycleStatus::Running { .. } => Ok(()),
        ryeos_node::LifecycleStatus::NotInitialized { diagnostics } => Err(CliError::Local {
            detail: format!(
                "RyeOS is not initialized. Run: ryeos init\nDetail: {}",
                diagnostics.message
            ),
        }),
        ryeos_node::LifecycleStatus::Stopped { .. } => Err(CliError::Local {
            detail: "RyeOS is initialized but not running. Run: ryeos start".into(),
        }),
        ryeos_node::LifecycleStatus::Stale { diagnostics, .. } => Err(CliError::Local {
            detail: format!(
                "RyeOS daemon metadata is stale: {}\nRun: ryeos start",
                diagnostics.message
            ),
        }),
    }
}

fn print_result(payload: serde_json::Value) {
    let result = payload.get("result").cloned().unwrap_or(payload);
    let pretty = serde_json::to_string_pretty(&result).unwrap_or_else(|_| result.to_string());
    println!("{pretty}");
}

fn discover_system_space_dir() -> PathBuf {
    if let Ok(p) = std::env::var("RYEOS_SYSTEM_SPACE_DIR") {
        return PathBuf::from(p);
    }
    dirs::data_dir()
        .map(|d| d.join("ryeos"))
        .expect("could not determine XDG data directory")
}

#[cfg(test)]
mod tests {
    use super::*;
    use ryeos_runtime::{PositionalForm, PositionalMatcher, PositionalSlot};
    fn with_user_space<T>(f: impl FnOnce() -> T) -> T {
        let _g = crate::test_env::lock();
        let saved = std::env::var_os("USER_SPACE");
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(ryeos_engine::AI_DIR)).unwrap();
        std::env::set_var("USER_SPACE", tmp.path());
        let result = f();
        if let Some(v) = saved {
            std::env::set_var("USER_SPACE", v);
        } else {
            std::env::remove_var("USER_SPACE");
        }
        result
    }

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    fn alias(
        tokens: &[&str],
        forms: Vec<Vec<(&str, PositionalMatcher)>>,
        project_resolution: ProjectResolution,
    ) -> AliasDef {
        AliasDef {
            tokens: s(tokens),
            verb: tokens.join("-"),
            deprecated: false,
            replacement_tokens: None,
            removed_in: None,
            positional_field: None,
            positional_forms: forms
                .into_iter()
                .map(|slots| PositionalForm {
                    slots: slots
                        .into_iter()
                        .map(|(field, matcher)| PositionalSlot {
                            field: field.to_string(),
                            matcher,
                        })
                        .collect(),
                })
                .collect(),
            project_resolution,
        }
    }

    #[test]
    fn remote_threads_positional_remote_is_normalized() {
        let aliases = vec![alias(
            &["remote", "threads"],
            vec![vec![("remote", PositionalMatcher::Any)]],
            ProjectResolution::None,
        )];
        let out = canonicalize_tokens_with_aliases(&s(&["remote", "threads", "railway"]), &aliases)
            .unwrap();
        assert_eq!(out, s(&["remote", "threads", "--remote", "railway"]));
    }

    #[test]
    fn bare_key_value_is_normalized_for_token_mode() {
        let out =
            normalize_bare_key_value_args(&s(&["remote", "status", "remote=railway", "limit=5"]));
        assert_eq!(
            out,
            s(&["remote", "status", "--remote", "railway", "--limit", "5",])
        );
    }

    #[test]
    fn remote_project_status_positional_remote_is_normalized() {
        let aliases = vec![alias(
            &["remote", "project-status"],
            vec![vec![("remote", PositionalMatcher::Any)]],
            ProjectResolution::Required,
        )];
        let out = canonicalize_tokens_with_aliases(
            &s(&["remote", "project-status", "railway", "--project", "/tmp"]),
            &aliases,
        )
        .unwrap();
        assert_eq!(
            out,
            s(&[
                "remote",
                "project-status",
                "--remote",
                "railway",
                "--project",
                "/tmp",
            ])
        );
    }

    #[test]
    fn remote_bind_project_accepts_project_after_verb() {
        let tmp = tempfile::tempdir().unwrap();
        let aliases = vec![alias(
            &["remote", "bind-project"],
            vec![vec![("remote", PositionalMatcher::Any)]],
            ProjectResolution::Required,
        )];
        let out = canonicalize_tokens_with_aliases(
            &s(&[
                "remote",
                "bind-project",
                "prod",
                "--project",
                &tmp.path().to_string_lossy(),
                "--remote-project",
                "/data/app",
                "--sync-scope",
                "ai_only",
            ]),
            &aliases,
        )
        .unwrap();
        assert_eq!(
            out[0..4],
            s(&["remote", "bind-project", "--remote", "prod"])
        );
        assert!(out
            .windows(2)
            .any(|w| w[0] == "--project" && w[1] == tmp.path().to_string_lossy()));
    }

    #[test]
    fn remote_doctor_accepts_optional_project_after_verb() {
        with_user_space(|| {
            let tmp = tempfile::tempdir().unwrap();
            let aliases = vec![alias(
                &["remote", "doctor"],
                vec![vec![("remote", PositionalMatcher::Any)]],
                ProjectResolution::Optional,
            )];
            let out = canonicalize_tokens_with_aliases(
                &s(&[
                    "remote",
                    "doctor",
                    "prod",
                    "--project",
                    &tmp.path().to_string_lossy(),
                ]),
                &aliases,
            )
            .unwrap();
            assert_eq!(out[0..4], s(&["remote", "doctor", "--remote", "prod"]));
            assert!(out
                .windows(2)
                .any(|w| w[0] == "--project" && w[1] == tmp.path().to_string_lossy()));
        });
    }

    #[test]
    fn project_aware_alias_uses_global_project_default() {
        let tmp = tempfile::tempdir().unwrap();
        let aliases = vec![alias(
            &["remote", "bind-project"],
            vec![vec![("remote", PositionalMatcher::Any)]],
            ProjectResolution::Required,
        )];
        let out = canonicalize_tokens_with_aliases_and_project(
            &s(&[
                "remote",
                "bind-project",
                "prod",
                "--remote-project",
                "/data/app",
            ]),
            &aliases,
            Some(tmp.path()),
        )
        .unwrap();
        assert!(out.windows(2).any(|w| {
            w[0] == "--project" && w[1] == tmp.path().canonicalize().unwrap().to_string_lossy()
        }));
    }

    #[test]
    fn remote_execute_remote_then_item_is_normalized() {
        with_user_space(|| {
            let aliases = vec![alias(
                &["remote", "execute"],
                vec![
                    vec![
                        ("remote", PositionalMatcher::Any),
                        ("item_ref", PositionalMatcher::CanonicalRef),
                    ],
                    vec![("item_ref", PositionalMatcher::CanonicalRef)],
                ],
                ProjectResolution::Optional,
            )];
            let out = canonicalize_tokens_with_aliases(
                &s(&[
                    "remote",
                    "execute",
                    "railway",
                    "service:health/status",
                    "--no-project",
                ]),
                &aliases,
            )
            .unwrap();
            assert_eq!(
                out,
                s(&[
                    "remote",
                    "execute",
                    "--item-ref",
                    "service:health/status",
                    "--remote",
                    "railway",
                    "--no-project",
                ])
            );
        });
    }

    #[test]
    fn remote_execute_item_only_is_left_for_default_remote() {
        with_user_space(|| {
            let aliases = vec![alias(
                &["remote", "execute"],
                vec![
                    vec![
                        ("remote", PositionalMatcher::Any),
                        ("item_ref", PositionalMatcher::CanonicalRef),
                    ],
                    vec![("item_ref", PositionalMatcher::CanonicalRef)],
                ],
                ProjectResolution::Optional,
            )];
            let input = s(&["remote", "execute", "service:health/status", "--no-project"]);
            let out = canonicalize_tokens_with_aliases(&input, &aliases).unwrap();
            assert_eq!(
                out,
                s(&[
                    "remote",
                    "execute",
                    "--item-ref",
                    "service:health/status",
                    "--no-project",
                ])
            );
        });
    }

    #[test]
    fn explicit_remote_forms_are_not_rewritten() {
        let aliases = vec![alias(
            &["remote", "threads"],
            vec![vec![("remote", PositionalMatcher::Any)]],
            ProjectResolution::None,
        )];
        let flag = s(&["remote", "threads", "--remote", "railway"]);
        assert_eq!(
            canonicalize_tokens_with_aliases(&flag, &aliases).unwrap(),
            flag
        );

        let equals_flag = s(&["remote", "threads", "--remote=railway"]);
        assert_eq!(
            canonicalize_tokens_with_aliases(&equals_flag, &aliases).unwrap(),
            s(&["remote", "threads", "--remote", "railway"])
        );
    }

    #[test]
    fn aliases_without_metadata_preserve_positional_tail() {
        let aliases = vec![alias(&["status"], Vec::new(), ProjectResolution::None)];
        let input = s(&["status", "extra-arg"]);
        let out = canonicalize_tokens_with_aliases(&input, &aliases).unwrap();
        assert_eq!(out, input);
    }
}

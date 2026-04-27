//! CLI dispatch: route commands to the daemon via HTTP, or fall back
//! to run-service for offline-only services when the daemon is down.

use std::path::PathBuf;

use anyhow::{bail, Result};
use http_body_util::BodyExt;
use hyper_util::rt::TokioExecutor;

use crate::cmd::ClientCmd;

/// Resolve the path to the `ryeosd` binary.
///
/// Priority:
/// 1. `RYEOSD_BIN` environment variable (explicit override for tests / deploys).
/// 2. Sibling of current exe (release builds, deployed bundles).
/// 3. PATH lookup.
fn ryeosd_binary_path() -> PathBuf {
    if let Ok(p) = std::env::var("RYEOSD_BIN") {
        return PathBuf::from(p);
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let candidate = dir.join("ryeosd");
            if candidate.exists() {
                return candidate;
            }
        }
    }
    PathBuf::from("ryeosd")
}

/// Try to call the daemon via HTTP over TCP.
///
/// Transport: TCP only.
///
/// The daemon also exposes a UDS listener (`socket` in `daemon.json`),
/// but per V5.2-CLEANUP Task 1 the UDS surface is intentionally narrow:
/// only `system.health` and token-gated `runtime.*` are exposed there
/// (custom MessagePack-RPC, not HTTP). General `/execute` traffic is
/// HTTP-over-TCP. There is one path per command; if the daemon is
/// unreachable, the caller surfaces a clear error or falls back to
/// `ryeosd run-service` per the architecture principle:
/// "Tools require the daemon. Services don't."
///
/// Returns `Ok(result)` on success, or an error if the daemon is unreachable.
async fn try_execute(
    state_dir: &std::path::Path,
    item_ref: &str,
    params: &str,
    project_path: &str,
) -> Result<serde_json::Value> {
    // Read daemon.json to find the daemon's TCP bind address.
    let daemon_json_path = state_dir.join("daemon.json");
    let daemon_json = std::fs::read_to_string(&daemon_json_path)
        .map_err(|_| anyhow::anyhow!(
            "daemon not running (no daemon.json found at {})",
            daemon_json_path.display()
        ))?;

    let daemon_info: serde_json::Value = serde_json::from_str(&daemon_json)
        .map_err(|e| anyhow::anyhow!("failed to parse daemon.json: {e}"))?;

    let bind = daemon_info
        .get("bind")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!(
            "daemon.json missing 'bind' field (TCP address required for /execute)"
        ))?;

    let url: http::Uri = format!("http://{bind}/execute")
        .parse()
        .map_err(|e| anyhow::anyhow!("invalid bind address '{}': {e}", bind))?;

    let body_value = serde_json::json!({
        "item_ref": item_ref,
        "project_path": project_path,
        "parameters": if params.is_empty() {
            serde_json::json!({})
        } else {
            serde_json::from_str(params)?
        },
    });
    let body_str = serde_json::to_string(&body_value)?;

    let req = hyper::Request::builder()
        .method("POST")
        .uri(url)
        .header("content-type", "application/json")
        .body(http_body_util::Full::new(bytes::Bytes::from(body_str)))?;

    let client = hyper_util::client::legacy::Client::builder(TokioExecutor::new())
        .build_http::<http_body_util::Full<bytes::Bytes>>();
    let resp = client.request(req).await?;

    let status = resp.status();
    let collected = resp.into_body().collect().await
        .map_err(|e| anyhow::anyhow!("failed to read response body: {e}"))?;
    let body_bytes = collected.to_bytes();

    let resp_json: serde_json::Value = serde_json::from_slice(&body_bytes)
        .map_err(|e| anyhow::anyhow!("failed to parse response JSON: {e}"))?;

    if !status.is_success() {
        let error_msg = resp_json["error"]
            .as_str()
            .unwrap_or("unknown error");
        bail!("daemon returned {}: {}", status, error_msg);
    }

    Ok(resp_json)
}

/// Spawn `ryeosd run-service` as a subprocess (daemon-unreachable fallback).
async fn spawn_run_service(
    state_dir: &std::path::Path,
    item_ref: &str,
    params: &str,
) -> Result<()> {
    let ryeosd = ryeosd_binary_path();

    // Argument order matters: `--state-dir` is a top-level Cli flag and must
    // precede the `run-service` subcommand. `--params` belongs to the
    // subcommand and goes after the service ref.
    let mut cmd = tokio::process::Command::new(&ryeosd);
    cmd.arg("--state-dir")
        .arg(state_dir)
        .arg("run-service")
        .arg(item_ref);

    if !params.is_empty() {
        cmd.arg("--params").arg(params);
    }

    let status = cmd.status().await?;
    if !status.success() {
        bail!(
            "ryeosd run-service (at {}) exited with code {:?}",
            ryeosd.display(),
            status.code()
        );
    }
    Ok(())
}

/// Extract project_path from a command, defaulting to ".".
fn resolve_project_path(cmd: &ClientCmd) -> std::borrow::Cow<'_, str> {
    match cmd {
        ClientCmd::Execute { project_path, .. } => {
            project_path
                .as_ref()
                .map(|p| p.to_str().unwrap_or("."))
                .unwrap_or(".")
                .into()
        }
        _ => ".".into(),
    }
}

/// Lower a thin alias to its execute equivalent.
fn lower_to_execute(cmd: &ClientCmd) -> Result<(String, String)> {
    match cmd {
        ClientCmd::Status => Ok(("service:system/status".into(), "{}".into())),
        ClientCmd::Verify { item_ref, all } => {
            if *all {
                Ok(("service:verify".into(), r#"{"all": true}"#.into()))
            } else {
                let ref_str = item_ref
                    .as_deref()
                    .ok_or_else(|| anyhow::anyhow!("verify requires --all or an item_ref"))?;
                Ok((
                    "service:verify".into(),
                    serde_json::json!({"item_ref": ref_str}).to_string(),
                ))
            }
        }
        ClientCmd::Rebuild { verify } => Ok((
            "service:rebuild".into(),
            serde_json::json!({"verify": *verify}).to_string(),
        )),
        ClientCmd::SubmitCommand {
            thread_id,
            command_type,
            params,
        } => {
            let p = params.as_deref().unwrap_or("{}");
            Ok((
                "service:commands/submit".into(),
                serde_json::json!({
                    "thread_id": thread_id,
                    "command_type": command_type,
                    "params": serde_json::from_str::<serde_json::Value>(p)?,
                })
                .to_string(),
            ))
        }
        ClientCmd::Execute {
            item_ref,
            params,
            ..
        } => Ok((
            item_ref.clone(),
            params.as_deref().unwrap_or("{}").into(),
        )),
        ClientCmd::BuildBundle { .. }
        | ClientCmd::UserKeySign { .. }
        | ClientCmd::RebuildManifest { .. } => {
            bail!("cannot lower to execute")
        }
    }
}

/// Build a bundle: recursively copy source into output.
fn build_bundle(source: &std::path::Path, output: Option<&std::path::Path>) -> Result<()> {
    if !source.is_dir() {
        bail!("source '{}' is not a directory", source.display());
    }

    let output = match output {
        Some(o) => o,
        None => source,
    };

    fn copy_recursive(src: &std::path::Path, dst: &std::path::Path) -> Result<()> {
        std::fs::create_dir_all(dst)?;
        for entry in std::fs::read_dir(src)? {
            let entry = entry?;
            let from = entry.path();
            let to = dst.join(entry.file_name());
            if from.is_dir() {
                copy_recursive(&from, &to)?;
            } else {
                std::fs::copy(&from, &to)?;
            }
        }
        Ok(())
    }

    copy_recursive(source, output)?;
    println!("Bundle built: {}", output.display());
    Ok(())
}

/// Pretty-print a JSON result to stdout.
fn print_result(result: &serde_json::Value) {
    let inner = result.get("result");
    let to_print = inner.unwrap_or(result);
    println!(
        "{}",
        serde_json::to_string_pretty(to_print).unwrap_or_else(|_| to_print.to_string())
    );
}

/// Main dispatch: try daemon, fall back to run-service for service refs.
pub async fn dispatch(state_dir: &std::path::Path, cmd: ClientCmd) -> Result<()> {
    match cmd {
        ClientCmd::BuildBundle { source, output } => {
            let source = source.as_deref()
                .ok_or_else(|| anyhow::anyhow!("build-bundle requires --source <path>"))?;
            build_bundle(source, output.as_deref())
        }
        ClientCmd::UserKeySign { input, key } => {
            let _report = ryeos_tools::actions::sign::run_sign(&input, key.as_deref())
                .map_err(|e| anyhow::anyhow!("user-key-sign: {e}"))?;
            Ok(())
        }
        ClientCmd::RebuildManifest { source, key, seed } => {
            let signing_key = match (key.as_ref(), seed) {
                (Some(_), Some(_)) => {
                    bail!("rebuild-manifest: pass either --key or --seed, not both")
                }
                (Some(p), None) => ryeos_tools::actions::build_bundle::load_signing_key(p)?,
                (None, Some(s)) => ryeos_tools::actions::build_bundle::signing_key_from_seed(s),
                (None, None) => bail!(
                    "rebuild-manifest: --key <pem> or --seed <0..=255> is required"
                ),
            };
            let report = ryeos_tools::actions::build_bundle::rebuild_bundle_manifest(
                &source,
                &signing_key,
            )?;
            println!("{}", serde_json::to_string_pretty(&report)?);
            Ok(())
        }
        other => {
            let project_path = resolve_project_path(&other);
            let (item_ref, params) = lower_to_execute(&other)?;
            match try_execute(state_dir, &item_ref, &params, &project_path).await {
                Ok(result) => {
                    print_result(&result);
                    Ok(())
                }
                Err(e) => {
                    let msg = e.to_string();
                    if msg.contains("daemon not running") || msg.contains("failed to connect") {
                        if item_ref.starts_with("service:") {
                            eprintln!("daemon not running, falling back to run-service...");
                            spawn_run_service(state_dir, &item_ref, &params).await?;
                            Ok(())
                        } else {
                            bail!("daemon required for {item_ref}; start the daemon")
                        }
                    } else {
                        Err(e)
                    }
                }
            }
        }
    }
}

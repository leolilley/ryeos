//! Deterministic controller-side provider fixture for the joined external
//! candidate acceptance. It speaks the ordinary structured-session JSON-RPC
//! boundary and obtains remote execution exclusively from the exact command
//! environment rendered by the signed provider-configuration adapter.

use std::collections::BTreeMap;
use std::io::{BufRead as _, Read as _, Write as _};

use anyhow::{Context as _, Result, ensure};
use serde::Deserialize;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Environments {
    default: String,
    include_local: bool,
    environments: Vec<CommandEnvironment>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CommandEnvironment {
    id: String,
    program: String,
    env: BTreeMap<String, String>,
}

struct RemoteConnector {
    process: lillux::SubordinateProcess,
    input: Option<lillux::SubordinateProcessInput>,
    output: lillux::SubordinateProcessOutput,
    error: Option<lillux::task::HostTask<std::io::Result<Vec<u8>>>>,
}

impl RemoteConnector {
    fn start() -> Result<Self> {
        let home =
            std::env::var_os("FIXTURE_HOME").context("joined provider has no FIXTURE_HOME")?;
        let home = lillux::PinnedDirectory::open(std::path::Path::new(&home))?
            .context("joined provider home is missing")?;
        let configuration = home
            .open_pinned_regular(std::ffi::OsStr::new("environments.toml"), false)?
            .context("admitted provider configuration is absent")?
            .read_bounded(
                ryeos_external_execution_contract::MAX_PROVIDER_CONFIGURATION_BYTES as u64,
            )
            .context("read admitted provider configuration")?;
        let configuration = String::from_utf8(configuration)
            .context("admitted provider configuration is not UTF-8")?;
        let configuration: Environments =
            toml::from_str(&configuration).context("decode admitted provider configuration")?;
        ensure!(
            !configuration.include_local && configuration.environments.len() == 1,
            "provider configuration admits a local fallback or ambiguous environment"
        );
        let environment = &configuration.environments[0];
        ensure!(
            environment.id == configuration.default,
            "provider configuration default differs from its only environment"
        );
        let mut request = lillux::SubordinateProcessRequest {
            cmd: String::new(),
            argv0: None,
            args: Vec::new(),
            cwd: "/".to_owned(),
            envs: environment
                .env
                .iter()
                .map(|(name, value)| (name.clone(), value.clone()))
                .collect(),
            limits: Some(lillux::SubprocessLimits {
                max_open_files: Some(64),
                ..lillux::SubprocessLimits::default()
            }),
            inherited_fds: Vec::new(),
        };
        request.cmd = environment.program.clone();
        let mut process = lillux::SubordinateProcess::spawn(request)
            .map_err(anyhow::Error::msg)
            .context("start exact external connector")?;
        let input = process
            .take_input()
            .map_err(anyhow::Error::msg)
            .context("take subordinate process input")?;
        let output = process
            .take_output()
            .map_err(anyhow::Error::msg)
            .context("take subordinate process output")?;
        let mut error = process
            .take_error()
            .map_err(anyhow::Error::msg)
            .context("take subordinate process error")?;
        let error =
            lillux::task::spawn_host_task("synthetic-external-connector-stderr", move || {
                let mut bytes = Vec::new();
                error
                    .by_ref()
                    .take(64 * 1024 + 1)
                    .read_to_end(&mut bytes)
                    .map(|_| bytes)
            })
            .context("start private connector stderr drain")?;
        Ok(Self {
            process,
            input: Some(input),
            output,
            error: Some(error),
        })
    }

    fn exchange(&mut self, request: &serde_json::Value) -> Result<serde_json::Value> {
        let deadline =
            lillux::time::MonotonicDeadline::after(lillux::time::Duration::from_secs(30));
        let mut frame = serde_json::to_vec(request)?;
        frame.push(b'\n');
        self.input
            .as_mut()
            .context("connector input is closed")?
            .write_all_until(&frame, deadline)
            .context("write external connector request")?;
        let response = self
            .output
            .read_frame_until(b'\n', 1024 * 1024, deadline)
            .context("read external connector response")?;
        ensure!(
            !response.is_empty(),
            "external connector closed before its protocol response"
        );
        serde_json::from_slice(&response).context("decode remote protocol response")
    }

    fn close(mut self) -> Result<()> {
        drop(self.input.take());
        let status = self
            .process
            .wait_exact_child()
            .map_err(anyhow::Error::msg)
            .context("wait for exact external connector")?;
        ensure!(status.success, "external connector exited with {status:?}");
        let error = self
            .error
            .take()
            .context("connector error owner is absent")?
            .join()
            .map_err(|_| anyhow::anyhow!("connector error owner panicked"))??;
        ensure!(
            error.is_empty(),
            "external connector emitted private diagnostics"
        );
        Ok(())
    }

    fn failure_class(mut self) -> String {
        drop(self.input.take());
        let exit = self.process.wait_exact_child();
        let Some(task) = self.error.take() else {
            return "connector_diagnostic_owner_missing".to_owned();
        };
        let diagnostic = match task.join() {
            Err(_) => return "connector_diagnostic_task_failed".to_owned(),
            Ok(Err(_)) => return "connector_diagnostic_read_failed".to_owned(),
            Ok(Ok(bytes)) => bytes,
        };
        classify_connector_exit(exit, &diagnostic).to_owned()
    }
}

fn classify_connector_exit(
    exit: Result<lillux::SubordinateProcessExit, String>,
    diagnostic: &[u8],
) -> &'static str {
    let Ok(exit) = exit else {
        return "connector_exit_observation_failed";
    };
    if diagnostic.len() > 64 * 1024 {
        return "connector_diagnostic_limit_exceeded";
    }
    let diagnostic = String::from_utf8_lossy(&diagnostic);
    if diagnostic.contains("connect protected external candidate controller")
        && diagnostic.contains("No such file or directory")
    {
        "connector_controller_endpoint_missing"
    } else if diagnostic.contains("connect protected external candidate controller")
        && diagnostic.contains("Permission denied")
    {
        "connector_controller_endpoint_refused"
    } else if diagnostic.contains("connect protected external candidate controller") {
        "connector_controller_connect_failed"
    } else if diagnostic.contains("missing RYEOS_EXTERNAL_CONNECTOR_") {
        "connector_environment_missing"
    } else if diagnostic.contains("external connector was not authenticated") {
        "connector_authentication_refused"
    } else if diagnostic.contains("external connector controller reported a closed fault") {
        "connector_controller_fault"
    } else if !diagnostic.is_empty() {
        "connector_failed_with_unclassified_diagnostic"
    } else if exit.code.is_none() {
        "connector_terminated_by_signal"
    } else if exit.success {
        "connector_clean_exit_before_response"
    } else {
        "connector_failed_without_diagnostic"
    }
}

#[cfg(test)]
mod diagnostic_tests {
    use super::*;

    #[test]
    fn observed_exit_and_private_diagnostics_remain_distinct() {
        let failed = lillux::SubordinateProcessExit {
            success: false,
            code: Some(126),
        };
        for (diagnostic, expected) in [
            (
                "connect protected external candidate controller",
                "connector_controller_connect_failed",
            ),
            (
                "connect protected external candidate controller: No such file or directory",
                "connector_controller_endpoint_missing",
            ),
            (
                "connect protected external candidate controller: Permission denied",
                "connector_controller_endpoint_refused",
            ),
            (
                "missing RYEOS_EXTERNAL_CONNECTOR_ENDPOINT",
                "connector_environment_missing",
            ),
            (
                "external connector was not authenticated",
                "connector_authentication_refused",
            ),
            (
                "external connector controller reported a closed fault",
                "connector_controller_fault",
            ),
            (
                "other private diagnostic",
                "connector_failed_with_unclassified_diagnostic",
            ),
        ] {
            assert_eq!(
                classify_connector_exit(Ok(failed), diagnostic.as_bytes()),
                expected
            );
        }
        assert_eq!(
            classify_connector_exit(Ok(failed), &vec![b'x'; 64 * 1024 + 1]),
            "connector_diagnostic_limit_exceeded"
        );
        assert_eq!(
            classify_connector_exit(
                Ok(lillux::SubordinateProcessExit {
                    success: false,
                    code: None
                }),
                b""
            ),
            "connector_terminated_by_signal"
        );
        assert_eq!(
            classify_connector_exit(
                Ok(lillux::SubordinateProcessExit {
                    success: true,
                    code: Some(0)
                }),
                b""
            ),
            "connector_clean_exit_before_response"
        );
    }

    #[test]
    fn private_diagnostic_never_enters_public_category() {
        for diagnostic in [
            b"secret-sentinel".as_slice(),
            b"connect protected external candidate controller: secret-sentinel".as_slice(),
        ] {
            let category = classify_connector_exit(
                Ok(lillux::SubordinateProcessExit {
                    success: false,
                    code: Some(126),
                }),
                diagnostic,
            );
            assert!(!category.contains("secret-sentinel"));
        }
        assert_eq!(
            classify_connector_exit(
                Ok(lillux::SubordinateProcessExit {
                    success: false,
                    code: Some(126)
                }),
                b"",
            ),
            "connector_failed_without_diagnostic"
        );
        assert_eq!(
            classify_connector_exit(Err("secret-sentinel".into()), b""),
            "connector_exit_observation_failed"
        );
    }
}

fn connector_failure_class(error: &anyhow::Error) -> &'static str {
    let chain = error
        .chain()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(": ");
    if chain.contains("read admitted provider configuration") {
        "configuration_read_failed"
    } else if chain.contains("decode admitted provider configuration") {
        "configuration_decode_failed"
    } else if chain.contains("start exact external connector") {
        "connector_spawn_failed"
    } else if chain.contains("take subordinate process") {
        "connector_process_setup_failed"
    } else if chain.contains("write external connector request") {
        "connector_request_write_failed"
    } else if chain.contains("read external connector response")
        || chain.contains("external connector closed before its protocol response")
    {
        "connector_response_closed"
    } else if chain.contains("decode remote protocol response") {
        "connector_response_invalid"
    } else {
        "connector_exchange_failed"
    }
}

fn main() {
    if let Err(error) = run() {
        eprintln!(
            "synthetic external provider failed: {}",
            connector_failure_class(&error)
        );
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    ensure!(
        std::env::args_os().count() == 1,
        "synthetic provider accepts no command arguments"
    );
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout().lock();
    let mut connector = None;
    for line in stdin.lock().lines() {
        let line = line?;
        let request: serde_json::Value = serde_json::from_str(&line)?;
        let id = request
            .get("id")
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("provider request has no id"))?;
        let method = request
            .get("method")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("provider request has no method"))?;
        let response = match method {
            "initialize" => serde_json::json!({
                "jsonrpc":"2.0",
                "id":id,
                "result":{"ready":true}
            }),
            "session/start" => {
                ensure!(
                    connector.is_none(),
                    "provider connector was already started"
                );
                match RemoteConnector::start() {
                    Ok(mut remote) => match remote.exchange(&request) {
                        Ok(response) => {
                            connector = Some(remote);
                            response
                        }
                        Err(_) => serde_json::json!({
                            "jsonrpc":"2.0",
                            "id":id,
                            "error":{
                                "code":-32001,
                                "message":remote.failure_class()
                            }
                        }),
                    },
                    Err(error) => serde_json::json!({
                        "jsonrpc":"2.0",
                        "id":id,
                        "error":{
                            "code":-32001,
                            "message":connector_failure_class(&error)
                        }
                    }),
                }
            }
            "turn/start" => connector
                .as_mut()
                .context("provider turn preceded session start")?
                .exchange(&request)?,
            other => anyhow::bail!("unsupported provider method `{other}`"),
        };
        serde_json::to_writer(&mut stdout, &response)?;
        stdout.write_all(b"\n")?;
        stdout.flush()?;
    }
    if let Some(connector) = connector {
        connector.close()?;
    }
    Ok(())
}

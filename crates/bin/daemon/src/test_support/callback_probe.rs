//! Daemon e2e support for tools that call back into the daemon.
//!
//! A tool launched under the default `tool_callback` protocol holds a
//! callback token and a thread-auth token and may call the daemon over its
//! UDS callback channel (bundle events, item authoring, ...). This module
//! plants one signed fixture tool that makes exactly one named callback with
//! given params, using the shipped runtime client (`ryeos_runtime`'s
//! `UdsRuntimeClient`) through the `ryeos-callback-probe` executable, and
//! prints the result as JSON. [`run_callback`] launches it through the
//! daemon and parses what it printed.
//!
//! Callback authority is minted only from a signed manifest, so
//! [`plant_probe_tool`] also plants a signed project manifest declaring
//! exactly the [`ProbeAuthority`] the test asks for, and the tool requests
//! that same authority. A test therefore chooses what the tool may do and
//! asserts what the daemon allows or refuses.
//!
//! ```ignore
//! let authority = ProbeAuthority::new()
//!     .bundle_event("probe_event", &["append", "scan"]);
//! let (h, fixture) = DaemonHarness::start_fast().await?;
//! plant_probe_tool(project, &fixture.publisher,
//!     Path::new(env!("CARGO_BIN_EXE_ryeos-callback-probe")), &authority)?;
//! let outcome = run_callback(&h, project, "bundle_events_append", params).await?;
//! ```
//!
//! The executable path comes from the test crate because only an integration
//! test sees `CARGO_BIN_EXE_ryeos-callback-probe`.

use std::path::Path;

use anyhow::{Context as _, Result, bail};
use lillux::crypto::SigningKey;
use serde_json::{Value, json};

use super::DaemonHarness;

/// Bundle id of the planted probe tool. The tool lives at
/// `tool:cbprobe/probe`, so the daemon derives this as its effective bundle
/// id, and the planted manifest carries the same name.
pub const PROBE_BUNDLE: &str = "cbprobe";

/// The item ref of the planted probe tool.
pub const PROBE_TOOL_REF: &str = "tool:cbprobe/probe";

/// The runtime authority the planted manifest declares and the probe tool
/// requests. Empty by default, so a tool holds nothing it was not given.
#[derive(Debug, Clone, Default)]
pub struct ProbeAuthority {
    bundle_events: Vec<(String, Vec<String>)>,
    item_authoring: Vec<(String, String)>,
    declared_caps: Vec<String>,
}

impl ProbeAuthority {
    pub fn new() -> Self {
        Self::default()
    }

    /// Allow bundle-event `operations` (`append`, `scan`) on `event_kind`
    /// under [`PROBE_BUNDLE`].
    pub fn bundle_event(mut self, event_kind: &str, operations: &[&str]) -> Self {
        self.bundle_events.push((
            event_kind.to_owned(),
            operations.iter().map(|op| (*op).to_owned()).collect(),
        ));
        self
    }

    /// Allow authoring items of `kind` whose bare id falls under `namespace`
    /// (a bare-id pattern such as `authored/*`).
    pub fn item_authoring(mut self, kind: &str, namespace: &str) -> Self {
        self.item_authoring
            .push((kind.to_owned(), namespace.to_owned()));
        self
    }

    /// Give the tool an ordinary declared capability, which becomes part of
    /// the authoring ceiling for items it authors.
    pub fn declared_cap(mut self, capability: &str) -> Self {
        self.declared_caps.push(capability.to_owned());
        self
    }

    fn manifest_authority_yaml(&self, indent: &str) -> String {
        let mut yaml = String::new();
        if !self.bundle_events.is_empty() {
            yaml.push_str(&format!("{indent}bundle_events:\n"));
            for (event_kind, operations) in &self.bundle_events {
                yaml.push_str(&format!(
                    "{indent}  - event_kind: {event_kind}\n{indent}    operations: [{}]\n",
                    operations.join(", ")
                ));
            }
        }
        if !self.item_authoring.is_empty() {
            yaml.push_str(&format!("{indent}item_authoring:\n"));
            for (kind, namespace) in &self.item_authoring {
                yaml.push_str(&format!(
                    "{indent}  - kind: {kind}\n{indent}    namespace: \"{namespace}\"\n"
                ));
            }
        }
        yaml
    }
}

/// Plant the signed project manifest and the probe tool under `project`.
///
/// Everything is signed by `signer` (use `fixture.publisher`). `probe_binary`
/// is the `ryeos-callback-probe` executable the tool runs.
pub fn plant_probe_tool(
    project: &Path,
    signer: &SigningKey,
    probe_binary: &Path,
    authority: &ProbeAuthority,
) -> Result<()> {
    let ai = project.join(".ai");
    std::fs::create_dir_all(ai.join("tools").join(PROBE_BUNDLE))
        .context("create probe tool dir")?;

    let manifest = format!(
        "name: {PROBE_BUNDLE}\nversion: 1.0.0\ndescription: callback probe fixture\n\
         provides_kinds: []\nrequires_kinds: []\nuses_kinds: []\nruntime_authority:\n{}",
        authority.manifest_authority_yaml("  ")
    );
    let manifest = if authority.bundle_events.is_empty() && authority.item_authoring.is_empty() {
        manifest.replace("runtime_authority:\n", "runtime_authority: {}\n")
    } else {
        manifest
    };
    std::fs::write(
        ai.join("manifest.yaml"),
        lillux::signature::sign_content(&manifest, signer, "#", None),
    )
    .context("write probe manifest")?;

    let command = serde_json::to_string(
        probe_binary
            .to_str()
            .context("probe binary path must be UTF-8")?,
    )?;
    let mut tool = format!(
        "category: \"{PROBE_BUNDLE}\"\nversion: \"1.0.0\"\ntool_type: \"subprocess\"\n\
         executor_id: \"@subprocess\"\n\
         execution_protocol: \"protocol:ryeos/core/tool_callback\"\n\
         description: \"callback probe fixture\"\n\
         config:\n  command: {command}\n  input_data: \"${{params_json}}\"\n  timeout_secs: 60\n\
         requires:\n  capabilities:\n"
    );
    if !authority.declared_caps.is_empty() {
        tool.push_str("    declared:\n");
        for capability in &authority.declared_caps {
            tool.push_str(&format!("      - {capability}\n"));
        }
    }
    tool.push_str("    manifest:\n      runtime_authority:\n");
    tool.push_str(&authority.manifest_authority_yaml("        "));
    if authority.bundle_events.is_empty() && authority.item_authoring.is_empty() {
        tool = tool.replace(
            "    manifest:\n      runtime_authority:\n",
            "    manifest:\n      runtime_authority: {}\n",
        );
    }
    std::fs::write(
        ai.join("tools").join(PROBE_BUNDLE).join("probe.yaml"),
        lillux::signature::sign_content(&tool, signer, "#", None),
    )
    .context("write probe tool")?;
    Ok(())
}

/// What one probe callback came back with.
#[derive(Debug, Clone, PartialEq)]
pub enum ProbeOutcome {
    /// The daemon accepted the callback; this is its result.
    Accepted(Value),
    /// The daemon refused the callback under a typed `code`.
    Refused { code: String, message: String },
}

impl ProbeOutcome {
    /// The accepted result, or a panic naming the refusal.
    pub fn accepted(self) -> Value {
        match self {
            Self::Accepted(result) => result,
            Self::Refused { code, message } => {
                panic!("callback was refused ({code}): {message}")
            }
        }
    }

    /// The refusal code, or a panic naming the accepted result.
    pub fn refusal_code(&self) -> &str {
        match self {
            Self::Refused { code, .. } => code,
            Self::Accepted(result) => panic!("callback was accepted: {result:#}"),
        }
    }
}

/// Launch the planted probe tool in `project`, making `callback` (one of
/// `bundle_events_append`, `bundle_events_read_chain`, `bundle_events_scan`,
/// `author_item`) with `params`, and parse what it printed. The launch runs
/// as the harness's signed principal, which is the tool's acting principal.
pub async fn run_callback(
    harness: &DaemonHarness,
    project: &Path,
    callback: &str,
    params: Value,
) -> Result<ProbeOutcome> {
    let (status, body) = harness
        .post_execute(
            PROBE_TOOL_REF,
            project.to_str().context("project path must be UTF-8")?,
            json!({ "callback": callback, "params": params }),
        )
        .await?;
    if !status.is_success() {
        bail!("launching {PROBE_TOOL_REF} failed with {status}: {body:#}");
    }
    parse_probe_outcome(&body)
}

/// Find the probe's one-line JSON report anywhere in an execute response.
/// The tool's stdout may be carried as a string or already decoded.
pub fn parse_probe_outcome(body: &Value) -> Result<ProbeOutcome> {
    let report = find_report(body)
        .with_context(|| format!("no probe report in the execute response: {body:#}"))?;
    if report.get("ok").and_then(Value::as_bool) == Some(true) {
        return Ok(ProbeOutcome::Accepted(
            report.get("result").cloned().unwrap_or(Value::Null),
        ));
    }
    let text = |key: &str| {
        report
            .get(key)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned()
    };
    Ok(ProbeOutcome::Refused {
        code: text("code"),
        message: text("message"),
    })
}

fn find_report(value: &Value) -> Option<Value> {
    match value {
        Value::Object(map) => {
            if map.get("ryeos_callback_probe") == Some(&Value::Bool(true)) {
                return Some(value.clone());
            }
            map.values().find_map(find_report)
        }
        Value::Array(items) => items.iter().find_map(find_report),
        Value::String(text) => text
            .lines()
            .filter_map(|line| serde_json::from_str::<Value>(line.trim()).ok())
            .find_map(|parsed| find_report(&parsed)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_an_accepted_report_carried_as_stdout_text() {
        let body = json!({"result": {"stdout":
            "{\"ryeos_callback_probe\":true,\"ok\":true,\"result\":{\"n\":1}}\n"}});
        assert_eq!(
            parse_probe_outcome(&body).unwrap(),
            ProbeOutcome::Accepted(json!({"n": 1}))
        );
    }

    #[test]
    fn parses_a_refusal_report_that_is_already_decoded() {
        let body = json!({"result": {"output": {
            "ryeos_callback_probe": true, "ok": false, "code": "x", "message": "m"}}});
        assert_eq!(
            parse_probe_outcome(&body).unwrap(),
            ProbeOutcome::Refused {
                code: "x".into(),
                message: "m".into()
            }
        );
    }

    #[test]
    fn a_response_without_a_report_is_an_error() {
        assert!(parse_probe_outcome(&json!({"result": {"stdout": "hello"}})).is_err());
    }

    #[test]
    fn the_manifest_declares_what_the_authority_asks_for() {
        let authority = ProbeAuthority::new()
            .bundle_event("probe_event", &["append", "scan"])
            .item_authoring("graph", "authored/*");
        let yaml = authority.manifest_authority_yaml("  ");
        assert!(yaml.contains("event_kind: probe_event"), "{yaml}");
        assert!(yaml.contains("operations: [append, scan]"), "{yaml}");
        assert!(yaml.contains("namespace: \"authored/*\""), "{yaml}");
    }
}

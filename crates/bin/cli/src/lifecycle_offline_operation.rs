//! `ryeos node offline-operation {sign,submit,list}`: the operator's side of
//! signed offline operations delivered to a node that is not the machine the
//! operator sits at.
//!
//! `sign` builds and signs a request, offline, against facts read from the
//! node. `submit` sends a signed request to a running node's route, which
//! queues it for the next startup; the node stops for it only when the
//! operator says `--stop`. `list` reads what a node has queued, applied and
//! refused. The node decides everything: nothing here is applied by the
//! command line, and what `sign` checks the node checks again.
//!
//! With `--remote <name>` the call goes to the operator's own node instead,
//! which forwards it to that remote under the delegation the operator stored
//! for it (`ryeos remote delegate`). The remote admits the operator as the
//! principal; the operator's key signs the request and the call to its own
//! node, and never leaves this process.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};
use clap::Parser;
use ryeos_node::offline_operation::facts::MANDATORY_FACTS;
use ryeos_node::offline_operation::sign::{Delivery, RemoteSigner, SignDraft, sign_request_now};
use ryeos_node::offline_operation::table::lookup;
use serde_json::{Value, json};

/// Largest document the command reads: a delivery carries a request and every
/// input the table allows, base64 encoded.
const MAX_DOCUMENT_BYTES: u64 = 4 * 1024 * 1024;

fn read_bytes(path: &Path, what: &str) -> Result<Vec<u8>> {
    lillux::read_regular_file_bounded_no_follow(path, MAX_DOCUMENT_BYTES)
        .with_context(|| format!("read {what} {}", path.display()))
}

fn read_json(path: &Path, what: &str) -> Result<Value> {
    serde_json::from_slice(&read_bytes(path, what)?)
        .with_context(|| format!("parse {what} {}", path.display()))
}

fn write_new(path: &Path, bytes: &[u8]) -> Result<()> {
    ensure!(
        lillux::inspect_optional_entry_no_follow(path)?.is_none(),
        "{} already exists; it is not overwritten",
        path.display()
    );
    lillux::atomic_write_private(path, bytes).with_context(|| format!("write {}", path.display()))
}

fn operator_key_path(app_root: Option<PathBuf>, signing_key: Option<PathBuf>) -> Result<PathBuf> {
    match signing_key {
        Some(path) => Ok(path),
        None => Ok(crate::transport::signing::operator_signing_key_path(
            &ryeos_node::NodeConfig::load_local(app_root)?.app_root,
        )),
    }
}

// ── sign ─────────────────────────────────────────────────────────────

#[derive(Parser, Debug)]
#[command(
    name = "ryeos node offline-operation sign",
    about = "Sign an offline-operation request against a node's facts, without the node",
    long_about = "Builds and signs one request. The facts it is signed against come from the node: a file holding the output of `ryeos node offline-operation facts --json` (or of the daemon's facts route), or the volume of a node on this machine. The request is valid for one hour unless --lifetime-minutes says otherwise, up to 24 hours. A remote operator also names the grant it signs under and the origin site the grant is bound to. The output is a delivery document for `submit`; --stage-dir also writes the files a platform delivers to a node that cannot start.",
    no_binary_name = true
)]
struct SignArgs {
    /// The operation id, for example `node.policy-apply`.
    #[arg(long)]
    operation: String,

    /// JSON file with the operation's arguments object. Defaults to `{}`.
    #[arg(long)]
    arguments: Option<PathBuf>,

    /// An input file for one of the operation's slots, as `slot=path`.
    #[arg(long = "input")]
    inputs: Vec<String>,

    /// A result fact the request pins, as `fact=sha256`.
    #[arg(long = "expect")]
    expected: Vec<String>,

    /// JSON file with the node's facts, as printed by `facts --json`.
    #[arg(long, conflicts_with = "from_node")]
    facts: Option<PathBuf>,

    /// App root of a node on this machine to read the facts from.
    #[arg(long)]
    from_node: Option<PathBuf>,

    /// The node's fingerprint, when the facts file does not carry it.
    #[arg(long)]
    node: Option<String>,

    /// The signing key (PKCS#8 PEM). Defaults to the operator key under the
    /// app root.
    #[arg(long)]
    signing_key: Option<PathBuf>,

    /// App root holding the default operator key.
    #[arg(long)]
    app_root: Option<PathBuf>,

    /// Digest of the signer's grant file on the node (a remote operator).
    #[arg(long, requires = "origin_site")]
    grant_digest: Option<String>,

    /// The origin site the signer's grant is bound to (a remote operator).
    #[arg(long, requires = "grant_digest")]
    origin_site: Option<String>,

    /// How long the request stays valid, in minutes. Defaults to 60; at most
    /// 1440.
    #[arg(long)]
    lifetime_minutes: Option<u32>,

    /// Write the delivery document here instead of printing it.
    #[arg(long)]
    out: Option<PathBuf>,

    /// Also write `request.json` and `inputs/<digest>` here, the files a
    /// platform delivers to a node that cannot start.
    #[arg(long)]
    stage_dir: Option<PathBuf>,

    /// Emit JSON instead of human-readable text.
    #[arg(long)]
    json: bool,
}

fn split_pair<'a>(value: &'a str, what: &str) -> Result<(&'a str, &'a str)> {
    value
        .split_once('=')
        .filter(|(left, right)| !left.is_empty() && !right.is_empty())
        .with_context(|| format!("{what} `{value}` is not `name=value`"))
}

pub fn run_sign(argv: &[String], console: &crate::tty::Console) -> Result<()> {
    let Some(args) = crate::lifecycle_commands::parse_or_render_help::<SignArgs>(argv, console)?
    else {
        return Ok(());
    };
    let spec = lookup(&args.operation)
        .with_context(|| format!("this build has no offline operation `{}`", args.operation))?;
    let report = match (&args.facts, &args.from_node) {
        (Some(path), None) => read_json(path, "facts")?,
        (None, Some(app_root)) => {
            let app_root = ryeos_node::NodeConfig::load_local(Some(app_root.clone()))?.app_root;
            ryeos_node::offline_operation::deliver::local_facts_report(&app_root)?
        }
        _ => bail!("give exactly one of --facts and --from-node"),
    };
    let facts = report
        .get("facts")
        .and_then(Value::as_object)
        .context("the facts document has no `facts` object")?;
    let mut pins = BTreeMap::new();
    for fact in MANDATORY_FACTS.iter().chain(spec.preconditions) {
        let value = facts
            .get(*fact)
            .and_then(Value::as_str)
            .with_context(|| format!("the facts document does not report `{fact}`"))?;
        pins.insert((*fact).to_owned(), value.to_owned());
    }
    let node_fingerprint = match (&args.node, report.get("node_fingerprint")) {
        (Some(node), _) => node.clone(),
        (None, Some(Value::String(node))) => node.clone(),
        _ => bail!("the facts document does not name the node; give --node"),
    };
    let mut inputs = BTreeMap::new();
    for pair in &args.inputs {
        let (slot, path) = split_pair(pair, "--input")?;
        ensure!(
            inputs
                .insert(slot.to_owned(), read_bytes(Path::new(path), "input")?)
                .is_none(),
            "input slot `{slot}` is given twice"
        );
    }
    let mut expected_result = BTreeMap::new();
    for pair in &args.expected {
        let (fact, digest) = split_pair(pair, "--expect")?;
        expected_result.insert(fact.to_owned(), digest.to_owned());
    }
    let arguments = match &args.arguments {
        Some(path) => read_json(path, "arguments")?,
        None => json!({}),
    };
    let remote = match (args.grant_digest, args.origin_site) {
        (Some(grant_digest), Some(origin_site_id)) => Some(RemoteSigner {
            grant_digest,
            origin_site_id,
        }),
        _ => None,
    };
    let key_path = operator_key_path(args.app_root, args.signing_key)?;
    let key = lillux::crypto::load_signing_key(&key_path)
        .with_context(|| format!("load the signing key {}", key_path.display()))?;
    let delivery = sign_request_now(
        SignDraft {
            node_fingerprint,
            operation: args.operation,
            arguments,
            inputs,
            expected_result,
            pins,
            lifetime_minutes: args.lifetime_minutes,
            remote,
        },
        &key,
    )?;
    let request_digest = delivery.request.digest()?;
    let text = serde_json::to_vec_pretty(&delivery)?;
    if let Some(directory) = &args.stage_dir {
        stage(directory, &delivery)?;
    }
    if let Some(path) = &args.out {
        write_new(path, &text)?;
    }
    if args.json || (args.out.is_none() && args.stage_dir.is_none()) {
        // The delivery itself is the machine output.
        crate::tty::write_json(&delivery)?;
        return Ok(());
    }
    let mut banner = crate::tty::StatusBanner::new(crate::tty::Tone::Success, "REQUEST SIGNED");
    banner.detail = Some(format!(
        "{} for node {}",
        delivery.request.body.operation, delivery.request.body.node_fingerprint
    ));
    banner.rows = vec![
        crate::tty::Row::key_value("request", request_digest),
        crate::tty::Row::key_value("signer", delivery.request.body.signer_fingerprint.clone()),
        crate::tty::Row::key_value("not after", delivery.request.body.not_after.clone()),
    ];
    if let Some(path) = &args.out {
        banner.rows.push(crate::tty::Row::key_value(
            "delivery",
            path.display().to_string(),
        ));
    }
    if let Some(directory) = &args.stage_dir {
        banner.rows.push(crate::tty::Row::key_value(
            "staged",
            directory.display().to_string(),
        ));
    }
    console.status(&banner)?;
    Ok(())
}

/// Write the request and its inputs as a platform delivers them to a node
/// that cannot start: `request.json`, and `inputs/<digest>` for each input.
fn stage(directory: &Path, delivery: &Delivery) -> Result<()> {
    let inputs = directory.join("inputs");
    std::fs::create_dir_all(&inputs).with_context(|| format!("create {}", inputs.display()))?;
    for (digest, bytes) in delivery.input_bytes()? {
        write_new(&inputs.join(digest), &bytes)?;
    }
    write_new(
        &directory.join("request.json"),
        &delivery.request.to_bytes()?,
    )
}

// ── remote calls ─────────────────────────────────────────────────────

/// One signed call to a node's offline-operation route. `url` names the node;
/// absent, the daemon of the local app root.
async fn call(
    url: Option<&str>,
    app_root: Option<PathBuf>,
    signing_key: Option<PathBuf>,
    method: &str,
    path: &str,
    body: Option<&Value>,
) -> Result<Value> {
    let config_root = ryeos_node::NodeConfig::load_local(app_root.clone())
        .ok()
        .map(|config| config.app_root);
    let base = match url {
        Some(url) => url.to_owned(),
        None => {
            let root = config_root
                .as_deref()
                .context("no --url given and no local node to reach")?;
            crate::transport::http::resolve_daemon_url(root).await?
        }
    };
    let key_path = match (signing_key, config_root.as_deref()) {
        (Some(path), _) => path,
        (None, Some(root)) => crate::transport::signing::operator_signing_key_path(root),
        (None, None) => bail!("no --signing-key given and no local operator key to use"),
    };
    let signer = crate::transport::signing::Signer::load(&key_path)?;
    let discovered = crate::transport::discovery::discover_audience(&base).await?;
    let bytes = match body {
        Some(body) => serde_json::to_vec(body)?,
        None => Vec::new(),
    };
    let headers = signer.sign(method, path, &bytes, &discovered.principal_id)?;
    let target = format!("{}{path}", discovered.base_url.trim_end_matches('/'));
    let reply = match method {
        "POST" => crate::transport::http::post_json(&target, &headers, &bytes).await?,
        _ => crate::transport::http::get_json(&target, &headers).await?,
    };
    Ok(reply)
}

// ── through the operator's own node ─────────────────────────────────

const REMOTE_SUBMIT_SERVICE: &str = "service:remote/offline-operations-submit";
const REMOTE_FACTS_SERVICE: &str = "service:remote/offline-operations-facts";
const REMOTE_STATUS_SERVICE: &str = "service:remote/offline-operations-status";

/// One call to a service of the operator's own node, which forwards it to a
/// remote under the operator's delegation. The node's refusal comes back as
/// its stable code and the message that names the cure.
pub(crate) async fn through_own_node(
    app_root: Option<PathBuf>,
    signing_key: Option<PathBuf>,
    service: &str,
    parameters: Value,
) -> Result<Value> {
    let root = ryeos_node::NodeConfig::load_local(app_root)?.app_root;
    let key_path =
        signing_key.unwrap_or_else(|| crate::transport::signing::operator_signing_key_path(&root));
    let signer = crate::transport::signing::Signer::load(&key_path)?;
    let base = crate::transport::http::resolve_daemon_url(&root).await?;
    let discovered = crate::transport::discovery::discover_audience(&base).await?;
    let body = json!({
        "item_ref": service,
        "ref_bindings": {},
        "product_selections": [],
        "parameters": parameters,
        "execution_policy": ryeos_app::execution_policy::ExecutionPolicy::projectless(
            ryeos_app::execution_policy::ExecutionResponse::Wait,
        ),
    });
    let bytes = serde_json::to_vec(&body)?;
    let headers = signer.sign("POST", "/execute", &bytes, &discovered.principal_id)?;
    let target = format!("{}/execute", discovered.base_url.trim_end_matches('/'));
    match crate::transport::http::post_json(&target, &headers, &bytes).await {
        Ok(response) => response
            .get("result")
            .cloned()
            .with_context(|| format!("the node returned no result: {response}")),
        Err(crate::error::CliDispatchError::Transport(
            crate::error::CliTransportError::HttpError { status, body },
        )) => Err(refusal(status, &body)),
        Err(error) => Err(error.into()),
    }
}

/// A refusal from the operator's node as `code: message`, with the remote's
/// own reason when the remote refused the request.
fn refusal(status: u16, body: &str) -> anyhow::Error {
    let Ok(parsed) = serde_json::from_str::<Value>(body) else {
        return anyhow::anyhow!("the node answered HTTP {status}: {body}");
    };
    let text = |field: &str| parsed.get(field).and_then(Value::as_str);
    let code = text("code").or_else(|| text("error_code"));
    let message = text("error").or_else(|| text("message")).unwrap_or(body);
    match (code, text("reason")) {
        (Some(code), Some(reason)) => anyhow::anyhow!("{code} ({reason}): {message}"),
        (Some(code), None) => anyhow::anyhow!("{code}: {message}"),
        _ => anyhow::anyhow!("the node answered HTTP {status}: {message}"),
    }
}

/// A string field of a node's reply, or `unknown`.
fn field_text<'a>(reply: &'a Value, field: &str) -> &'a str {
    reply
        .get(field)
        .and_then(Value::as_str)
        .unwrap_or("unknown")
}

// ── facts of a remote ───────────────────────────────────────────────

/// `ryeos node offline-operation facts --remote <name>`: the remote's facts,
/// read through this operator's own node, as the document `sign --facts`
/// reads.
pub async fn run_remote_facts(
    remote: &str,
    app_root: Option<PathBuf>,
    signing_key: Option<PathBuf>,
    json_output: bool,
    console: &crate::tty::Console,
) -> Result<()> {
    let report = through_own_node(
        app_root,
        signing_key,
        REMOTE_FACTS_SERVICE,
        json!({ "remote": remote }),
    )
    .await?;
    if json_output {
        crate::tty::write_json(&report)?;
        return Ok(());
    }
    let mut banner =
        crate::tty::StatusBanner::new(crate::tty::Tone::Neutral, "OFFLINE-OPERATION FACTS");
    banner.detail = Some(format!(
        "remote {remote}, node {}",
        field_text(&report, "node_fingerprint")
    ));
    if let Some(facts) = report.get("facts").and_then(Value::as_object) {
        banner.rows = facts
            .iter()
            .map(|(fact, value)| {
                crate::tty::Row::key_value(
                    fact.as_str(),
                    value
                        .as_str()
                        .map_or_else(|| value.to_string(), str::to_owned),
                )
            })
            .collect();
    }
    console.status(&banner)?;
    Ok(())
}

// ── submit ───────────────────────────────────────────────────────────

#[derive(Parser, Debug)]
#[command(
    name = "ryeos node offline-operation submit",
    about = "Send a signed offline-operation request to a running node's route",
    long_about = "The node verifies the request exactly as it will at its next startup and queues it. It applies nothing while running. The node keeps running unless --stop is given; with --stop it makes a plain operator stop, and its supervisor (or the hosting platform) starts it again, which applies the request. The key that authenticates the delivery must be the key that signed the request. With --remote <name> the request and its input files go to this operator's own node, which forwards them to that remote under the operator's stored delegation (`ryeos remote delegate --remote <name> --scopes ryeos.execute.service.offline-operations/submit,...`); the remote records the operator as the principal and this node's site as the forwarding node.",
    no_binary_name = true
)]
struct SubmitArgs {
    /// The delivery document `sign` wrote.
    #[arg(long)]
    file: PathBuf,

    /// The node's base URL. Defaults to the daemon of the local app root.
    #[arg(long)]
    url: Option<String>,

    /// Deliver through this operator's own node to the remote of this name,
    /// under the delegation stored for it. The delegation must lend
    /// `ryeos.execute.service.offline-operations/submit`.
    #[arg(long, conflicts_with = "url")]
    remote: Option<String>,

    /// Stop the node once the request is queued, so that it is applied at the
    /// next startup. Without it the node keeps running.
    #[arg(long)]
    stop: bool,

    /// The key authenticating the delivery (PKCS#8 PEM); it must be the key
    /// that signed the request. Defaults to the operator key under the app
    /// root.
    #[arg(long)]
    signing_key: Option<PathBuf>,

    /// App root for the default URL and key.
    #[arg(long)]
    app_root: Option<PathBuf>,

    /// Emit JSON instead of human-readable text.
    #[arg(long)]
    json: bool,
}

pub async fn run_submit(argv: &[String], console: &crate::tty::Console) -> Result<()> {
    let Some(args) = crate::lifecycle_commands::parse_or_render_help::<SubmitArgs>(argv, console)?
    else {
        return Ok(());
    };
    let delivery: Delivery = serde_json::from_slice(&read_bytes(&args.file, "delivery")?)
        .with_context(|| format!("parse delivery {}", args.file.display()))?;
    let mut body = serde_json::to_value(&delivery)?;
    if args.stop {
        body["stop"] = json!("now");
    }
    let reply = match &args.remote {
        Some(remote) => {
            body["remote"] = json!(remote);
            let forwarded =
                through_own_node(args.app_root, args.signing_key, REMOTE_SUBMIT_SERVICE, body)
                    .await?;
            forwarded
                .get("result")
                .cloned()
                .context("the node returned no remote result")?
        }
        None => {
            call(
                args.url.as_deref(),
                args.app_root,
                args.signing_key,
                "POST",
                "/offline-operations",
                Some(&body),
            )
            .await?
        }
    };
    if args.json {
        crate::tty::write_json(&reply)?;
        return Ok(());
    }
    let text = |field: &str| field_text(&reply, field);
    let stopping = text("stop") == "requested";
    let mut banner = crate::tty::StatusBanner::new(
        crate::tty::Tone::Success,
        if reply.get("already_queued") == Some(&Value::Bool(true)) {
            "ALREADY QUEUED"
        } else {
            "QUEUED"
        },
    );
    banner.detail = Some(if stopping {
        "the node is stopping; it applies the request when it starts again".to_owned()
    } else {
        "the node keeps running; the request is applied at its next startup".to_owned()
    });
    banner.rows = vec![
        crate::tty::Row::key_value("operation", text("operation")),
        crate::tty::Row::key_value("request", text("request_digest")),
        crate::tty::Row::key_value("ledger head", text("ledger_head")),
    ];
    if let Some(remote) = &args.remote {
        banner
            .rows
            .push(crate::tty::Row::key_value("remote", remote.clone()));
        banner.rows.push(crate::tty::Row::key_value(
            "submitted by",
            text("submitted_by"),
        ));
        banner.rows.push(crate::tty::Row::key_value(
            "delegated via",
            text("delegated_via"),
        ));
    }
    console.status(&banner)?;
    Ok(())
}

// ── list ─────────────────────────────────────────────────────────────

#[derive(Parser, Debug)]
#[command(
    name = "ryeos node offline-operation list",
    about = "Show what a node has queued, applied and refused",
    long_about = "The ledger tip, the requests waiting in the inbox, the newest receipts and the refusals on record. Without --url it reads the volume of the local node, running or not.",
    no_binary_name = true
)]
struct ListArgs {
    /// The node's base URL. Defaults to the local node's volume.
    #[arg(long)]
    url: Option<String>,

    /// Read the remote of this name through this operator's own node, under
    /// the delegation stored for it. The delegation must lend
    /// `ryeos.execute.service.offline-operations/status`.
    #[arg(long, conflicts_with = "url")]
    remote: Option<String>,

    /// The key authenticating the call. Defaults to the operator key under
    /// the app root.
    #[arg(long)]
    signing_key: Option<PathBuf>,

    /// App root of the local node.
    #[arg(long)]
    app_root: Option<PathBuf>,

    /// Emit JSON instead of human-readable text.
    #[arg(long)]
    json: bool,
}

pub async fn run_list(argv: &[String], console: &crate::tty::Console) -> Result<()> {
    let Some(args) = crate::lifecycle_commands::parse_or_render_help::<ListArgs>(argv, console)?
    else {
        return Ok(());
    };
    let report = match (&args.remote, &args.url) {
        (Some(remote), _) => {
            through_own_node(
                args.app_root,
                args.signing_key,
                REMOTE_STATUS_SERVICE,
                json!({ "remote": remote }),
            )
            .await?
        }
        (None, Some(url)) => {
            call(
                Some(url.as_str()),
                args.app_root,
                args.signing_key,
                "GET",
                "/offline-operations",
                None,
            )
            .await?
        }
        (None, None) => {
            let app_root = ryeos_node::NodeConfig::load_local(args.app_root)?.app_root;
            ryeos_node::offline_operation::deliver::local_status_report(&app_root)?
        }
    };
    if args.json {
        crate::tty::write_json(&report)?;
        return Ok(());
    }
    let list = |field: &str| {
        report
            .get(field)
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default()
    };
    let cell = |item: &Value, field: &str| {
        item.get(field)
            .map(|value| {
                value
                    .as_str()
                    .map_or_else(|| value.to_string(), str::to_owned)
            })
            .unwrap_or_default()
    };
    let mut banner = crate::tty::StatusBanner::new(crate::tty::Tone::Neutral, "OFFLINE OPERATIONS");
    banner.detail = report
        .get("node_fingerprint")
        .and_then(Value::as_str)
        .map(str::to_owned);
    banner.rows = vec![
        crate::tty::Row::key_value("ledger head", cell(&report, "ledger_head")),
        crate::tty::Row::key_value("receipts applied", cell(&report, "ledger_sequence")),
    ];
    for item in list("queued") {
        banner.rows.push(crate::tty::Row::key_value(
            "queued",
            format!(
                "{} {}",
                cell(&item, "operation"),
                cell(&item, "request_digest")
            ),
        ));
    }
    for item in list("receipts") {
        banner.rows.push(crate::tty::Row::key_value(
            format!("receipt {}", cell(&item, "sequence")),
            format!(
                "{} {}",
                cell(&item, "operation"),
                cell(&item, "request_digest")
            ),
        ));
    }
    for item in list("refusals") {
        banner.rows.push(crate::tty::Row::key_value(
            "refused",
            format!(
                "{} x{} {}",
                cell(&item, "reason"),
                cell(&item, "occurrences"),
                cell(&item, "request_digest")
            ),
        ));
    }
    console.status(&banner)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(items: &[&str]) -> Vec<String> {
        items.iter().map(|item| (*item).to_owned()).collect()
    }

    #[test]
    fn a_grant_digest_needs_its_origin_site() {
        let error = SignArgs::try_parse_from(argv(&[
            "--operation",
            "node.allow-restart",
            "--facts",
            "facts.json",
            "--grant-digest",
            "abc",
        ]))
        .unwrap_err();
        assert!(error.to_string().contains("origin-site"), "{error}");
        SignArgs::try_parse_from(argv(&[
            "--operation",
            "node.allow-restart",
            "--facts",
            "facts.json",
            "--grant-digest",
            "abc",
            "--origin-site",
            "site:origin-a",
        ]))
        .unwrap();
    }

    #[test]
    fn facts_come_from_a_file_or_a_node_not_both() {
        assert!(
            SignArgs::try_parse_from(argv(&[
                "--operation",
                "node.allow-restart",
                "--facts",
                "facts.json",
                "--from-node",
                "/srv/node",
            ]))
            .is_err()
        );
    }

    #[test]
    fn a_pair_names_both_sides() {
        assert_eq!(split_pair("a=b", "--input").unwrap(), ("a", "b"));
        assert!(split_pair("a", "--input").is_err());
        assert!(split_pair("=b", "--input").is_err());
        assert!(split_pair("a=", "--input").is_err());
    }

    #[test]
    fn a_remote_delivery_names_no_url() {
        let remote =
            SubmitArgs::try_parse_from(argv(&["--file", "d.json", "--remote", "hosted"])).unwrap();
        assert_eq!(remote.remote.as_deref(), Some("hosted"));
        assert!(
            SubmitArgs::try_parse_from(argv(&[
                "--file",
                "d.json",
                "--remote",
                "hosted",
                "--url",
                "https://hosted.example",
            ]))
            .is_err()
        );
        assert!(
            ListArgs::try_parse_from(argv(&[
                "--remote",
                "hosted",
                "--url",
                "https://hosted.example",
            ]))
            .is_err()
        );
    }

    #[test]
    fn a_refusal_reads_as_its_code_and_cure() {
        let refused = refusal(
            403,
            r#"{"code":"remote_offline_operation_refused","reason":"stale_ledger_head","error":"remote `hosted` refused the request: sign again"}"#,
        );
        assert_eq!(
            refused.to_string(),
            "remote_offline_operation_refused (stale_ledger_head): remote `hosted` refused the request: sign again"
        );
        let missing = refusal(
            403,
            r#"{"code":"remote_delegation_missing","error":"sign one with `ryeos remote delegate --remote hosted`"}"#,
        );
        assert!(
            missing
                .to_string()
                .starts_with("remote_delegation_missing: ")
        );
        let plain = refusal(502, "Bad Gateway");
        assert!(plain.to_string().contains("HTTP 502"));
    }

    #[test]
    fn only_the_stop_flag_asks_the_node_to_stop() {
        let plain = SubmitArgs::try_parse_from(argv(&["--file", "d.json"])).unwrap();
        assert!(!plain.stop);
        let stopping = SubmitArgs::try_parse_from(argv(&["--file", "d.json", "--stop"])).unwrap();
        assert!(stopping.stop);
    }
}

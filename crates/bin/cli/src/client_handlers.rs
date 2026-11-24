use std::ffi::OsString;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use base64::Engine as _;
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};

use ryeos_engine::trust::{EndorsedItem, EndorsementBody, SignedEndorsement};

use crate::error::CliError;

const ARTIFACT_EXPORT_CHUNK_BYTES: usize = 512 * 1024;

pub(crate) async fn export_artifact(
    app_root: &Path,
    parameters: &Value,
) -> Result<Value, CliError> {
    let blob_hash = parameters
        .get("blob_hash")
        .and_then(Value::as_str)
        .ok_or_else(|| local("artifact export requires blob_hash"))?;
    validate_blob_hash(blob_hash)?;
    let destination = parameters
        .get("destination")
        .and_then(Value::as_str)
        .ok_or_else(|| local("artifact export requires destination"))?;
    let destination = absolute_destination(destination)?;
    let target_name = destination
        .file_name()
        .ok_or_else(|| local("artifact export destination has no file name"))?;
    let parent_path = destination
        .parent()
        .ok_or_else(|| local("artifact export destination has no parent"))?;
    let parent = lillux::PinnedDirectory::open(parent_path)
        .map_err(local_error)?
        .ok_or_else(|| local("artifact export destination parent does not exist"))?;
    parent.ensure_path_binding().map_err(local_error)?;
    if parent
        .entry_no_follow(target_name)
        .map_err(local_error)?
        .is_some()
    {
        return Err(local("artifact export destination already exists"));
    }

    crate::daemon_preflight::lifecycle_preflight(app_root).await?;
    let daemon_url = crate::transport::http::resolve_daemon_url(app_root).await?;
    let signer = crate::transport::signing::Signer::resolve(app_root)?;
    let discovered = crate::transport::discovery::discover_audience(&daemon_url).await?;

    let temp_name = OsString::from(format!(
        ".ryeos-artifact-export.{}.{}",
        std::process::id(),
        rand::random::<u64>()
    ));
    let mut staging = parent
        .open_regular_create(&temp_name, true, true, 0o600)
        .map_err(local_error)?;
    let transfer = async {
        let mut offset = 0_u64;
        let mut declared_total = None;
        let mut digest = Sha256::new();
        loop {
            // The artifact export is the node operator reading their own
            // node, so it uses the operator-only unscoped read.
            let body = json!({
                "object_hashes": [],
                "blob_hashes": [],
                "blob_chunk": {
                    "hash": blob_hash,
                    "offset": offset,
                    "length": ARTIFACT_EXPORT_CHUNK_BYTES,
                },
            });
            let body_bytes = serde_json::to_vec(&body).expect("JSON value serialization");
            let headers = signer.sign(
                "POST",
                "/objects/get-any",
                &body_bytes,
                &discovered.principal_id,
            )?;
            let url = format!(
                "{}/objects/get-any",
                discovered.base_url.trim_end_matches('/')
            );
            let response = crate::transport::http::post_json(&url, &headers, &body_bytes).await?;
            let chunk = response
                .get("blob_chunk")
                .and_then(Value::as_object)
                .ok_or_else(|| local("objects/get-any omitted the artifact blob chunk"))?;
            let kind = chunk.get("kind").and_then(Value::as_str).unwrap_or("");
            if kind == "missing" {
                return Err(local("artifact blob is not present on this node"));
            }
            if kind != "blob_chunk"
                || chunk.get("hash").and_then(Value::as_str) != Some(blob_hash)
                || chunk.get("offset").and_then(Value::as_u64) != Some(offset)
            {
                return Err(local("objects/get-any returned a contradictory artifact chunk"));
            }
            let total = chunk
                .get("total_size")
                .and_then(Value::as_u64)
                .ok_or_else(|| local("objects/get-any artifact chunk omitted total_size"))?;
            if declared_total
                .replace(total)
                .is_some_and(|prior| prior != total)
            {
                return Err(local("artifact size changed during export"));
            }
            let encoded = chunk
                .get("data")
                .and_then(Value::as_str)
                .ok_or_else(|| local("objects/get-any artifact chunk omitted data"))?;
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(encoded)
                .map_err(|error| local(format!("decode artifact chunk: {error}")))?;
            if bytes.len() > ARTIFACT_EXPORT_CHUNK_BYTES {
                return Err(local(
                    "objects/get-any artifact chunk exceeded its requested bound",
                ));
            }
            let end = offset
                .checked_add(u64::try_from(bytes.len()).map_err(local_error)?)
                .ok_or_else(|| local("artifact export byte count overflow"))?;
            let eof = chunk.get("eof").and_then(Value::as_bool) == Some(true);
            if end > total || eof != (end == total) || (bytes.is_empty() && !eof) {
                return Err(local("objects/get-any returned an invalid artifact byte range"));
            }
            staging.write_all(&bytes).map_err(local_error)?;
            digest.update(&bytes);
            offset = end;
            if eof {
                break;
            }
        }
        let observed = format!("{:x}", digest.finalize());
        if observed != blob_hash {
            return Err(local(
                "exported artifact bytes do not match their durable identity",
            ));
        }
        staging.sync_all().map_err(local_error)?;
        lillux::set_open_regular_file_mode(&staging, 0o644).map_err(local_error)?;
        parent.ensure_path_binding().map_err(local_error)?;
        match parent.rename_regular_child_noreplace_atomic(&temp_name, target_name, &staging) {
            Ok(()) => {}
            Err(error) if error.namespace_committed() => {
                if let Err(sync_error) = parent.sync() {
                    return Err(local(format!(
                        "artifact export committed at '{}' but directory durability is uncertain; do not retry to another destination: {error}; durability retry failed: {sync_error}",
                        destination.display()
                    )));
                }
            }
            Err(error) => return Err(local(format!("publish artifact export: {error}"))),
        }
        Ok::<u64, CliError>(offset)
    }
    .await;

    let bytes = match transfer {
        Ok(bytes) => bytes,
        Err(error) => {
            let _ = parent.remove_if_same(&temp_name, &staging);
            return Err(error);
        }
    };
    Ok(json!({
        "status": "exported",
        "blob_hash": blob_hash,
        "bytes": bytes,
        "destination": destination,
    }))
}

/// Default lifetime of a delegation signed without `--lifetime-secs`.
const DEFAULT_DELEGATION_LIFETIME_SECS: u64 = 8 * 60 * 60;

/// Sign a bounded delegation with the caller's own key and hand it to the
/// node. The node supplies the identities of both parties; the key never
/// leaves this process.
pub(crate) async fn remote_delegate(
    app_root: &Path,
    parameters: &Value,
) -> Result<Value, CliError> {
    let remote = parameters
        .get("remote")
        .and_then(Value::as_str)
        .unwrap_or("default");
    let scopes = delegation_scopes(parameters.get("scopes"))?;
    let lifetime_secs = match parameters.get("lifetime_secs") {
        None => DEFAULT_DELEGATION_LIFETIME_SECS,
        Some(Value::Number(number)) => number
            .as_u64()
            .ok_or_else(|| local("--lifetime-secs must be a positive whole number"))?,
        Some(Value::String(text)) => text
            .parse::<u64>()
            .map_err(|_| local("--lifetime-secs must be a positive whole number"))?,
        Some(_) => return Err(local("--lifetime-secs must be a positive whole number")),
    };
    if lifetime_secs == 0 {
        return Err(local("--lifetime-secs must be greater than zero"));
    }

    crate::daemon_preflight::lifecycle_preflight(app_root).await?;
    let signer = crate::transport::signing::Signer::resolve(app_root)?;
    let template = execute_service(
        app_root,
        &signer,
        json!({
            "item_ref": "service:remote/delegate",
            "ref_bindings": {},
            "product_selections": [],
            "parameters": { "remote": remote },
        }),
    )
    .await?;
    let facts = template
        .get("template")
        .ok_or_else(|| local("the node did not return delegation facts"))?;
    let field = |name: &str| {
        facts
            .get(name)
            .and_then(Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| local(format!("delegation facts omit {name}")))
    };
    let delegator = field("delegator")?;
    if delegator != format!("fp:{}", signer.fingerprint) {
        return Err(local(
            "the node names another principal as the delegator than the key this CLI signs with",
        ));
    }
    let (delegate_site, delegate_node) = (field("delegate_site")?, field("delegate_node")?);
    let (audience_site, audience_node) = (field("audience_site")?, field("audience_node")?);
    let not_after = lillux::time::unix_secs_now().saturating_add(lifetime_secs);
    let body = ryeos_node_runtime::remote_delegation::DelegationBody::new(
        &delegator,
        ryeos_node_runtime::remote_delegation::DelegationParties {
            delegate_site: &delegate_site,
            delegate_node: &delegate_node,
            audience_site: &audience_site,
            audience_node: &audience_node,
        },
        &scopes,
        not_after,
    );
    let signature = signer.sign_bytes_base64(&body.signing_bytes().map_err(local_error)?);
    let signed =
        ryeos_node_runtime::remote_delegation::SignedDelegation::with_signature(body, signature);
    execute_service(
        app_root,
        &signer,
        json!({
            "item_ref": "service:remote/delegate",
            "ref_bindings": {},
            "product_selections": [],
            "parameters": { "remote": remote, "delegation": signed },
        }),
    )
    .await
}

/// A flag that may be given once or repeated, as a list of text.
fn text_list(value: Option<&Value>, flag: &str) -> Result<Vec<String>, CliError> {
    match value {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::String(text)) => Ok(vec![text.clone()]),
        Some(Value::Array(items)) => items
            .iter()
            .map(|item| {
                item.as_str()
                    .map(str::to_owned)
                    .ok_or_else(|| local(format!("--{flag} entries must be strings")))
            })
            .collect(),
        Some(_) => Err(local(format!("--{flag} must be text"))),
    }
}

/// Sign an endorsement with the operator key, in this process, and hand it to
/// the node. Per-change: `--item` names the exact items (refs, globs or `.ai`
/// paths) in the project; their digests are read from the signed bytes on disk.
/// Standing: `--standing --kind <kind> --lifetime-secs <n>` covers a kind in
/// the project until `n` seconds pass (30 days at most); only kinds whose
/// schema allows a standing grant can be named, and the expiry is printed
/// before anything is signed. The node verifies the signature against
/// its signer registry, countersigns and stores it; the key never leaves this
/// process.
pub(crate) async fn endorse_items(app_root: &Path, parameters: &Value) -> Result<Value, CliError> {
    let project_path = parameters
        .get("project_path")
        .and_then(Value::as_str)
        .ok_or_else(|| local("endorse requires a project (--project <dir>)"))?;
    let project = std::fs::canonicalize(project_path)
        .map_err(|error| local(format!("project `{project_path}` is not readable: {error}")))?;
    let project_text = project
        .to_str()
        .ok_or_else(|| local("the project path is not UTF-8"))?
        .to_owned();
    let items = text_list(parameters.get("item"), "item")?;
    let kinds = text_list(parameters.get("kind"), "kind")?;
    let standing = parameters
        .get("standing")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let lifetime_secs = match parameters.get("lifetime_secs") {
        None | Some(Value::Null) => None,
        Some(Value::Number(number)) => Some(
            number
                .as_u64()
                .filter(|secs| *secs > 0)
                .ok_or_else(|| local("--lifetime-secs must be a positive whole number"))?,
        ),
        Some(Value::String(text)) => Some(
            text.parse::<u64>()
                .ok()
                .filter(|secs| *secs > 0)
                .ok_or_else(|| local("--lifetime-secs must be a positive whole number"))?,
        ),
        Some(_) => return Err(local("--lifetime-secs must be a positive whole number")),
    };
    if standing != items.is_empty() {
        return Err(local(
            "endorse takes either --item <ref>... (these exact items) or --standing --kind <kind>... \
             (every item of those kinds the authoring key signs in the project), not both",
        ));
    }
    if standing && kinds.is_empty() {
        return Err(local("--standing needs at least one --kind <kind>"));
    }
    if !standing && (!kinds.is_empty() || lifetime_secs.is_some()) {
        return Err(local("--kind and --lifetime-secs belong to --standing"));
    }
    if standing && lifetime_secs.is_none() {
        return Err(local(
            "a standing endorsement must end: give --lifetime-secs <n> (at most 30 days, \
             2592000 seconds)",
        ));
    }

    let described = if standing {
        Vec::new()
    } else {
        let app_root = app_root.to_path_buf();
        let project = project.clone();
        tokio::task::spawn_blocking(move || {
            ryeos_core_tools::actions::endorse::describe_project_items(&app_root, &items, &project)
        })
        .await
        .map_err(|error| local(format!("endorse worker stopped: {error}")))?
        .map_err(|error| local(format!("{error:#}")))?
    };

    crate::daemon_preflight::lifecycle_preflight(app_root).await?;
    let signer = crate::transport::signing::Signer::resolve(app_root)?;
    let facts = execute_service(
        app_root,
        &signer,
        json!({
            "item_ref": "service:endorsements/record",
            "ref_bindings": {},
            "product_selections": [],
            "parameters": {},
        }),
    )
    .await?;
    let ledger_head = facts
        .get("ledger_head")
        .and_then(Value::as_str)
        .ok_or_else(|| local("the node did not return its endorsement ledger head"))?
        .to_owned();
    if standing {
        // Refuse before signing anything what the node would refuse after.
        let allowed: Vec<&str> = facts
            .get("standing_kinds")
            .and_then(Value::as_array)
            .map(|kinds| kinds.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default();
        if let Some(kind) = kinds.iter().find(|kind| !allowed.contains(&kind.as_str())) {
            return Err(local(format!(
                "endorsement_kind_not_standing: kind `{kind}` cannot be endorsed as a standing \
                 grant (kinds that can: {}). A standing grant would trust everything the \
                 authoring key signs of it from now on; endorse its items one change at a time \
                 with `ryeos endorse --item <ref>`",
                if allowed.is_empty() {
                    "none".to_owned()
                } else {
                    allowed.join(", ")
                }
            )));
        }
        let max = facts
            .get("max_standing_lifetime_secs")
            .and_then(Value::as_u64)
            .unwrap_or(ryeos_engine::trust::MAX_STANDING_LIFETIME_SECS);
        if lifetime_secs.is_some_and(|secs| secs > max) {
            return Err(local(format!(
                "endorsement_standing_lifetime_too_long: --lifetime-secs is at most {max}"
            )));
        }
    }
    let endorser = format!("fp:{}", signer.fingerprint);
    let nonce: [u8; 16] = rand::random();
    let nonce: String = nonce.iter().map(|byte| format!("{byte:02x}")).collect();
    let now = lillux::time::unix_secs_now();
    let endorsed_at = lillux::time::iso8601_from_unix_secs(now);
    let body = if standing {
        let not_after = lifetime_secs
            .map(|secs| lillux::time::iso8601_from_unix_secs(now.saturating_add(secs)));
        if let Some(not_after) = &not_after {
            eprintln!(
                "standing endorsement of {} in {project_text}, ending {not_after}",
                kinds.join(", ")
            );
        }
        EndorsementBody::standing(
            &project_text,
            &endorser,
            kinds,
            not_after,
            &endorsed_at,
            &nonce,
            &ledger_head,
        )
    } else {
        EndorsementBody::for_items(
            &project_text,
            &endorser,
            described
                .iter()
                .map(|item| EndorsedItem {
                    item_ref: item.item_ref.clone(),
                    kind: item.kind.clone(),
                    digest: item.digest.clone(),
                })
                .collect(),
            &endorsed_at,
            &nonce,
            &ledger_head,
        )
    };
    body.validate_shape().map_err(local_error)?;
    let signature = signer.sign_bytes_base64(&body.signing_bytes().map_err(local_error)?);
    let signed = SignedEndorsement::with_signature(body, signature);
    let mut recorded = execute_service(
        app_root,
        &signer,
        json!({
            "item_ref": "service:endorsements/record",
            "ref_bindings": {},
            "product_selections": [],
            "parameters": { "endorsement": signed },
        }),
    )
    .await?;
    if let Some(digest) = recorded
        .get("endorsement_digest")
        .and_then(Value::as_str)
        .map(str::to_owned)
    {
        let announcement = announce_revised(
            app_root,
            json!({ "project_path": project_text, "endorsement": digest }),
        )
        .await;
        recorded["announcement"] = announcement;
    }
    Ok(recorded)
}

/// Tell a running node about a change this process made, so seats that show
/// the changed items can pick it up. The node verifies the change against the
/// revision record or the endorsement ledger before it announces anything.
/// Nothing needs announcing when no node runs here (no seat exists, and the
/// node announces what it finds on its next start), so that case, like any
/// failure to reach the node, is reported in the result and never fails the
/// command: the change itself is already made.
async fn announce_revised(app_root: &Path, parameters: Value) -> Value {
    if std::env::var_os("RYEOSD_URL").is_some() {
        return json!({ "status": "skipped", "reason": "the node is not this machine's own" });
    }
    let running = match ryeos_node::LocalLifecycleEnv::load(Some(app_root.to_path_buf())) {
        Ok(env) => matches!(
            ryeos_node::LifecycleController::from_env(env)
                .status()
                .await,
            Ok(ryeos_node::LifecycleStatus::Running { .. })
        ),
        Err(_) => false,
    };
    if !running {
        return json!({ "status": "skipped", "reason": "the node is not running" });
    }
    let announced: Result<Value, CliError> = async {
        let signer = crate::transport::signing::Signer::resolve(app_root)?;
        execute_service(
            app_root,
            &signer,
            json!({
                "item_ref": "service:items/revised",
                "ref_bindings": {},
                "product_selections": [],
                "parameters": parameters,
            }),
        )
        .await
    }
    .await;
    match announced {
        Ok(result) => json!({ "status": "announced", "result": result }),
        Err(error) => json!({ "status": "failed", "reason": error.to_string() }),
    }
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct SignParameters {
    #[serde(deserialize_with = "ryeos_runtime::scalar_or_vec::deserialize")]
    item_refs: Vec<String>,
    project_path: String,
}

/// Sign project items with the operator key, in this process. The node never
/// holds the key; this reads it from the operator's own key directory and
/// verifies items against the node's registered generation.
pub(crate) async fn sign_items(app_root: &Path, parameters: &Value) -> Result<Value, CliError> {
    let params: SignParameters = serde_json::from_value(parameters.clone())
        .map_err(|error| local(format!("sign parameters are invalid: {error}")))?;
    if params.item_refs.is_empty() {
        return Err(local("sign requires at least one item ref"));
    }
    let app_root_for_announce = app_root.to_path_buf();
    let project_path_for_announce = params.project_path.clone();
    let app_root = app_root.to_path_buf();
    let report = tokio::task::spawn_blocking(move || {
        ryeos_core_tools::actions::sign::run_sign_batch(
            &app_root,
            &params.item_refs,
            Some(Path::new(&params.project_path)),
            ryeos_core_tools::actions::sign::SignSource::Project,
        )
    })
    .await
    .map_err(|error| local(format!("sign worker stopped: {error}")))?
    .map_err(|error| local(format!("{error:#}")))?;
    if !report.is_total_success() {
        let failures = report
            .failed
            .iter()
            .map(|item| {
                format!(
                    "{}: {}",
                    item.item_ref,
                    item.error.as_deref().unwrap_or("signing failed")
                )
            })
            .collect::<Vec<_>>()
            .join("; ");
        return Err(local(format!(
            "{}/{} items failed validation or signing: {failures}",
            report.failed.len(),
            report.total()
        )));
    }
    let signed: Vec<String> = report
        .signed
        .iter()
        .map(|outcome| outcome.item_ref.clone())
        .collect();
    let mut value = serde_json::to_value(report).map_err(local_error)?;
    if !signed.is_empty() {
        value["announcement"] = announce_revised(
            &app_root_for_announce,
            json!({ "project_path": project_path_for_announce, "item_refs": signed }),
        )
        .await;
    }
    Ok(value)
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ItemRemoveParameters {
    item_ref: String,
    project_path: String,
}

/// Remove a project item with the operator key, in this process, and record
/// the removal as a tombstone revision signed by that key. The node never
/// holds the key; the node need not be running.
pub(crate) async fn item_remove(app_root: &Path, parameters: &Value) -> Result<Value, CliError> {
    let params: ItemRemoveParameters = serde_json::from_value(parameters.clone())
        .map_err(|error| local(format!("item remove parameters are invalid: {error}")))?;
    let app_root_for_announce = app_root.to_path_buf();
    let project_path = params.project_path.clone();
    let app_root = app_root.to_path_buf();
    let report = tokio::task::spawn_blocking(move || {
        ryeos_core_tools::actions::revision::run_item_remove(
            &app_root,
            &params.item_ref,
            Path::new(&params.project_path),
        )
    })
    .await
    .map_err(|error| local(format!("item remove worker stopped: {error}")))?
    .map_err(|error| local(format!("{error:#}")))?;
    let item_ref = report.item_ref.clone();
    let mut value = serde_json::to_value(report).map_err(local_error)?;
    value["announcement"] = announce_revised(
        &app_root_for_announce,
        json!({ "project_path": project_path, "item_refs": [item_ref] }),
    )
    .await;
    Ok(value)
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ContentPinParameters {
    item_ref: String,
    project_path: String,
    #[serde(
        default,
        rename = "id",
        deserialize_with = "ryeos_runtime::scalar_or_vec::deserialize"
    )]
    ids: Vec<String>,
    #[serde(default)]
    all: bool,
    #[serde(default)]
    update: bool,
}

/// Observe current project bytes and complete signed external-content pins
/// with the operator key, in this process.
pub(crate) async fn content_pin(app_root: &Path, parameters: &Value) -> Result<Value, CliError> {
    let params: ContentPinParameters = serde_json::from_value(parameters.clone())
        .map_err(|error| local(format!("content pin parameters are invalid: {error}")))?;
    let app_root = app_root.to_path_buf();
    let report = tokio::task::spawn_blocking(move || {
        ryeos_core_tools::actions::content_pin::run_content_pin(
            &app_root,
            &params.item_ref,
            Path::new(&params.project_path),
            &ryeos_core_tools::actions::content_pin::ContentPinOptions {
                ids: params.ids,
                all: params.all,
                update: params.update,
            },
        )
    })
    .await
    .map_err(|error| local(format!("content pin worker stopped: {error}")))?
    .map_err(|error| local(format!("{error:#}")))?;
    serde_json::to_value(report).map_err(local_error)
}

/// Export one local qualification as a testimony the operator signs here.
/// The node derives the exact unsigned statement; this process checks that it
/// is the statement the operator asked for, signs it with the operator key,
/// and hands the signed testimony back for the node to verify and submit. The
/// key never leaves this process.
pub(crate) async fn export_qualification_testimony(
    app_root: &Path,
    parameters: &Value,
) -> Result<Value, CliError> {
    let text = |name: &str| {
        parameters
            .get(name)
            .and_then(Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| local(format!("testimony export requires {name}")))
    };
    let remote = text("remote")?;
    let qualification_hash = text("qualification_hash")?;
    let acceptance_policy_ref = text("acceptance_policy_ref")?;
    let content = match parameters.get("content") {
        None | Some(Value::Null) => None,
        Some(Value::String(raw)) => Some(
            serde_json::from_str::<Value>(raw)
                .map_err(|error| local(format!("--content is not JSON: {error}")))?,
        ),
        Some(value) => Some(value.clone()),
    };
    let mut request = json!({
        "remote": remote,
        "qualification_hash": qualification_hash,
        "acceptance_policy_ref": acceptance_policy_ref,
    });
    if let Some(content) = &content {
        request["content"] = content.clone();
    }

    crate::daemon_preflight::lifecycle_preflight(app_root).await?;
    let signer = crate::transport::signing::Signer::resolve(app_root)?;
    let service_call = |parameters: Value| {
        json!({
            "item_ref": "service:external-content/export-qualification-testimony",
            "ref_bindings": {},
            "product_selections": [],
            "parameters": parameters,
        })
    };
    let prepared = execute_service(app_root, &signer, service_call(request.clone())).await?;
    let unsigned: ryeos_state::objects::Attestation = serde_json::from_value(
        prepared
            .get("unsigned_testimony")
            .cloned()
            .ok_or_else(|| local("the node did not return an unsigned testimony"))?,
    )
    .map_err(|error| local(format!("the node returned a malformed testimony: {error}")))?;
    check_prepared_testimony(
        &unsigned,
        &signer.fingerprint,
        &qualification_hash,
        &acceptance_policy_ref,
        content.as_ref(),
    )?;
    let mut signed = unsigned.clone();
    signed.signature = signer.sign_bytes_base64(&unsigned.signing_bytes().map_err(local_error)?);
    request["signed_testimony"] = serde_json::to_value(&signed).map_err(local_error)?;
    execute_service(app_root, &signer, service_call(request)).await
}

/// Refuse to sign a testimony that is not the one asked for: the signature is
/// the operator's statement, so the node cannot choose what it covers.
fn check_prepared_testimony(
    unsigned: &ryeos_state::objects::Attestation,
    operator_fingerprint: &str,
    qualification_hash: &str,
    acceptance_policy_ref: &str,
    content: Option<&Value>,
) -> Result<(), CliError> {
    use ryeos_state::external_content::products::qualification_testimony::{
        QualificationLane, REMOTE_QUALIFICATION_TESTIMONY_CLAIM,
        REMOTE_QUALIFICATION_TESTIMONY_POLICY, RemoteQualificationTestimonyBody,
    };
    let refuse = |what: &str| {
        local(format!(
            "refusing to sign: the node's testimony {what}; nothing was signed"
        ))
    };
    if !unsigned.signature.is_empty() {
        return Err(refuse("arrived already signed"));
    }
    if unsigned.issuer != format!("fp:{operator_fingerprint}") {
        return Err(refuse(
            "names another issuer than the key this CLI signs with",
        ));
    }
    if unsigned.claim != REMOTE_QUALIFICATION_TESTIMONY_CLAIM
        || unsigned.policy != REMOTE_QUALIFICATION_TESTIMONY_POLICY
    {
        return Err(refuse("is not a remote qualification testimony"));
    }
    let body =
        RemoteQualificationTestimonyBody::from_value(&unsigned.evidence).map_err(|error| {
            local(format!(
                "refusing to sign: the testimony body is invalid: {error:#}"
            ))
        })?;
    if body.qualification_attestation_hash != qualification_hash {
        return Err(refuse("speaks to another qualification than the one named"));
    }
    if body.acceptance_policy_ref != acceptance_policy_ref {
        return Err(refuse("names another acceptance policy than the one named"));
    }
    match (content, body.lane) {
        (None, QualificationLane::Product) => {}
        (Some(content), QualificationLane::Content)
            if content.get("activation_ref").and_then(Value::as_str)
                == body.activation_ref.as_deref() => {}
        _ => {
            return Err(refuse(
                "is for another lane or activation than the one named",
            ));
        }
    }
    Ok(())
}

/// Claim an admission token on a remote. Without `--as-principal` the claim
/// is the node's own, made by the node-owned `remote/admit` service. With it,
/// the claim is a `remote_principal` claim signed here with the caller's own
/// key and handed to the caller's node, which co-signs and forwards it.
pub(crate) async fn remote_admit(app_root: &Path, parameters: &Value) -> Result<Value, CliError> {
    let remote = parameters
        .get("remote")
        .and_then(Value::as_str)
        .unwrap_or("default");
    let token = parameters
        .get("token")
        .and_then(Value::as_str)
        .ok_or_else(|| local("remote admit needs --token, the one-time admission token"))?;
    let label = parameters.get("label").and_then(Value::as_str);
    let mut scopes = delegation_scopes(parameters.get("scopes"))?;
    let as_principal = match parameters.get("as_principal") {
        None | Some(Value::Null) => false,
        Some(Value::Bool(flag)) => *flag,
        Some(Value::String(text)) => text == "true",
        Some(_) => return Err(local("--as-principal takes no value")),
    };

    crate::daemon_preflight::lifecycle_preflight(app_root).await?;
    let signer = crate::transport::signing::Signer::resolve(app_root)?;
    if !as_principal {
        let mut params = json!({ "remote": remote, "token": token, "scopes": scopes });
        if let Some(label) = label {
            params["label"] = json!(label);
        }
        return execute_service(
            app_root,
            &signer,
            json!({
                "item_ref": "service:remote/admit",
                "ref_bindings": {},
                "product_selections": [],
                "parameters": params,
            }),
        )
        .await;
    }

    // The claim is signed over sorted, unique scopes.
    scopes.sort();
    scopes.dedup();
    let template = execute_service(
        app_root,
        &signer,
        json!({
            "item_ref": "service:remote/admit-principal",
            "ref_bindings": {},
            "product_selections": [],
            "parameters": { "remote": remote },
        }),
    )
    .await?;
    let facts = template
        .get("template")
        .ok_or_else(|| local("the node did not return admission facts"))?;
    let field = |name: &str| {
        facts
            .get(name)
            .and_then(Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| local(format!("admission facts omit {name}")))
    };
    let (audience, origin_site_id, public_key) = (
        field("audience")?,
        field("origin_site_id")?,
        field("public_key")?,
    );
    let signed_at = lillux::time::unix_secs_now();
    let mut nonce_bytes = [0u8; 16];
    rand::RngCore::fill_bytes(&mut rand::thread_rng(), &mut nonce_bytes);
    let nonce = base64::engine::general_purpose::STANDARD.encode(nonce_bytes);
    let claim = ryeos_api::handlers::admission_claim::claim_string(
        &audience,
        &lillux::cas::sha256_hex(token.as_bytes()),
        ryeos_node_runtime::identity::AuthorizedKeyPrincipalClass::RemotePrincipal,
        &public_key,
        Some(&origin_site_id),
        &scopes,
        signed_at,
        &nonce,
    );
    let digest = lillux::cas::sha256_hex(claim.as_bytes());
    let signature = signer.sign_bytes_base64(digest.as_bytes());
    let mut params = json!({
        "remote": remote,
        "token": token,
        "scopes": scopes,
        "signed_at": signed_at,
        "nonce": nonce,
        "signature": signature,
    });
    if let Some(label) = label {
        params["label"] = json!(label);
    }
    execute_service(
        app_root,
        &signer,
        json!({
            "item_ref": "service:remote/admit-principal",
            "ref_bindings": {},
            "product_selections": [],
            "parameters": params,
        }),
    )
    .await
}

/// `--scopes` is a comma-separated list, or a JSON array from a structured
/// overlay.
fn delegation_scopes(value: Option<&Value>) -> Result<Vec<String>, CliError> {
    let scopes: Vec<String> = match value {
        Some(Value::String(text)) => text
            .split(',')
            .map(str::trim)
            .filter(|scope| !scope.is_empty())
            .map(str::to_owned)
            .collect(),
        Some(Value::Array(items)) => items
            .iter()
            .map(|item| {
                item.as_str()
                    .map(str::to_owned)
                    .ok_or_else(|| local("--scopes entries must be strings"))
            })
            .collect::<Result<_, _>>()?,
        _ => Vec::new(),
    };
    if scopes.is_empty() {
        return Err(local(
            "a delegation needs --scopes: the capabilities this node may use for you on the remote, \
             for example --scopes ryeos.execute.service.remote/run",
        ));
    }
    Ok(scopes)
}

/// One signed `service:threads/timeline` call, returning the service's
/// response. Shared by `ryeos threads timeline` and the live view of
/// `ryeos execute`, so both read the node's one projection.
pub(crate) async fn call_timeline_service(
    signer: &crate::transport::signing::Signer,
    base_url: &str,
    audience: &str,
    parameters: Value,
) -> Result<Value, CliError> {
    let body = json!({
        "item_ref": "service:threads/timeline",
        "ref_bindings": {},
        "product_selections": [],
        "parameters": parameters,
        "execution_policy": ryeos_app::execution_policy::ExecutionPolicy::projectless(
            ryeos_app::execution_policy::ExecutionResponse::Wait,
        ),
    });
    let body_bytes = serde_json::to_vec(&body).expect("JSON value serialization");
    let headers = signer.sign("POST", "/execute", &body_bytes, audience)?;
    let url = format!("{}/execute", base_url.trim_end_matches('/'));
    let response = crate::transport::http::post_json(&url, &headers, &body_bytes).await?;
    let result = response
        .get("result")
        .cloned()
        .ok_or_else(|| local(format!("the node returned no result: {response}")))?;
    // The timeline contract has one version; a node speaking another is
    // refused, not drawn.
    ryeos_client_base::ui::timeline::require_timeline_contract(&result).map_err(local)?;
    Ok(result)
}

/// `ryeos threads timeline <chain_root_id> [--limit N] [--json]`.
///
/// Prints the chain as text, one line per entry, and returns `None`; with
/// `--json` it returns the node's response unchanged for the result printer.
pub(crate) async fn threads_timeline(
    app_root: &Path,
    console: &crate::tty::Console,
    parameters: &Value,
) -> Result<Option<Value>, CliError> {
    let object = parameters
        .as_object()
        .ok_or_else(|| local("threads timeline parameters must be an object"))?;
    if let Some(unknown) = object
        .keys()
        .find(|key| !matches!(key.as_str(), "chain_root_id" | "limit" | "json"))
    {
        return Err(local(format!(
            "threads timeline does not take --{}",
            unknown.replace('_', "-")
        )));
    }
    let chain_root_id = object
        .get("chain_root_id")
        .and_then(Value::as_str)
        .ok_or_else(|| local("threads timeline requires a chain_root_id"))?;
    let json_output = match object.get("json") {
        None | Some(Value::Null) | Some(Value::Bool(false)) => false,
        Some(Value::Bool(true)) => true,
        Some(Value::String(text)) => text != "false",
        Some(_) => return Err(local("--json takes no value")),
    };
    let mut service_parameters = json!({ "chain_root_id": chain_root_id });
    if let Some(limit) = object.get("limit").filter(|limit| !limit.is_null()) {
        service_parameters["limit"] = limit.clone();
    }

    crate::daemon_preflight::lifecycle_preflight(app_root).await?;
    let daemon_url = crate::transport::http::resolve_daemon_url(app_root).await?;
    let signer = crate::transport::signing::Signer::resolve(app_root)?;
    let discovered = crate::transport::discovery::discover_audience(&daemon_url).await?;
    let response = call_timeline_service(
        &signer,
        &discovered.base_url,
        &discovered.principal_id,
        service_parameters,
    )
    .await?;
    if json_output {
        return Ok(Some(response));
    }
    let lines = crate::timeline_text::render_response(&response, console.capabilities().color);
    for line in &lines {
        console.text(line).map_err(CliError::Io)?;
    }
    Ok(None)
}

/// One signed `/execute` call for a service, returning its result.
async fn execute_service(
    app_root: &Path,
    signer: &crate::transport::signing::Signer,
    mut body: Value,
) -> Result<Value, CliError> {
    body["execution_policy"] =
        serde_json::to_value(ryeos_app::execution_policy::ExecutionPolicy::projectless(
            ryeos_app::execution_policy::ExecutionResponse::Wait,
        ))
        .map_err(local_error)?;
    let daemon_url = crate::transport::http::resolve_daemon_url(app_root).await?;
    let discovered = crate::transport::discovery::discover_audience(&daemon_url).await?;
    let body_bytes = serde_json::to_vec(&body).expect("JSON value serialization");
    let headers = signer.sign("POST", "/execute", &body_bytes, &discovered.principal_id)?;
    let url = format!("{}/execute", discovered.base_url.trim_end_matches('/'));
    let response = crate::transport::http::post_json(&url, &headers, &body_bytes).await?;
    response
        .get("result")
        .cloned()
        .ok_or_else(|| local(format!("the node returned no result: {response}")))
}

fn absolute_destination(raw: &str) -> Result<PathBuf, CliError> {
    if raw.trim() != raw || raw.is_empty() || raw.chars().any(char::is_control) {
        return Err(local("artifact export destination is invalid"));
    }
    let path = PathBuf::from(raw);
    if path.is_absolute() {
        Ok(path)
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(path))
            .map_err(local_error)
    }
}

fn validate_blob_hash(value: &str) -> Result<(), CliError> {
    if value.len() == 64
        && value
            .as_bytes()
            .iter()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        Ok(())
    } else {
        Err(local(
            "artifact export requires a canonical SHA-256 blob hash",
        ))
    }
}

fn local(detail: impl Into<String>) -> CliError {
    CliError::Local {
        detail: detail.into(),
    }
}

fn local_error(error: impl std::fmt::Display) -> CliError {
    local(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endorse_flags_accept_one_value_or_a_list() {
        assert!(text_list(None, "item").unwrap().is_empty());
        assert_eq!(
            text_list(Some(&json!("view:a")), "item").unwrap(),
            vec!["view:a".to_owned()]
        );
        assert_eq!(
            text_list(Some(&json!(["view:a", "view:b"])), "item").unwrap(),
            vec!["view:a".to_owned(), "view:b".to_owned()]
        );
        assert!(text_list(Some(&json!([1])), "item").is_err());
        assert!(text_list(Some(&json!(true)), "item").is_err());
    }

    #[tokio::test]
    async fn endorse_refuses_a_mix_of_items_and_standing_before_touching_the_node() {
        let project = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let path = project.path().to_str().unwrap();
        for parameters in [
            json!({ "project_path": path }),
            json!({ "project_path": path, "standing": true, "item": ["view:a"], "kind": ["view"] }),
            json!({ "project_path": path, "standing": true }),
            json!({ "project_path": path, "item": ["view:a"], "kind": ["view"] }),
            json!({ "project_path": path, "item": ["view:a"], "lifetime_secs": 5 }),
            json!({ "project_path": path, "standing": true, "kind": ["view"], "lifetime_secs": 0 }),
            // A standing endorsement must end.
            json!({ "project_path": path, "standing": true, "kind": ["knowledge"] }),
        ] {
            let error = endorse_items(root.path(), &parameters).await.unwrap_err();
            assert!(
                matches!(error, CliError::Local { .. }),
                "{parameters}: {error:?}"
            );
        }
    }

    #[test]
    fn artifact_hash_requires_lowercase_sha256() {
        validate_blob_hash(&"a".repeat(64)).unwrap();
        assert!(validate_blob_hash(&"A".repeat(64)).is_err());
        assert!(validate_blob_hash("abc").is_err());
        assert!(validate_blob_hash(&"g".repeat(64)).is_err());
    }

    #[test]
    fn artifact_destination_rejects_ambiguous_text() {
        assert!(absolute_destination("").is_err());
        assert!(absolute_destination(" result.bin").is_err());
        assert!(absolute_destination("result\nbin").is_err());
        assert!(absolute_destination("result.bin").unwrap().is_absolute());
    }

    fn unsigned_testimony(issuer: &str, claim: &str) -> ryeos_state::objects::Attestation {
        ryeos_state::objects::Attestation::unsigned(
            "a".repeat(64),
            claim.to_owned(),
            "ryeos.remote_qualification_testimony.v1".to_owned(),
            "2026-11-15T00:00:00Z".to_owned(),
            Some("2026-11-16T00:00:00Z".to_owned()),
            json!({}),
        )
        .for_issuer(issuer)
        .unwrap()
    }

    #[tokio::test]
    async fn a_change_is_announced_only_to_a_node_that_is_running() {
        // No node runs under a fresh directory: the announcement is skipped and
        // says so, and nothing is raised, because the change is already made.
        if std::env::var_os("RYEOSD_URL").is_some() {
            return;
        }
        let root = tempfile::tempdir().unwrap();
        let outcome = announce_revised(
            root.path(),
            json!({ "project_path": "/nowhere", "item_refs": ["graph:a/b"] }),
        )
        .await;
        assert_eq!(outcome["status"], "skipped", "{outcome}");
        assert_eq!(outcome["reason"], "the node is not running", "{outcome}");
    }

    #[test]
    fn the_cli_signs_only_the_testimony_it_asked_for() {
        let operator = "b".repeat(64);
        let check = |testimony: &ryeos_state::objects::Attestation| {
            check_prepared_testimony(
                testimony,
                &operator,
                &"c".repeat(64),
                "config:test/policy",
                None,
            )
            .unwrap_err()
            .to_string()
        };

        // Already signed by someone.
        let mut signed = unsigned_testimony(&operator, "qualification_testimony");
        signed.signature = "AAAA".to_owned();
        assert!(check(&signed).contains("already signed"));

        // Another issuer than the key this CLI holds.
        let other = unsigned_testimony(&"d".repeat(64), "qualification_testimony");
        assert!(check(&other).contains("another issuer"));

        // Not a remote qualification testimony at all.
        let wrong_claim = unsigned_testimony(&operator, "something_else");
        assert!(check(&wrong_claim).contains("not a remote qualification testimony"));

        // A body that is not a testimony body is never signed.
        let hollow = unsigned_testimony(&operator, "qualification_testimony");
        assert!(check(&hollow).contains("testimony body is invalid"));
    }
}

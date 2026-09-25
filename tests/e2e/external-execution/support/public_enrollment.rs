//! Offline enrollment source and public-request helpers for DaemonHarness.
//!
//! The prebuilt environment-products runtime implements the two fixture
//! credential routes. It observes only a synthetic account, never real Codex
//! credentials. Its original captured product must be imported/bound to this
//! separately installed Worker before launch. This module creates no profile
//! database rows, capture witnesses, admission tokens, or placement state.
//!
//! Required caller sequence: install signed sources; publicly bind runtime;
//! create profile; launch login once; observe that exact session ready; send
//! start/read commands once and inspect their exact command observations;
//! terminate login and prove settlement; read confirming profile; confirm it.
//! Preserve launch/root on any uncertain response: these helpers never retry.

use std::collections::BTreeMap;

use anyhow::{Context as _, Result, ensure};
use lillux::crypto::SigningKey;
use ryeos_app::execution_policy::{ExecutionPolicy, ExecutionResponse};
use serde_json::{Value, json};

use crate::common::DaemonHarness;

pub const WORKER_REF: &str = "worker:fixture/enrollment";
pub const LOGIN_REF: &str = "worker_execution:fixture/login";
pub const ACCOUNT: &str = r#"{"email":"offline@example.test","type":"fixture"}"#;

macro_rules! fixture_source {
    ($file:literal) => {
        include_str!(concat!(
            "../../environment-products/qualification-bundle-overlay/codex/.ai/workers/fixture/lib/hosted/",
            $file
        ))
    };
}

/// Pure authoring: return the complete current signed source closure. Callers
/// register these in a disposable trusted bundle using the existing fixture
/// installer; do not overlay a running node or treat returned bytes as admission.
pub fn signed_sources(
    runtime_manifest_hash: &str,
    publisher: &SigningKey,
) -> Result<BTreeMap<String, Vec<u8>>> {
    ensure!(
        lillux::valid_hash(runtime_manifest_hash),
        "invalid runtime hash"
    );
    let mut profile: Value = serde_json::from_str(fixture_source!("profile.json"))?;
    // This is a newly authored current profile, not a parser compatibility path.
    profile["schema_version"] =
        json!(ryeos_engine::structured_session_profile::STRUCTURED_SESSION_PROFILE_SCHEMA_VERSION);
    profile["transport"] = json!("stdio_jsonrpc");
    profile["http_sse"] = Value::Null;
    profile["required_process_environment"] = json!([]);
    profile["auxiliary_configs"] = json!([]);
    profile["runtime_configs"] = json!([]);
    profile["external_candidate"] = Value::Null;
    // Enrollment has no model turn, candidate environment or recovery route.
    profile["route_sets"] = json!({
        "enrollment": ["credential.account.read", "credential.login.start"]
    });
    profile["routes"]
        .as_array_mut()
        .context("fixture routes")?
        .retain(|route| route["id"] != "session.run");
    let mut source = BTreeMap::from([
        (
            "baseline.json".into(),
            fixture_source!("baseline.json").as_bytes().to_vec(),
        ),
        (
            "schema/empty-request.json".into(),
            fixture_source!("schema/empty-request.json")
                .as_bytes()
                .to_vec(),
        ),
        (
            "schema/initialize-response.json".into(),
            fixture_source!("schema/initialize-response.json")
                .as_bytes()
                .to_vec(),
        ),
        (
            "schema/account-response.json".into(),
            fixture_source!("schema/account-response.json")
                .as_bytes()
                .to_vec(),
        ),
        (
            "schema/login-response.json".into(),
            fixture_source!("schema/login-response.json")
                .as_bytes()
                .to_vec(),
        ),
    ]);
    let profile = lillux::canonical_json(&profile)?.into_bytes();
    ryeos_engine::structured_session_profile::compile(&profile, &source)?;
    source.insert("profile.json".into(), profile);
    let manifest = ryeos_state::objects::SourceClosureManifest::new(
        vec![ryeos_state::objects::LogicalSourceRoot {
            id: "source".into(),
        }],
        source
            .iter()
            .map(|(path, bytes)| ryeos_state::objects::SourceClosureFile {
                root: "source".into(),
                path: path.clone(),
                blob_hash: lillux::sha256_hex(bytes),
                size: bytes.len() as u64,
                mode: ryeos_state::objects::SourceFileMode::ReadOnly,
            })
            .collect(),
    )?;
    let mut worker: Value = serde_yaml::from_str(include_str!(
        "../../environment-products/qualification-bundle-overlay/codex/.ai/workers/fixture/enrollment.yaml"
    ))?;
    worker["source"]["digest"] = json!(manifest.digest()?);
    worker["external_content"][0]["digest"] = json!(runtime_manifest_hash);
    // Current Worker composed contract requires an explicit slot declaration.
    // This fixture binds one fixed captured input, not a selectable product slot.
    worker["external_product_slots"] = json!([]);
    // This protocol REQUIRES the disposable controller's explicit preboot
    // trusted-process-group opt-in (public_enrollment_flow::install_before_start).
    // Source authoring alone does not grant it or make login an external guest.
    worker["execution_protocol"] = json!("protocol:ryeos/core/trusted_structured_session");
    worker["filesystem_authority"] = json!("node_policy");
    worker["supported_target"]["resources"] = json!([]);
    let login = include_str!(
        "../../environment-products/qualification-bundle-overlay/codex/.ai/worker-executions/fixture/login.yaml"
    );
    let login_value: Value = serde_yaml::from_str(login)?;
    ensure!(
        login_value["config"]["mode"]["kind"] == "session"
            && login_value["config"]["required_credential_state"] == "any"
            && login_value["config"]["worker_ref"] == WORKER_REF,
        "fixture enrollment execution changed"
    );
    let mut files: BTreeMap<String, Vec<u8>> = source
        .into_iter()
        .map(|(path, bytes)| (format!(".ai/workers/fixture/lib/hosted/{path}"), bytes))
        .collect();
    for (path, text) in [
        (
            ".ai/workers/fixture/enrollment.yaml",
            serde_yaml::to_string(&worker)?,
        ),
        (".ai/worker-executions/fixture/login.yaml", login.to_owned()),
    ] {
        files.insert(
            path.into(),
            lillux::signature::sign_content(&text, publisher, "#", None).into_bytes(),
        );
    }
    Ok(files)
}

pub async fn service(harness: &DaemonHarness, item: &str, parameters: Value) -> Result<Value> {
    let (status, response) = harness.post_execute(item, ".", parameters).await?;
    ensure!(
        status.is_success(),
        "public enrollment service {item} refused: {status}: {response}"
    );
    response
        .get("result")
        .cloned()
        .context("public enrollment service has no result")
}

pub async fn create_profile(harness: &DaemonHarness, profile_id: &str) -> Result<Value> {
    let result = service(
        harness,
        "service:credential-profiles/create",
        json!({"profile_id":profile_id}),
    )
    .await?;
    ensure!(
        result["profile_id"] == profile_id && result["state"] == "unauthenticated",
        "new fixture profile was not empty: {result}"
    );
    Ok(result)
}

/// Returns the accepted root, not a session-readiness assertion. The caller
/// observes this exact root via worker-executions/status before issuing commands.
pub async fn launch_login(
    harness: &DaemonHarness,
    profile_id: &str,
    launch_id: &str,
) -> Result<Value> {
    let (status, accepted) = harness.post_json("/execute/launch", json!({
        "item_ref":LOGIN_REF, "launch_id":launch_id, "ref_bindings":{},
        "parameters":{"credential_profile_id":profile_id},
        "execution_policy":ExecutionPolicy::projectless(ExecutionResponse::Accepted).exclude_operator_vault(),
    })).await.context("login acceptance uncertain; preserve launch ID, do not relaunch")?;
    ensure!(
        status == reqwest::StatusCode::ACCEPTED && accepted["launch_id"] == launch_id,
        "login was not durably accepted: {status}: {accepted}"
    );
    let root = accepted["thread_id"]
        .as_str()
        .context("login has no accepted root")?;
    ryeos_runtime::validate_runtime_thread_id(root).map_err(anyhow::Error::msg)?;
    Ok(accepted)
}

pub async fn command(
    harness: &DaemonHarness,
    root: &str,
    idempotency_key: &str,
    route_id: &str,
) -> Result<Value> {
    ensure!(
        matches!(
            route_id,
            "credential.login.start" | "credential.account.read"
        ),
        "enrollment helper accepts only its two declared routes"
    );
    service(
        harness,
        "service:worker-executions/command",
        json!({
            "chain_root_id":root, "idempotency_key":idempotency_key,
            "route_id":route_id, "payload":{},
        }),
    )
    .await
}

/// Public get -> exact confirming observation -> public confirm. The caller
/// must first settle the login execution; this function cannot manufacture an
/// observed account from the expected fixture value.
pub async fn confirm_observed_profile(harness: &DaemonHarness, profile_id: &str) -> Result<Value> {
    let observed = service(
        harness,
        "service:credential-profiles/get",
        json!({"profile_id":profile_id}),
    )
    .await?;
    let parameters = confirmation_parameters(profile_id, &observed)?;
    let confirmed = service(
        harness,
        "service:credential-profiles/confirm",
        parameters.clone(),
    )
    .await?;
    ensure!(
        confirmed["profile_id"] == profile_id
            && confirmed["state"] == "active"
            && confirmed["credential_generation"]
                .as_u64()
                .is_some_and(|n| n > 0)
            && confirmed["confirmed_account_digest"] == parameters["expected_account_digest"],
        "fixture confirmation did not return the observed active generation: {confirmed}"
    );
    Ok(confirmed)
}

pub fn confirmation_parameters(profile_id: &str, observed: &Value) -> Result<Value> {
    let account: Value = serde_json::from_str(ACCOUNT)?;
    let digest = ryeos_state::objects::canonical_value_digest(&account)?;
    ensure!(
        observed["profile_id"] == profile_id
            && observed["state"] == "confirming"
            && observed["sanitized_account"] == account
            && observed["sanitized_account_digest"] == digest,
        "profile did not authoritatively observe the exact offline fixture account"
    );
    let epoch = observed["login_epoch"]
        .as_u64()
        .filter(|n| *n > 0)
        .context("observed profile has no positive login epoch")?;
    Ok(json!({"profile_id":profile_id,"login_epoch":epoch,"expected_account_digest":digest}))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enrollment_sources_compile_current_profile_and_keep_session_separate() {
        let files = signed_sources(&"a".repeat(64), &SigningKey::from_bytes(&[42; 32]))
            .expect("current offline enrollment source compilation");
        let profile: Value =
            serde_json::from_slice(&files[".ai/workers/fixture/lib/hosted/profile.json"]).unwrap();
        assert_eq!(profile["external_candidate"], Value::Null);
        assert_eq!(profile["routes"].as_array().unwrap().len(), 2);
        assert!(profile["route_sets"].get("session").is_none());
        let login: Value =
            serde_yaml::from_slice(&files[".ai/worker-executions/fixture/login.yaml"]).unwrap();
        assert_eq!(login["config"]["mode"]["kind"], "session");
        assert_eq!(login["config"]["required_credential_state"], "any");
        let worker: Value =
            serde_yaml::from_slice(&files[".ai/workers/fixture/enrollment.yaml"]).unwrap();
        assert_eq!(worker["external_content"][0]["digest"], "a".repeat(64));
        assert_eq!(worker["external_product_slots"], json!([]));
        assert!(lillux::valid_hash(
            worker["source"]["digest"].as_str().unwrap()
        ));
        assert!(signed_sources("pending", &SigningKey::from_bytes(&[42; 32])).is_err());
    }

    #[test]
    fn enrollment_authored_sources_include_current_kind_required_fields() {
        let files = signed_sources(&"a".repeat(64), &SigningKey::from_bytes(&[42; 32])).unwrap();
        for (path, schema) in [
            (
                ".ai/workers/fixture/enrollment.yaml",
                include_str!(
                    "../../../../bundles/core/.ai/node/engine/kinds/worker/worker.kind-schema.yaml"
                ),
            ),
            (
                ".ai/worker-executions/fixture/login.yaml",
                include_str!(
                    "../../../../bundles/core/.ai/node/engine/kinds/worker_execution/worker_execution.kind-schema.yaml"
                ),
            ),
        ] {
            let authored: Value = serde_yaml::from_slice(&files[path]).unwrap();
            let schema: Value = serde_yaml::from_str(schema).unwrap();
            // Presence/type regression only; this does not replace normal kind
            // resolution, signature verification or launch admission.
            for (field, contract) in schema["composed_value_contract"]["required"]
                .as_object()
                .unwrap()
            {
                let value = authored
                    .get(field)
                    .unwrap_or_else(|| panic!("{path} omits required {field}"));
                let matches = match contract["prim"].as_str().unwrap() {
                    "string" => value.is_string(),
                    "mapping" => value.is_object(),
                    "array" => value.is_array(),
                    primitive => panic!("unhandled current required primitive {primitive}"),
                };
                assert!(matches, "{path} required {field} has wrong shape");
            }
        }
    }

    #[test]
    fn confirmation_requires_observed_account_and_epoch() {
        let account: Value = serde_json::from_str(ACCOUNT).unwrap();
        let observed = json!({"profile_id":"credential:fixture","state":"confirming",
            "login_epoch":1,"sanitized_account":account,
            "sanitized_account_digest":ryeos_state::objects::canonical_value_digest(&account).unwrap()});
        assert!(confirmation_parameters("credential:fixture", &observed).is_ok());
        for (field, value) in [
            ("state", json!("unauthenticated")),
            ("login_epoch", json!(0)),
            ("sanitized_account", json!({})),
            ("sanitized_account_digest", json!("a".repeat(64))),
        ] {
            let mut changed = observed.clone();
            changed[field] = value;
            assert!(confirmation_parameters("credential:fixture", &changed).is_err());
        }
        assert!(confirmation_parameters("credential:other", &observed).is_err());
    }
}

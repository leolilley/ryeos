//! End-to-end: per-name ceilings on the shared sealed vault.
//!
//! The vault is one store. The node's signed `sealed_vault_names` policy says,
//! per name, who may receive the value into a launch and who may set or
//! delete it; a name with no entry is operator-only. These tests prove, on a
//! real daemon, that a second principal holding the vault service
//! capabilities and the operator-vault capability:
//!
//! 1. cannot overwrite a name whose writers list only the operator, and the
//!    stored value is unchanged;
//! 2. cannot have that name delivered into its launch, and the provider is
//!    never contacted;
//! 3. receives it once the operator lists it as a recipient and the node
//!    restarts on the new policy;
//! 4. never sees a name it may neither receive nor write in `vault/list`.

use ryeosd::test_support as common;

use std::path::Path;
use std::time::Duration;

use common::DaemonHarness;
use common::fast_fixture::{
    FastFixture, register_config_fixture_bundle, register_workflow_bundles,
};
use common::mock_provider::{MockProvider, MockResponse};
use lillux::crypto::SigningKey;
use ryeos_app::execution_policy::{
    ExecutionPolicy, ExecutionResponse, LIVE_PROJECT_WRITE_CAPABILITY, OPERATOR_VAULT_CAPABILITY,
};
use serde_json::{Value, json};

const SECRET_NAME: &str = "ZEN_API_KEY";
const OTHER_NAME: &str = "UNLISTED_API_KEY";
const ITEM_REF: &str = "directive:test/vault_ceiling";

fn principal_of(key: &SigningKey) -> String {
    format!(
        "fp:{}",
        lillux::signature::compute_fingerprint(&key.verifying_key())
    )
}

/// Replace the node-signed `sealed_vault_names` section, as
/// `ryeos node policy-apply sealed_vault_names` would on a stopped node.
fn write_sealed_vault_names(
    state_path: &Path,
    node: &SigningKey,
    recipients: &[&str],
    writers: &[&str],
) -> anyhow::Result<()> {
    let list = |principals: &[&str]| json!({"principals": principals, "capabilities": []});
    let body = serde_yaml::to_string(&json!({
        "schema": 1,
        "names": [{
            "name": SECRET_NAME,
            "recipients": list(recipients),
            "writers": list(writers),
        }],
    }))?;
    let signed = lillux::signature::sign_content(&body, node, "#", None);
    std::fs::write(
        state_path.join(".ai/node/policies/sealed_vault_names.yaml"),
        signed,
    )?;
    Ok(())
}

fn plant_mock_provider(
    root: &Path,
    mock_base_url: &str,
    signer: &SigningKey,
) -> anyhow::Result<()> {
    let dir = root.join(".ai/config/ryeos-runtime/model-providers");
    std::fs::create_dir_all(&dir)?;
    let body = format!(
        r#"family: chat_completions
transport:
  kind: remote_http
  base_url: "{mock_base_url}"
body_template:
  model: "{{{{model}}}}"
  messages: "{{{{messages}}}}"
  tools: "{{{{tools}}}}"
  stream: "{{{{stream}}}}"
auth:
  env_var: "{SECRET_NAME}"
  header_name: "Authorization"
  prefix: "Bearer "
headers: {{}}
schemas:
  streaming:
    mode: delta_merge
  output_limit: {{path: max_tokens, semantics: provider_native_output_tokens}}
pricing:
  input_per_million: "0.0"
  output_per_million: "0.0"
"#
    );
    let signed = lillux::signature::sign_content(&body, signer, "#", None);
    std::fs::write(dir.join("mock.yaml"), signed)?;
    Ok(())
}

fn plant_project(root: &Path, signer: &SigningKey) -> anyhow::Result<()> {
    let dir = root.join(".ai/config/ryeos-runtime");
    std::fs::create_dir_all(&dir)?;
    let routing = "tiers:\n  general:\n    provider: mock\n    model: mock-model\n    context_window: 200000\n";
    std::fs::write(
        dir.join("model_routing.yaml"),
        lillux::signature::sign_content(routing, signer, "#", None),
    )?;
    let directive_dir = root.join(".ai/directives/test");
    std::fs::create_dir_all(&directive_dir)?;
    let body = format!(
        "---\nname: vault_ceiling\ncategory: \"test\"\ndescription: \"Vault ceiling e2e fixture\"\n\
         inputs:\n  - name: name\n    type: string\n    required: true\nmodel:\n  tier: general\n\
         required_secrets:\n  - {SECRET_NAME}\n---\nSay hello to ${{inputs.name}}.\n"
    );
    std::fs::write(
        directive_dir.join("vault_ceiling.md"),
        lillux::signature::sign_content(&body, signer, "<!--", Some("-->")),
    )?;
    Ok(())
}

fn plant_sealed_vault_secrets(
    state_path: &Path,
    secrets: &std::collections::HashMap<String, String>,
) -> anyhow::Result<()> {
    let vault_dir = state_path
        .join(ryeos_engine::AI_DIR)
        .join("node")
        .join("vault");
    let sk = lillux::vault::VaultSecretKey::generate();
    lillux::vault::write_secret_key(&vault_dir.join("private_key.pem"), &sk)?;
    let pk = sk.public_key();
    lillux::vault::write_public_key(&vault_dir.join("public_key.pem"), &pk)?;
    let store_path = ryeos_app::vault::default_sealed_store_path(state_path);
    ryeos_app::vault::write_sealed_secrets(&store_path, &pk, secrets)?;
    Ok(())
}

/// A signed `/execute` call as `key`, returning the status and JSON body.
async fn execute_as(
    h: &DaemonHarness,
    key: &SigningKey,
    body: Value,
) -> (reqwest::StatusCode, Value) {
    let bytes = serde_json::to_vec(&body).unwrap();
    let node_key = h.node_key.as_ref().expect("fast fixture node key");
    let mut request = reqwest::Client::new()
        .post(format!("http://{}/execute", h.bind))
        .header("content-type", "application/json")
        .body(bytes.clone());
    for (name, value) in
        common::build_signed_headers_for_bytes(key, node_key, "POST", "/execute", &bytes)
    {
        request = request.header(name, value);
    }
    let response = tokio::time::timeout(Duration::from_secs(60), request.send())
        .await
        .expect("/execute timed out")
        .expect("/execute send failed");
    let status = response.status();
    let value = response.json().await.unwrap_or(Value::Null);
    (status, value)
}

async fn service_as(
    h: &DaemonHarness,
    key: &SigningKey,
    service: &str,
    parameters: Value,
) -> (reqwest::StatusCode, Value) {
    execute_as(
        h,
        key,
        json!({
            "item_ref": service,
            "ref_bindings": {},
            "parameters": parameters,
            "execution_policy": ExecutionPolicy::projectless(ExecutionResponse::Wait),
        }),
    )
    .await
}

async fn launch_as(
    h: &DaemonHarness,
    key: &SigningKey,
    project: &Path,
) -> (reqwest::StatusCode, Value) {
    execute_as(
        h,
        key,
        json!({
            "item_ref": ITEM_REF,
            "ref_bindings": {"model": ITEM_REF},
            "project_path": project.to_str().unwrap(),
            "parameters": {"name": "World"},
            "execution_policy": ExecutionPolicy::local_live(ExecutionResponse::Wait),
        }),
    )
    .await
}

fn listed_names(body: &Value) -> Vec<String> {
    let secrets = body
        .pointer("/result/secrets")
        .or_else(|| body.get("secrets"))
        .unwrap_or_else(|| panic!("vault/list body carries no secrets: {body:#}"));
    secrets
        .as_array()
        .expect("secrets is a list")
        .iter()
        .map(|name| name.as_str().expect("name is a string").to_owned())
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_second_principal_is_bounded_by_the_name_ceilings() {
    let initial = "sk-vault-ceiling-initial-0ddba11";
    let overwrite = "sk-vault-ceiling-overwrite-bad";
    let principal_b = SigningKey::from_bytes(&[0x5a_u8; 32]);
    let b_for_plant = principal_b.clone();

    let mock = MockProvider::start(vec![
        MockResponse::Text("ok".into()),
        MockResponse::Text("ok".into()),
    ])
    .await;
    let mock_url = mock.base_url.clone();

    let plant =
        move |state_path: &Path, _user: &Path, fixture: &FastFixture| -> anyhow::Result<()> {
            register_workflow_bundles(state_path, fixture)?;
            register_config_fixture_bundle(
                state_path,
                "fixture-vault-ceiling-model-config",
                fixture,
                |bundle_root| plant_mock_provider(bundle_root, &mock_url, &fixture.publisher),
            )?;
            let mut secrets = std::collections::HashMap::new();
            secrets.insert(SECRET_NAME.to_owned(), initial.to_owned());
            secrets.insert(OTHER_NAME.to_owned(), "unlisted-value".to_owned());
            plant_sealed_vault_secrets(state_path, &secrets)?;
            // B holds every service capability, live project write and the
            // operator-vault capability: only the name ceilings stand between
            // it and the operator's secret.
            common::fast_fixture::write_authorized_key_with_scopes(
                state_path,
                &b_for_plant,
                &fixture.node,
                &[
                    "ryeos.execute.*",
                    LIVE_PROJECT_WRITE_CAPABILITY,
                    OPERATOR_VAULT_CAPABILITY,
                ],
            )?;
            // Only the operator may receive or write the secret.
            let operator = format!("fp:{}", fixture.user_fp());
            write_sealed_vault_names(state_path, &fixture.node, &[&operator], &[&operator])
        };
    let (mut h, fixture) = DaemonHarness::start_fast_with(plant, |_| {})
        .await
        .expect("start daemon with a ceilinged vault name and a second principal");
    let operator_key = h.user_key.clone().expect("fast fixture operator key");
    let operator = format!("fp:{}", fixture.user_fp());

    let project = tempfile::tempdir().expect("project tempdir");
    plant_project(project.path(), &fixture.publisher).expect("plant project");

    // 1. B may not overwrite a name whose writers list only the operator.
    let (status, body) = service_as(
        &h,
        &principal_b,
        "service:vault/set",
        json!({"name": SECRET_NAME, "value": overwrite}),
    )
    .await;
    assert_eq!(status, reqwest::StatusCode::FORBIDDEN, "{body:#}");
    assert!(
        body.to_string().contains("vault_name_not_writable"),
        "the refusal names its code: {body:#}"
    );
    // A name with no ceiling at all is not writable by B either.
    let (status, body) = service_as(
        &h,
        &principal_b,
        "service:vault/set",
        json!({"name": OTHER_NAME, "value": overwrite}),
    )
    .await;
    assert_eq!(status, reqwest::StatusCode::FORBIDDEN, "{body:#}");
    assert!(
        body.to_string().contains("vault_name_has_no_ceiling"),
        "{body:#}"
    );
    let (status, body) = service_as(
        &h,
        &principal_b,
        "service:vault/delete",
        json!({"name": SECRET_NAME}),
    )
    .await;
    assert_eq!(status, reqwest::StatusCode::FORBIDDEN, "{body:#}");

    // B sees none of the names; the operator sees both.
    let (status, body) = service_as(&h, &principal_b, "service:vault/list", json!({})).await;
    assert!(status.is_success(), "{body:#}");
    assert!(listed_names(&body).is_empty(), "{body:#}");
    let (status, body) = service_as(&h, &operator_key, "service:vault/list", json!({})).await;
    assert!(status.is_success(), "{body:#}");
    assert_eq!(
        listed_names(&body),
        vec![OTHER_NAME, SECRET_NAME],
        "{body:#}"
    );

    // 2. B's launch declaring the name under the operator vault is refused,
    // and the provider never sees the secret.
    let (status, body) = launch_as(&h, &principal_b, project.path()).await;
    assert_eq!(status, reqwest::StatusCode::FORBIDDEN, "{body:#}");
    assert!(
        body.to_string().contains("vault_name_not_delivered"),
        "the refusal names its code: {body:#}"
    );
    assert!(
        mock.captured_headers().await.is_empty(),
        "a refused principal must never reach the provider with the secret"
    );

    // The value is unchanged: the operator's run delivers the original.
    let (status, body) = launch_as(&h, &operator_key, project.path()).await;
    assert_eq!(status, reqwest::StatusCode::OK, "{body:#}");
    let captured = mock.captured_headers().await;
    assert_eq!(
        captured
            .first()
            .and_then(|headers| headers.get("authorization")),
        Some(&format!("Bearer {initial}")),
        "{captured:#?}"
    );

    // 3. The operator lists B as a recipient; after the node restarts on the
    // new policy B's launch is delivered the name. B stays no writer.
    h.kill_daemon().await.expect("stop the daemon");
    let principal_b_id = principal_of(&principal_b);
    let mut recipients = [operator.as_str(), principal_b_id.as_str()];
    recipients.sort();
    write_sealed_vault_names(&h.state_path, &fixture.node, &recipients, &[&operator])
        .expect("apply the new ceiling");
    h.respawn_with(|_| {})
        .await
        .expect("restart on the new policy");

    let (status, body) = launch_as(&h, &principal_b, project.path()).await;
    assert_eq!(status, reqwest::StatusCode::OK, "{body:#}");
    let captured = mock.captured_headers().await;
    assert_eq!(captured.len(), 2, "{captured:#?}");
    assert_eq!(
        captured[1].get("authorization"),
        Some(&format!("Bearer {initial}")),
        "{captured:#?}"
    );
    let (status, body) = service_as(
        &h,
        &principal_b,
        "service:vault/set",
        json!({"name": SECRET_NAME, "value": overwrite}),
    )
    .await;
    assert_eq!(status, reqwest::StatusCode::FORBIDDEN, "{body:#}");
    let (status, body) = service_as(&h, &principal_b, "service:vault/list", json!({})).await;
    assert!(status.is_success(), "{body:#}");
    assert_eq!(listed_names(&body), vec![SECRET_NAME], "{body:#}");

    drop(project);
    drop(mock);
}

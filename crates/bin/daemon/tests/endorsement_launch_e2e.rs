//! End to end: an endorsement carries into a launch.
//!
//! A directive whose context references a knowledge item the authoring key
//! signed is refused before the model while the item is unendorsed. Once an
//! operator endorses the item's exact digest (signing in this process, as
//! `ryeos endorse` does), the same launch runs: the daemon's launch floor and
//! the launch envelope the runtime receives both carry the endorsement the
//! request froze. Revoking the endorsement refuses the next launch again.

use ryeosd::test_support as common;

use std::path::Path;

use common::DaemonHarness;
use common::fast_fixture::{
    FastFixture, register_config_fixture_bundle, register_workflow_bundles,
};
use common::mock_provider::{MockProvider, MockResponse};
use lillux::crypto::SigningKey;

fn plant_mock_provider(
    user_space: &Path,
    mock_base_url: &str,
    signer: &SigningKey,
) -> anyhow::Result<()> {
    let dir = user_space.join(".ai/config/ryeos-runtime/model-providers");
    std::fs::create_dir_all(&dir)?;
    let body = format!(
        r#"family: chat_completions
transport:
  kind: remote_http
  base_url: "{mock_base_url}"
body_template:
  model: "{{model}}"
  messages: "{{messages}}"
  tools: "{{tools}}"
  stream: "{{stream}}"
auth: {{}}
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

fn plant_model_routing(user_space: &Path, signer: &SigningKey) -> anyhow::Result<()> {
    let dir = user_space.join(".ai/config/ryeos-runtime");
    std::fs::create_dir_all(&dir)?;
    let body = r#"tiers:
  general:
    provider: mock
    model: mock-model
    context_window: 200000
"#;
    let signed = lillux::signature::sign_content(body, signer, "#", None);
    std::fs::write(dir.join("model_routing.yaml"), signed)?;
    Ok(())
}

/// Plant a knowledge item at `<user>/.ai/knowledge/<rel>.md`.
/// Knowledge items use ```yaml code fences for metadata (parsed by
/// parser:ryeos/core/markdown/frontmatter), not --- frontmatter.
fn plant_knowledge_item(
    user_space: &Path,
    rel_path: &str,
    extra_yaml: &str,
    body: &str,
    signer: &SigningKey,
) -> anyhow::Result<()> {
    let path = user_space.join(format!(".ai/knowledge/{rel_path}.md"));
    std::fs::create_dir_all(path.parent().expect("knowledge parent dir"))?;
    let stem = Path::new(rel_path)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or(rel_path);
    let dir_relative = Path::new(rel_path)
        .parent()
        .and_then(|p| p.to_str())
        .filter(|s| !s.is_empty())
        .unwrap_or("");
    let content = format!(
        r#"```yaml
name: {stem}
category: "{dir_relative}"
{extra_yaml}```

{body}
"#
    );
    // Knowledge .md files use the `<!--` envelope
    let signed = lillux::signature::sign_content(&content, signer, "<!--", Some("-->"));
    std::fs::write(&path, signed)?;
    Ok(())
}

/// Plant a directive with a `context:` block referencing knowledge items.
fn plant_directive_with_context(
    user_space: &Path,
    rel_path: &str,
    body_text: &str,
    context_system: &[&str],
    signer: &SigningKey,
) -> anyhow::Result<()> {
    let path = user_space.join(format!(".ai/directives/{rel_path}.md"));
    std::fs::create_dir_all(path.parent().expect("directive parent dir"))?;

    let context_block = if context_system.is_empty() {
        String::new()
    } else {
        let refs = context_system
            .iter()
            .map(|r| format!("    - \"{r}\"\n"))
            .collect::<String>();
        format!("context:\n  system:\n{refs}")
    };

    let stem = Path::new(rel_path)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or(rel_path);
    let dir_relative = Path::new(rel_path)
        .parent()
        .and_then(|p| p.to_str())
        .filter(|s| !s.is_empty())
        .unwrap_or("");
    let body = format!(
        r#"---
name: {stem}
category: "{dir_relative}"
description: "Knowledge context rendering e2e fixture"
inputs:
  - name: name
    type: string
    required: true
model:
  tier: general
{context_block}---
{body_text}
"#
    );
    let signed = lillux::signature::sign_content(&body, signer, "<!--", Some("-->"));
    std::fs::write(&path, signed)?;
    Ok(())
}

const NOTE_REF: &str = "knowledge:test/referenced_note";

fn note_digest(project: &Path) -> String {
    let content = std::fs::read_to_string(project.join(".ai/knowledge/test/referenced_note.md"))
        .expect("planted note");
    ryeos_engine::item_resolution::parse_signature_header(
        &content,
        &ryeos_engine::contracts::SignatureEnvelope {
            prefix: "<!--".to_owned(),
            suffix: Some("-->".to_owned()),
            after_shebang: false,
        },
    )
    .expect("signature header")
    .content_hash
}

async fn service(
    h: &DaemonHarness,
    service: &str,
    params: serde_json::Value,
) -> (u16, serde_json::Value) {
    let (status, body) = h
        .post_execute(service, ".", params)
        .await
        .expect("post /execute");
    (status.as_u16(), body)
}

async fn endorse_note(h: &DaemonHarness, fixture: &FastFixture, project: &Path) -> String {
    let (status, facts) = service(h, "service:endorsements/record", serde_json::json!({})).await;
    assert_eq!(status, 200, "{facts:#}");
    let head = facts["result"]["ledger_head"].as_str().expect("head");
    let nonce: [u8; 16] = rand::random();
    let body = ryeos_engine::trust::EndorsementBody::for_items(
        project.canonicalize().unwrap().to_str().unwrap(),
        &format!("fp:{}", fixture.user_fp()),
        vec![ryeos_engine::trust::EndorsedItem {
            item_ref: NOTE_REF.to_owned(),
            kind: "knowledge".to_owned(),
            digest: note_digest(project),
        }],
        &lillux::time::iso8601_now(),
        &nonce
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>(),
        head,
    );
    let signed = ryeos_engine::trust::SignedEndorsement::sign(body, &fixture.user).expect("sign");
    let (status, recorded) = service(
        h,
        "service:endorsements/record",
        serde_json::json!({ "endorsement": signed }),
    )
    .await;
    assert_eq!(status, 200, "{recorded:#}");
    recorded["result"]["endorsement_digest"]
        .as_str()
        .expect("digest")
        .to_owned()
}

async fn launch(h: &DaemonHarness, project: &Path) -> (reqwest::StatusCode, serde_json::Value) {
    tokio::time::timeout(
        std::time::Duration::from_secs(60),
        h.post_execute(
            "directive:test/ref_dir",
            project.to_str().unwrap(),
            serde_json::json!({"name": "test"}),
        ),
    )
    .await
    .expect("execute timed out")
    .expect("post_execute")
}

#[tokio::test(flavor = "multi_thread")]
async fn e2e_an_endorsed_authoring_signed_reference_runs_and_a_revocation_refuses_the_next_launch()
{
    let mock = MockProvider::start(vec![MockResponse::Text("Context was loaded.".into())]).await;
    let mock_url = mock.base_url.clone();
    let project = tempfile::tempdir().expect("temp project dir");
    let project_path = project.path().to_path_buf();
    let plant =
        |state_path: &Path, _user_space: &Path, fixture: &FastFixture| -> anyhow::Result<()> {
            register_workflow_bundles(state_path, fixture)?;
            register_config_fixture_bundle(
                state_path,
                "fixture-context-model-config",
                fixture,
                |bundle_root| plant_mock_provider(bundle_root, &mock_url, &fixture.publisher),
            )?;
            plant_model_routing(&project_path, &fixture.publisher)?;
            plant_knowledge_item(
                &project_path,
                "test/ref_root",
                "references:\n  - \"knowledge:test/referenced_note\"\n",
                "The root note.",
                &fixture.publisher,
            )?;
            plant_knowledge_item(
                &project_path,
                "test/referenced_note",
                "",
                "The referenced note.",
                &fixture.authoring,
            )?;
            plant_directive_with_context(
                &project_path,
                "test/ref_dir",
                "Repeat whatever context was provided.",
                &["knowledge:test/ref_root"],
                &fixture.publisher,
            )?;
            Ok(())
        };
    let (h, fixture) = DaemonHarness::start_fast_with(plant, |cmd| {
        cmd.env("RUST_LOG", "info");
    })
    .await
    .expect("daemon should start");

    // Unendorsed: a machine draft never reaches the model.
    let (status, body) = launch(&h, project.path()).await;
    assert_eq!(status, reqwest::StatusCode::FORBIDDEN, "{body:#}");
    assert!(
        mock.captured_bodies().await.is_empty(),
        "a refused launch must not reach the model"
    );

    // Endorsed: the same launch runs, daemon and runtime agreeing.
    let endorsement = endorse_note(&h, &fixture, project.path()).await;
    let (status, body) = launch(&h, project.path()).await;
    assert_eq!(status, reqwest::StatusCode::OK, "{body:#}");
    assert_eq!(
        body.pointer("/result/success")
            .and_then(serde_json::Value::as_bool),
        Some(true),
        "{body:#}"
    );
    let requests = serde_json::Value::Array(mock.captured_bodies().await).to_string();
    assert!(requests.contains("The referenced note."), "{requests}");

    // Revoked: the next launch is refused again.
    let (status, revoked) = service(
        &h,
        "service:endorsements/revoke",
        serde_json::json!({ "endorsement": endorsement }),
    )
    .await;
    assert_eq!(status, 200, "{revoked:#}");
    let (status, body) = launch(&h, project.path()).await;
    assert_eq!(status, reqwest::StatusCode::FORBIDDEN, "{body:#}");
}

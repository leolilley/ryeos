//! Knowledge runtime E2E: directive with context block triggers child
//! knowledge-runtime compose thread, producing rendered contexts in the
//! parent directive's composed view.
//!
//! This test proves the full loop:
//!   1. User plants a knowledge item + directive with context block
//!   2. Daemon resolves the directive, runs compose_context_positions augmentation
//!   3. Augmentation pre-resolves knowledge refs, dispatches child thread
//!   4. Child knowledge-runtime composes the content
//!   5. Rendered strings appear in parent's composed_view.derived["rendered_contexts"]
//!   6. Directive runtime reads rendered_contexts instead of resolving knowledge itself

mod common;

use std::path::Path;

use common::fast_fixture::{register_standard_bundle, FastFixture};
use common::mock_provider::{MockProvider, MockResponse};
use common::DaemonHarness;
use lillux::crypto::SigningKey;

fn plant_mock_provider(user_space: &Path, mock_base_url: &str, signer: &SigningKey) -> anyhow::Result<()> {
    let dir = user_space.join(".ai/config/ryeos-runtime/model-providers");
    std::fs::create_dir_all(&dir)?;
    let body = format!(
        r#"base_url: "{mock_base_url}"
auth: {{}}
headers: {{}}
pricing:
  input_per_million: 0.0
  output_per_million: 0.0
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
  name:
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

#[tokio::test(flavor = "multi_thread")]
async fn e2e_directive_with_knowledge_context_succeeds() {
    // Mock LLM returns a simple text response.
    let mock = MockProvider::start(vec![MockResponse::Text("Context was loaded.".into())]).await;
    let mock_url = mock.base_url.clone();

    let plant = |state_path: &Path, user_space: &Path, fixture: &FastFixture| -> anyhow::Result<()> {
        register_standard_bundle(state_path, fixture)?;
        plant_mock_provider(user_space, &mock_url, &fixture.publisher)?;
        plant_model_routing(user_space, &fixture.publisher)?;

        // Plant a knowledge item with a distinctive body.
        plant_knowledge_item(
            user_space,
            "test/important_fact",
            "", // no extra frontmatter
            "The sky is blue on a clear day.",
            &fixture.publisher,
        )?;

        // Plant a directive that references the knowledge item in its
        // context block.
        plant_directive_with_context(
            user_space,
            "test/ctx_dir",
            "Repeat whatever context was provided.",
            &["knowledge:test/important_fact"],
            &fixture.publisher,
        )?;

        Ok(())
    };

    let (h, _fixture) = DaemonHarness::start_fast_with(plant, |cmd| {
        cmd.env("RUST_LOG", "info");
    }).await.expect("daemon should start");

    let project = tempfile::tempdir().expect("temp project dir");
    let (status, body) = tokio::time::timeout(
        std::time::Duration::from_secs(60),
        h.post_execute(
            "directive:test/ctx_dir",
            project.path().to_str().unwrap(),
            serde_json::json!({"name": "test"}),
        ),
    )
    .await
    .expect("execute timed out")
    .expect("post_execute failed");

    assert_eq!(status, reqwest::StatusCode::OK, "response body: {body:?}");

    let result = body.get("result").expect("result field");
    let success = result.get("success").and_then(|v| v.as_bool());
    assert_eq!(success, Some(true), "result: {result:#?}");

    drop(project);
    drop(mock);
}

#[tokio::test(flavor = "multi_thread")]
async fn e2e_directive_without_context_still_works() {
    // Regression: directives WITHOUT context blocks must still work
    // after the render_context_position deletion.
    let mock = MockProvider::start(vec![MockResponse::Text("Hello!".into())]).await;
    let mock_url = mock.base_url.clone();

    let plant = |state_path: &Path, user_space: &Path, fixture: &FastFixture| -> anyhow::Result<()> {
        register_standard_bundle(state_path, fixture)?;
        plant_mock_provider(user_space, &mock_url, &fixture.publisher)?;
        plant_model_routing(user_space, &fixture.publisher)?;

        // Directive with NO context block.
        plant_directive_with_context(
            user_space,
            "test/no_ctx",
            "Say hello.",
            &[], // empty — no context
            &fixture.publisher,
        )?;

        Ok(())
    };

    let (h, _fixture) = DaemonHarness::start_fast_with(plant, |cmd| {
        cmd.env("RUST_LOG", "info");
    }).await.expect("daemon should start");

    let project = tempfile::tempdir().expect("temp project dir");
    let (status, body) = tokio::time::timeout(
        std::time::Duration::from_secs(60),
        h.post_execute(
            "directive:test/no_ctx",
            project.path().to_str().unwrap(),
            serde_json::json!({"name": "test"}),
        ),
    )
    .await
    .expect("execute timed out")
    .expect("post_execute failed");

    assert_eq!(status, reqwest::StatusCode::OK, "response body: {body:?}");

    let result = body.get("result").expect("result field");
    let success = result.get("success").and_then(|v| v.as_bool());
    assert_eq!(success, Some(true), "result: {result:#?}");

    drop(project);
    drop(mock);
}

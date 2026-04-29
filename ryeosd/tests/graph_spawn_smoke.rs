//! F1 pin test — graph runtime spawn smoke.
//!
//! Verifies that the daemon can launch the graph runtime via the
//! spawn contract and the runtime exits 0 with a valid `RuntimeResult`.
//! This catches regressions in the `--project-path` arg mismatch
//! (F1 from 04b-phase4-graph-remaining.md).
//!
//! The graph is a trivial return-only node — no action dispatches,
//! so callback cap enforcement is irrelevant.

mod common;

use std::path::Path;

use common::DaemonHarness;

fn e2e_signing_key() -> lillux::crypto::SigningKey {
    lillux::crypto::SigningKey::from_bytes(&[0x77u8; 32])
}

fn write_trusted_signer(
    user_space: &Path,
    vk: &lillux::crypto::VerifyingKey,
) -> anyhow::Result<()> {
    use base64::engine::Engine as _;

    let fp = lillux::signature::compute_fingerprint(vk);
    let trust_dir = user_space.join(".ai/config/keys/trusted");
    std::fs::create_dir_all(&trust_dir)?;
    let key_b64 = base64::engine::general_purpose::STANDARD.encode(vk.as_bytes());
    let toml = format!(
        r#"version = "1.0.0"
category = "keys/trusted"
fingerprint = "{fp}"
owner = "self"
attestation = ""

[public_key]
pem = "ed25519:{key_b64}"
"#
    );
    std::fs::write(trust_dir.join(format!("{fp}.toml")), toml)?;
    Ok(())
}

/// Register the standard bundle so the daemon discovers
/// `runtime:graph-runtime`.
fn register_standard_bundle(state_path: &Path) -> anyhow::Result<()> {
    let standard = common::workspace_root().join("ryeos-bundles/standard");
    if !standard.is_dir() {
        anyhow::bail!(
            "ryeos-bundles/standard does not exist at {}",
            standard.display()
        );
    }
    let abs = standard.canonicalize()?;
    let dir = state_path.join(".ai/node/bundles");
    std::fs::create_dir_all(&dir)?;

    let body = format!(
        "section: bundles\npath: {}\n",
        abs.display()
    );
    let signed = lillux::signature::sign_content(&body, &e2e_signing_key(), "#", None);
    std::fs::write(dir.join("standard.yaml"), signed)?;
    Ok(())
}

/// Plant a trivial return-only graph in the project's `.ai/graphs/`
/// directory. No action nodes, so no callback cap enforcement needed.
fn plant_smoke_graph(project_dir: &Path) -> anyhow::Result<()> {
    let graphs_dir = project_dir.join(".ai/graphs");
    std::fs::create_dir_all(&graphs_dir)?;
    let body = r#"category: ""
version: "1.0.0"
config:
  start: done
  nodes:
    done:
      node_type: return
"#;
    let signed = lillux::signature::sign_content(body, &e2e_signing_key(), "#", None);
    std::fs::write(graphs_dir.join("smoke.yaml"), signed)?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn graph_spawn_smoke_returns_valid_result() {
    let pre_init = |state_path: &Path, user: &Path| -> anyhow::Result<()> {
        std::fs::create_dir_all(state_path)?;
        let sk = e2e_signing_key();
        write_trusted_signer(user, &sk.verifying_key())?;
        register_standard_bundle(state_path)?;
        Ok(())
    };

    let mut h = DaemonHarness::start_with_pre_init(pre_init, |cmd| {
        cmd.env(
            "RUST_LOG",
            std::env::var("RUST_LOG").unwrap_or_else(|_| {
                "info,ryeosd=debug,ryeos_graph_runtime=debug".into()
            }),
        );
    })
    .await
    .expect("start daemon with standard bundle");

    let project = tempfile::tempdir().expect("project tempdir");
    plant_smoke_graph(project.path()).expect("plant smoke graph");

    let post_fut = h.post_execute(
        "graph:smoke",
        project.path().to_str().unwrap(),
        serde_json::json!({}),
    );
    let (status, body) = match tokio::time::timeout(
        std::time::Duration::from_secs(30),
        post_fut,
    )
    .await
    {
        Ok(Ok(pair)) => pair,
        Ok(Err(e)) => panic!("post /execute failed: {e}"),
        Err(_) => {
            let stderr = h.drain_stderr_nonblocking().await;
            panic!(
                "POST /execute timed out after 30s — graph runtime hung.\n\
                 --- daemon stderr ---\n{stderr}\n\
                 state_path={}",
                h.state_path.display()
            );
        }
    };

    if status != reqwest::StatusCode::OK {
        let stderr = h.drain_stderr_nonblocking().await;
        panic!(
            "expected 200 OK from graph spawn smoke; got {status}\nbody={body:#}\n--- daemon stderr ---\n{stderr}"
        );
    }

    // The response must carry a `result` envelope with `success: true`.
    let result = match body.get("result") {
        Some(r) => r,
        None => {
            let stderr = h.drain_stderr_nonblocking().await;
            panic!(
                "response missing `result` envelope\nbody={body:#}\n--- daemon stderr ---\n{stderr}"
            );
        }
    };
    assert_eq!(
        result.get("success").and_then(|v| v.as_bool()),
        Some(true),
        "graph smoke must succeed; body={body:#}"
    );
    assert_eq!(
        result.get("status").and_then(|v| v.as_str()),
        Some("completed"),
        "graph smoke must complete; body={body:#}"
    );
}

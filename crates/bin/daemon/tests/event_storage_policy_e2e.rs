//! The `event_storage` node-policy section, end to end through a node.
//!
//! A node whose policy generation predates the section does not start: the
//! daemon names the missing section and the replacement that cures it, and
//! never falls back to a code mapping. The stopped-node
//! `ryeos node reset policy-generation` takes the section from the release's
//! profile, after which the node starts.

use ryeosd::test_support as common;

use std::ffi::OsStr;
use std::process::Stdio;

use anyhow::{Context, Result};

use common::{DaemonHarness, ryeos_binary};

/// Run one stopped-node `ryeos` command against the harness node, requiring
/// success, and return its stdout.
async fn ryeos_offline(h: &DaemonHarness, args: &[&OsStr]) -> Result<String> {
    let output = tokio::process::Command::new(ryeos_binary())
        .args(args)
        .env("RYEOS_APP_ROOT", &h.state_path)
        .env("HOME", h.user_space.path())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .output()
        .await
        .with_context(|| format!("spawn ryeos {args:?}"))?;
    anyhow::ensure!(
        output.status.success(),
        "ryeos {args:?} exited {}; stdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// The fast fixture pins its signers with placeholder event digests and no
/// key-event chain. A policy-generation replacement re-opens the node-signed
/// registry and cross-checks every record against the chain, so give the
/// stopped node the real thing: the same records, written through the
/// registry's own writer.
fn give_the_signer_registry_its_event_chain(
    state_path: &std::path::Path,
    node: &lillux::crypto::SigningKey,
    operator_fp: &str,
) -> Result<()> {
    let directory = ryeos_engine::trust::signers_dir(state_path);
    let mut signers = Vec::new();
    for entry in std::fs::read_dir(&directory).context("read the signer registry")? {
        let path = entry?.path();
        let text = std::fs::read_to_string(&path)?;
        let record: ryeos_engine::trust::SignerRecord =
            serde_yaml::from_str(&lillux::signature::strip_signature_lines(&text))
                .with_context(|| format!("parse {}", path.display()))?;
        let key = record
            .verifying_key()
            .map_err(|error| anyhow::anyhow!("{}: {error}", path.display()))?;
        signers.push((key, record.label, record.role, record.scope));
        std::fs::remove_file(&path)?;
    }
    let lock = ryeos_node_runtime::state_lock::StateLock::acquire(
        &ryeos_node_runtime::state_lock::default_lock_path(state_path),
    )?;
    ryeos_node::trust_registry::record_init_signers(
        state_path,
        &lock,
        &ryeos_node::trust_registry::Actor {
            node_key: node,
            operator: operator_fp,
            request_digest: &lillux::sha256_hex(b"event-storage-policy-e2e/signer-chain"),
        },
        &signers,
    )?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_node_without_event_storage_refuses_to_start_until_the_policy_reset() -> Result<()> {
    common::require_bundles_prepared(&["core"])?;
    let (mut h, fixture) = DaemonHarness::start_fast_core_only_prepared().await?;
    h.stop_gracefully().await?;
    give_the_signer_registry_its_event_chain(
        &h.state_path,
        &common::fast_fixture::node_signing_key(),
        &fixture.user_fp(),
    )?;

    // The generation as a node initialized before this release holds it:
    // every other section, signed, and no event_storage member.
    let section = h.state_path.join(".ai/node/policies/event_storage.yaml");
    anyhow::ensure!(
        section.is_file(),
        "a node initialized from the shipped profile holds event_storage"
    );
    std::fs::remove_file(&section).context("remove the event_storage member")?;

    let refusal = format!(
        "{:#}",
        h.respawn_with(|_| {})
            .await
            .expect_err("a generation without event_storage must not start")
    );
    anyhow::ensure!(
        refusal.contains("`event_storage`")
            && refusal.contains("ryeos node reset policy-generation"),
        "the refusal must name the missing section and its cure:\n{refusal}"
    );

    // The cure, exactly as an operator runs it: the profile source of this
    // release (narrowed as the fixture narrows its node), the node's host
    // class, and the new section named for the profile's body.
    let authored = tempfile::tempdir().context("create the profile source root")?;
    common::fast_fixture::author_direct_host_profile_source(
        &common::workspace_root().join("bundles"),
        "core",
        authored.path(),
    )?;
    let substrate: serde_json::Value = serde_json::from_slice(&std::fs::read(
        h.state_path.join(".ai/node/substrate-identity.json"),
    )?)?;
    let host_class = substrate["host_class"]
        .as_str()
        .context("the node's substrate identity records no host class")?
        .to_owned();
    // The fast fixture records no verified initialization completion, so the
    // image digest cannot be defaulted from it; name it as the operator would.
    let image_digest = substrate["image_digest"]
        .as_str()
        .context("the node's substrate identity records no image digest")?
        .to_owned();
    ryeos_offline(
        &h,
        &[
            OsStr::new("node"),
            OsStr::new("reset"),
            OsStr::new("policy-generation"),
            OsStr::new("--node-profile"),
            OsStr::new("core"),
            OsStr::new("--host-class"),
            OsStr::new(&host_class),
            OsStr::new("--substrate-image-digest"),
            OsStr::new(&image_digest),
            OsStr::new("--source"),
            authored.path().as_os_str(),
            OsStr::new("--app-root"),
            h.state_path.as_os_str(),
            OsStr::new("--reset-section"),
            OsStr::new("event_storage"),
            OsStr::new("--confirm"),
        ],
    )
    .await?;
    anyhow::ensure!(
        section.is_file(),
        "the replacement must publish the event_storage member"
    );

    h.respawn_with(|_| {}).await?;
    Ok(())
}

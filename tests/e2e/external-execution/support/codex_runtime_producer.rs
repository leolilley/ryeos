//! Disposable authoring for the existing Codex workspace-output producer.
//! No product testimony or qualification is authored by this fixture.
//! The producer input must be the self-contained static payload published by
//! scripts/populate-bundles.sh (its require_static_payload gate), not an
//! arbitrary host-linked debug binary. Execution retains enforced isolation.

use std::path::Path;

use anyhow::{Context as _, ensure};
use serde_json::{Value, json};

use crate::common::fast_fixture::{self, FastFixture};

pub const PRODUCER_REF: &str = "graph:codex/guest-runtime-production";
pub const CONSUMER_REF: &str = "tool:codex/guest-runtime/produce";
pub const RECIPE_REF: &str = "config:codex/guest-runtime-products";
pub const IMPORT_ROOT: &str = "codex-fixture-inputs";

pub struct ProductionInputs {
    pub file_sha256: String,
    pub input_manifest_hash: String,
    pub maximum_bytes: u64,
    pub total_bytes: u64,
    pub producer_sha256: String,
}

/// Pre-boot fixture staging, not product capture testimony. Read one pinned
/// descriptor under the authored bound and verify the exact bytes before any
/// private installation. Never reopen a mutable source for an unbounded copy.
fn stage_exact_input(
    source: &Path,
    incoming: &Path,
    maximum_bytes: u64,
    expected_sha256: &str,
) -> anyhow::Result<u64> {
    let source = lillux::secure_fs::open_pinned_regular_file_no_follow(source)?;
    let observed = source.observation()?;
    ensure!(
        (1..=maximum_bytes).contains(&observed.size()),
        "predownloaded Codex input must be one bounded regular file"
    );
    let bytes = source.read_stable_bounded(&observed, maximum_bytes)?;
    ensure!(
        lillux::sha256_hex(&bytes) == expected_sha256,
        "predownloaded Codex differs from authored exact bytes"
    );
    let directory = lillux::PinnedDirectory::open(incoming)?.context("private input root")?;
    let (_, copied) = directory
        .atomic_create_regular_from_reader(
            std::ffi::OsStr::new("codex"),
            &mut std::io::Cursor::new(bytes),
            maximum_bytes,
            0o755,
        )?
        .context("private Codex input already exists")?;
    ensure!(copied == observed.size(), "private Codex copy size changed");
    Ok(copied)
}

/// All host writes are pre-boot test setup. Runtime import, binding, execution,
/// and capture still go through the actual daemon's public owners.
pub fn prepare(
    repository: &Path,
    state_path: &Path,
    fixture: &FastFixture,
    codex_path: &Path,
    producer_path: &Path,
    expected_producer_sha256: &str,
    qualification: Option<(&str, &[&str])>,
) -> anyhow::Result<ProductionInputs> {
    let source_root = repository.join("bundles/codex/.ai");
    let read_source = |relative: &str| -> anyhow::Result<String> {
        Ok(lillux::signature::strip_signature_lines(
            &std::fs::read_to_string(source_root.join(relative))?,
        ))
    };
    let tool_source = read_source("tools/codex/guest-runtime/produce.yaml")?;
    let tool: Value = serde_yaml::from_str(&tool_source)?;
    let activation: Value =
        serde_yaml::from_str(&read_source("config/codex/guest-runtime-activation.yaml")?)?;
    let member = &activation["sources"][0]["members"][0];
    ensure!(
        activation["consumer_ref"] == CONSUMER_REF
            && member["path"] == "bin/codex"
            && member["executable"] == true
            && tool["filesystem_authority"] == "captured_execution"
            && tool["network_authority"] == "isolated"
            && tool["external_content"][0]["id"] == "codex-guest-input"
            && tool["external_content"][0]["kind"] == "file"
            && tool["external_content"][0]["mode"] == "pinned",
        "existing Codex producer input contract changed"
    );
    let file_sha256 = member["sha256"]
        .as_str()
        .context("authored Codex hash")?
        .to_owned();
    let maximum_bytes = member["maximum_bytes"]
        .as_u64()
        .context("authored Codex bound")?;
    let input_manifest_hash = tool["external_content"][0]["digest"]
        .as_str()
        .context("authored exact input manifest")?
        .to_owned();
    ensure!(
        lillux::valid_hash(&file_sha256)
            && lillux::valid_hash(&input_manifest_hash)
            && lillux::valid_hash(expected_producer_sha256),
        "fixture inputs require exact canonical hashes"
    );
    let mut recipe: Value =
        serde_yaml::from_str(&read_source("config/codex/guest-runtime-products.yaml")?)?;
    if let Some((policy_ref, required_claims)) = qualification {
        let relationship = &mut recipe["product_relationships"]["relationships"][0];
        ensure!(
            relationship["name"] == "runtime_to_external_authoring_worker"
                && relationship["qualification"]["policy_ref"].is_null()
                && relationship["qualification"]["required_claims"] == json!([]),
            "production relationship changed before qualification fixture authoring"
        );
        relationship["qualification"] = json!({
            "policy_ref": policy_ref,
            "required_claims": required_claims,
        });
    }
    let declarations = ryeos_state::external_content::products::ProductDeclarations::from_value(
        recipe["build_products"].clone(),
    )?;
    let relationships =
        ryeos_state::external_content::products::composition::ProductRelationships::from_value(
            recipe["product_relationships"].clone(),
        )?;
    relationships.validate_against(&declarations, "product_recipe")?;
    let runtime = declarations.select("runtime")?;
    ensure!(
        runtime.bounds.maximum_file_bytes == maximum_bytes
            && runtime.bounds.maximum_total_bytes == maximum_bytes,
        "runtime declaration no longer matches the exact activation bound"
    );

    // Keep the large executable outside the pinned project/small-CAS route.
    let incoming = state_path.join("codex-fixture-inputs");
    std::fs::create_dir(&incoming)?;
    let total_bytes = stage_exact_input(codex_path, &incoming, maximum_bytes, &file_sha256)?;

    // Register a disposable production-only Codex bundle with the unchanged
    // production definitions. It is not full Codex-bundle qualification.
    let bundle = state_path.join(".ai/bundles/codex");
    for relative in [
        "graphs/codex/guest-runtime-production.yaml",
        "tools/codex/guest-runtime/produce.yaml",
        "config/codex/guest-runtime-products.yaml",
    ] {
        let body = if qualification.is_some()
            && relative == "config/codex/guest-runtime-products.yaml"
        {
            serde_yaml::to_string(&recipe)?
        } else {
            read_source(relative)?
        };
        let destination = bundle.join(".ai").join(relative);
        std::fs::create_dir_all(destination.parent().context("fixture source parent")?)?;
        std::fs::write(
            destination,
            lillux::signature::sign_content_at(
                &body,
                &fixture.publisher,
                "#",
                None,
                fast_fixture::FAST_FIXTURE_TIME,
            ),
        )?;
    }
    fast_fixture::register_fixture_bundle(state_path, "codex", &bundle, fixture)?;

    let isolation_path = state_path.join(".ai/node/policies/isolation.yaml");
    let mut isolation_document: Value = serde_yaml::from_str(
        &lillux::signature::strip_signature_lines(&std::fs::read_to_string(&isolation_path)?),
    )?;
    let mut isolation: ryeos_engine::isolation::IsolationPolicy =
        serde_json::from_value(isolation_document["policy"].clone())?;
    // Preserve all current node bounds and scope requirements. No trusted
    // session fallback is needed for this ordinary deterministic Tool.
    ensure!(
        !isolation.trusted_process_group_sessions,
        "unexpected trusted session fixture"
    );
    isolation.mode = ryeos_engine::isolation::IsolationMode::Enforce;
    isolation.backend = Some(ryeos_isolation_protocol::IsolationBackendSelection {
        bundle: "core".into(),
        implementation: "linux-lillux".into(),
    });
    isolation.filesystem.proc_filesystem =
        ryeos_isolation_protocol::IsolationProcFilesystem::PidNamespace;
    isolation.network.mode = ryeos_engine::isolation::IsolationNetworkMode::Isolated;
    isolation.network.runtime_files.clear();
    ryeos_engine::isolation::IsolationRuntime::validate_policy(&isolation)?;
    let producer_bytes = lillux::secure_fs::read_regular_file_bounded_no_follow(
        producer_path,
        isolation.limits.verified_artifact_file_bytes,
    )?;
    let producer_sha256 = lillux::sha256_hex(&producer_bytes);
    ensure!(
        producer_sha256 == expected_producer_sha256,
        "prebuilt producer hash mismatch"
    );
    fast_fixture::install_signed_bundle_binary(
        &bundle,
        "ryeos-codex-guest-runtime-producer",
        &producer_bytes,
        &fixture.publisher,
    )?;
    isolation_document["policy"] = serde_json::to_value(isolation)?;
    std::fs::write(
        &isolation_path,
        lillux::signature::sign_content_at(
            &serde_yaml::to_string(&isolation_document)?,
            &fixture.node,
            "#",
            None,
            fast_fixture::FAST_FIXTURE_TIME,
        ),
    )?;

    use ryeos_app::node_policy::sections::external_content::{
        ExternalContentImportPolicyRecord, ExternalContentImportRoot, ManagedExternalContentPolicy,
    };
    let import_policy_path = state_path.join(".ai/node/policies/external_content.yaml");
    let mut import_policy: ExternalContentImportPolicyRecord = serde_yaml::from_str(
        &lillux::signature::strip_signature_lines(&std::fs::read_to_string(&import_policy_path)?),
    )?;
    ensure!(
        import_policy.limits.max_file_bytes >= maximum_bytes
            && import_policy.limits.max_total_bytes >= maximum_bytes,
        "current signed fixture node limits cannot admit the declared large runtime; do not widen small CAS"
    );
    let root = lillux::PinnedDirectory::open(&incoming)?.context("private input root")?;
    let (containing_device, root_inode) = root.device_inode()?;
    ensure!(
        import_policy.roots.is_empty(),
        "unexpected ambient fixture import roots"
    );
    import_policy.roots.insert(
        IMPORT_ROOT.into(),
        ExternalContentImportRoot {
            path: incoming,
            containing_device,
            root_inode,
        },
    );
    import_policy.managed_activation = ManagedExternalContentPolicy {
        enabled: false,
        limits: None,
    };
    import_policy.validate()?;
    std::fs::write(
        import_policy_path,
        lillux::signature::sign_content_at(
            &serde_yaml::to_string(&import_policy)?,
            &fixture.node,
            "#",
            None,
            fast_fixture::FAST_FIXTURE_TIME,
        ),
    )?;
    eprintln!(
        "Codex production fixture inputs: {}",
        json!({
            "codex_sha256": file_sha256, "codex_bytes": total_bytes,
            "input_manifest_hash": input_manifest_hash, "producer_sha256": producer_sha256,
            "provider_model_invocation_requested": false, "producer_network_policy": "isolated",
            "managed_activation": false,
        })
    );
    Ok(ProductionInputs {
        file_sha256,
        input_manifest_hash,
        maximum_bytes,
        total_bytes,
        producer_sha256,
    })
}

#[cfg(test)]
mod tests {
    use super::stage_exact_input;
    use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _, symlink};

    #[test]
    fn stage_exact_input_installs_exact_bytes_and_executable_mode() -> anyhow::Result<()> {
        let source = tempfile::tempdir()?;
        let incoming = tempfile::tempdir()?;
        let source_path = source.path().join("downloaded-codex");
        let bytes = b"exact fixture executable";
        std::fs::write(&source_path, bytes)?;
        std::fs::set_permissions(&source_path, std::fs::Permissions::from_mode(0o600))?;

        assert_eq!(
            stage_exact_input(
                &source_path,
                incoming.path(),
                bytes.len() as u64,
                &lillux::sha256_hex(bytes),
            )?,
            bytes.len() as u64
        );
        let installed = incoming.path().join("codex");
        let metadata = std::fs::symlink_metadata(&installed)?;
        assert!(metadata.is_file());
        assert_eq!(std::fs::read(&installed)?, bytes);
        assert_eq!(metadata.permissions().mode() & 0o7777, 0o755);
        assert_eq!(std::fs::read_dir(incoming.path())?.count(), 1);
        assert_eq!(
            std::fs::metadata(&source_path)?.permissions().mode() & 0o7777,
            0o600,
            "staging must not mutate the source"
        );
        Ok(())
    }

    #[test]
    fn stage_exact_input_refuses_oversized_or_wrong_hash_before_installation() -> anyhow::Result<()>
    {
        let source = tempfile::tempdir()?;
        let source_path = source.path().join("downloaded-codex");
        let bytes = b"bounded executable";
        std::fs::write(&source_path, bytes)?;
        for (maximum_bytes, expected_hash) in [
            (bytes.len() as u64 - 1, lillux::sha256_hex(bytes)),
            (
                bytes.len() as u64,
                lillux::sha256_hex(b"different executable"),
            ),
            (0, lillux::sha256_hex(bytes)),
        ] {
            let incoming = tempfile::tempdir()?;
            assert!(
                stage_exact_input(&source_path, incoming.path(), maximum_bytes, &expected_hash,)
                    .is_err()
            );
            assert_eq!(
                std::fs::read_dir(incoming.path())?.count(),
                0,
                "refused source must leave neither installed bytes nor temporary entries"
            );
        }
        Ok(())
    }

    #[test]
    fn stage_exact_input_refuses_symlink_source() -> anyhow::Result<()> {
        let source = tempfile::tempdir()?;
        let incoming = tempfile::tempdir()?;
        let bytes = b"otherwise valid executable";
        let target = source.path().join("actual-codex");
        let source_path = source.path().join("downloaded-codex");
        std::fs::write(&target, bytes)?;
        symlink(&target, &source_path)?;

        assert!(
            stage_exact_input(
                &source_path,
                incoming.path(),
                bytes.len() as u64,
                &lillux::sha256_hex(bytes),
            )
            .is_err()
        );
        assert_eq!(std::fs::read_dir(incoming.path())?.count(), 0);
        assert_eq!(std::fs::read(&target)?, bytes);
        Ok(())
    }

    #[test]
    fn stage_exact_input_preserves_existing_destination_inode_bytes_and_mode() -> anyhow::Result<()>
    {
        let source = tempfile::tempdir()?;
        let incoming = tempfile::tempdir()?;
        let source_path = source.path().join("downloaded-codex");
        let bytes = b"new exact executable";
        std::fs::write(&source_path, bytes)?;
        let installed = incoming.path().join("codex");
        let original = b"retained original destination";
        std::fs::write(&installed, original)?;
        std::fs::set_permissions(&installed, std::fs::Permissions::from_mode(0o640))?;
        let before = std::fs::symlink_metadata(&installed)?;

        assert!(
            stage_exact_input(
                &source_path,
                incoming.path(),
                bytes.len() as u64,
                &lillux::sha256_hex(bytes),
            )
            .is_err()
        );
        let after = std::fs::symlink_metadata(&installed)?;
        assert_eq!((after.dev(), after.ino()), (before.dev(), before.ino()));
        assert_eq!(std::fs::read(&installed)?, original);
        assert_eq!(after.permissions().mode() & 0o7777, 0o640);
        assert_eq!(std::fs::read_dir(incoming.path())?.count(), 1);
        Ok(())
    }
}

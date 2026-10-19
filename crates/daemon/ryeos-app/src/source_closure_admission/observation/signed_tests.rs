//! Real signed Bundle resolution/capture controls. Like the existing consumer
//! definition fixtures, these require populated core/graph/directive handler payloads.
//! No workload admission, workload process, credential, or provider contact is minted.

use super::*;
use lillux::crypto::SigningKey;
use ryeos_engine::trust::{TrustStore, TrustedSigner};
use ryeos_state::objects::{
    LogicalSourceRoot, SourceClosureFile, SourceClosureManifest, SourceFileMode,
};
use serde_json::json;
use std::{collections::BTreeMap, fs, path::PathBuf, sync::Arc};

const WORKER: &str = "worker:fixture/session";

/// Fields drop in declaration order: the application, then the owner that
/// closes its Store, then the directory.
struct Fixture {
    state: AppState,
    bundle: PathBuf,
    kind_root: PathBuf,
    signer: SigningKey,
    worker_body: String,
    manifest: SourceClosureManifest,
    _store_owner: crate::state_store::test_native::TestStateStore,
    _root: tempfile::TempDir,
}

impl Fixture {
    fn new() -> anyhow::Result<Self> {
        let root = tempfile::tempdir()?;
        let (store_owner, state) = crate::state::test_support::build(root.path())?;
        let bundle = root.path().join("fixture-bundle");
        let source = bundle.join(".ai/workers/fixture/lib/session");
        let profile = json!({
            "schema_version": ryeos_state::objects::STRUCTURED_SESSION_PROFILE_SCHEMA_VERSION,
            "transport":"stdio_jsonrpc", "http_sse":null,
            "workload_realization_id":"fixture-runtime", "workload_executable":"fixture-worker",
            "required_process_environment":[], "workload_args":[],
            "workload_home_env":"FIXTURE_HOME", "workload_client":null,
            "baseline_config":"baseline.conf", "baseline_destination":"runtime.conf",
            "auxiliary_configs":[], "runtime_configs":[],
            "configuration_absence":{"absolute_paths":[],"startup_ancestor_paths":[],"route_ancestor_paths":[]},
            "external_candidate":null, "portable_state":null, "credential_subject":null,
            "configuration_authority":"immutable_argv",
            "initialization":[{"method":"initialize","effect_class":"pure_read","params":{},
                "response_schema":"schema/response.json","notification":null}],
            "recovery":null, "route_sets":{"default":["inspect"]},
            "routes":[{"id":"inspect","method":"inspect","effect_class":"pure_read",
                "request_schema":"schema/request.json","response_schema":"schema/response.json",
                "fixed_params":{},"workspace_fields":[],"forbidden_non_null_fields":[],
                "response_predicates":[],"observations":[],"result_retention":"ephemeral","ceremony":null}],
            "notifications":[], "ignored_notifications":{}, "server_requests":[]
        });
        let files = BTreeMap::from([
            ("profile.json", serde_json::to_vec(&profile)?),
            ("baseline.conf", b"fixture=true\n".to_vec()),
            (
                "schema/request.json",
                serde_json::to_vec(&json!({"type":"object","additionalProperties":false}))?,
            ),
            ("schema/response.json", b"{}".to_vec()),
        ]);
        let mut entries = Vec::new();
        for (name, bytes) in files {
            let path = source.join(name);
            fs::create_dir_all(path.parent().unwrap())?;
            fs::write(path, &bytes)?;
            entries.push(SourceClosureFile {
                root: "source".into(),
                path: name.into(),
                blob_hash: lillux::sha256_hex(&bytes),
                size: bytes.len() as u64,
                mode: SourceFileMode::ReadOnly,
            });
        }
        let manifest = SourceClosureManifest::new(
            vec![LogicalSourceRoot {
                id: "source".into(),
            }],
            entries,
        )?;
        let mut helpers = String::new();
        for index in 0..5 {
            let id = if index == 0 {
                "fixture-runtime".to_owned()
            } else {
                format!("fixture-helper-{index}")
            };
            let mount = if index == 0 {
                "fixture-worker".to_owned()
            } else {
                format!("helper-{index}")
            };
            helpers.push_str(&format!("  - id: {id}\n    kind: file\n    mode: pinned\n    digest: \"{index:064x}\"\n    mount_root: execution_runtime\n    mount: {mount}\n"));
        }
        let worker_body = format!(
            r#"category: fixture
version: "1.0.0"
description: signed generic session source fixture
executor_id: "@subprocess"
execution_protocol: protocol:ryeos/core/trusted_structured_session
external_product_slots:
  - id: guest-runtime
    relationship_ref: config:fixture/not-installed
    relationship: runtime_to_worker
    kind: tree
    mount_root: execution_runtime
    mount: guest-runtime
supported_target:
  os: linux
  arch: x86_64
  resources: []
source:
  root: lib/session
  entry: profile.json
  digest: {}
external_content:
{}config:
  command: bin:core/ryeos-structured-session-bridge
  args: ["${{source.entry}}"]
  env: {{}}
  timeout_secs: 0
"#,
            manifest.digest()?,
            helpers
        );
        let mut fixture = Self {
            kind_root: root.path().join("fixture-kinds"),
            state,
            bundle,
            signer: SigningKey::from_bytes(&[19; 32]),
            worker_body,
            manifest,
            _store_owner: store_owner,
            _root: root,
        };
        fixture.write_worker()?;
        fixture.reload_engine()?;
        Ok(fixture)
    }

    fn worker_path(&self) -> PathBuf {
        self.bundle.join(".ai/workers/fixture/session.yaml")
    }

    fn write_worker(&self) -> anyhow::Result<()> {
        fs::write(
            self.worker_path(),
            lillux::signature::sign_content(&self.worker_body, &self.signer, "#", None),
        )?;
        Ok(())
    }

    fn reload_engine(&mut self) -> anyhow::Result<()> {
        let core = ryeos_engine::test_support::core_bundle_root();
        let workflow = ryeos_engine::test_support::workflow_bundle_roots();
        fs::create_dir_all(&self.kind_root)?;
        let mut roots = vec![core.clone()];
        roots.extend(workflow.iter().cloned());
        roots.push(self.bundle.clone());
        let mut trust = ryeos_engine::test_support::live_trust_store();
        let key = self.signer.verifying_key();
        trust.extend_from(&TrustStore::from_signers(vec![TrustedSigner {
            fingerprint: lillux::signature::compute_fingerprint(&key),
            verifying_key: key,
            label: Some("source observation fixture".into()),
        }]));
        let mut kind_roots = vec![self.kind_root.clone(), core.join(".ai/node/engine/kinds")];
        kind_roots.extend(
            workflow
                .iter()
                .map(|root| root.join(".ai/node/engine/kinds")),
        );
        let kinds = ryeos_engine::kind_registry::KindRegistry::load_base(&kind_roots, &trust)?;
        let (parsers, _) =
            ryeos_engine::parsers::ParserRegistry::load_base(&roots, &trust, &kinds)?;
        let handlers = ryeos_engine::test_support::load_live_handler_registry();
        let dispatcher =
            ryeos_engine::parsers::ParserDispatcher::new(parsers, Arc::clone(&handlers));
        let composers = ryeos_engine::composers::ComposerRegistry::from_kinds(&kinds, &handlers)?;
        let protocol_roots = roots
            .iter()
            .cloned()
            .map(|root| (root, ryeos_engine::resolution::TrustClass::TrustedBundle))
            .collect::<Vec<_>>();
        let protocols =
            ryeos_engine::protocols::ProtocolRegistry::load_base(&protocol_roots, &trust)?;
        let registered = ["core", "knowledge", "directive", "graph", "fixture"]
            .into_iter()
            .zip(roots.iter().cloned())
            .map(
                |(name, canonical_root)| ryeos_engine::item_resolution::RegisteredBundleRoot {
                    name: name.into(),
                    canonical_root,
                },
            )
            .collect();
        self.state.engine = Arc::new(
            ryeos_engine::engine::Engine::new(kinds, dispatcher, roots)
                .with_trust_store(trust.clone())
                .with_node_trust_store(trust)
                .with_composers(composers)
                .with_protocols(protocols)
                .with_registered_bundle_roots(registered),
        );
        Ok(())
    }

    fn change_signed_kind(&mut self) -> anyhow::Result<()> {
        let original = fs::read_to_string(
            ryeos_engine::test_support::core_bundle_root()
                .join(".ai/node/engine/kinds/worker/worker.kind-schema.yaml"),
        )?;
        let (_, body) = original
            .split_once('\n')
            .context("signed kind lacks body")?;
        let changed = body.replace("max_files: 256", "max_files: 255");
        ensure!(changed != body, "fixture kind ceiling changed upstream");
        let path = self.kind_root.join("worker/worker.kind-schema.yaml");
        fs::create_dir_all(path.parent().unwrap())?;
        fs::write(
            path,
            lillux::signature::sign_content(&changed, &self.signer, "#", None),
        )?;
        self.reload_engine()
    }

    fn snapshot(&self) -> anyhow::Result<(Vec<(String, String)>, Vec<(String, String)>, usize)> {
        let authority = self.state.state_store.pinned_state_authority()?;
        let cas = authority.cas_store()?;
        let recovery = authority.require_recovery()?;
        Ok((
            tree(cas.root())?,
            tree(recovery.root())?,
            self.state.state_store.list_threads(100)?.len(),
        ))
    }
}

// Only this fresh fixture's CAS and recovery directories, never a live store.
fn tree(root: &Path) -> anyhow::Result<Vec<(String, String)>> {
    fn visit(root: &Path, path: &Path, out: &mut Vec<(String, String)>) -> anyhow::Result<()> {
        if !path.exists() {
            return Ok(());
        }
        for entry in fs::read_dir(path)? {
            let entry = entry?;
            let path = entry.path();
            let ty = entry.file_type()?;
            ensure!(!ty.is_symlink(), "fixture snapshot contains a symlink");
            if ty.is_dir() {
                out.push((
                    path.strip_prefix(root)?.to_string_lossy().into_owned(),
                    "directory".into(),
                ));
                visit(root, &path, out)?;
            } else {
                ensure!(ty.is_file(), "fixture snapshot contains a special file");
                out.push((
                    path.strip_prefix(root)?.to_string_lossy().into_owned(),
                    lillux::sha256_hex(&fs::read(path)?),
                ));
            }
        }
        Ok(())
    }
    let mut out = Vec::new();
    visit(root, root, &mut out)?;
    out.sort();
    Ok(out)
}

#[test]
fn signed_observer_matches_admission_and_preserves_missing_helpers_without_writes()
-> anyhow::Result<()> {
    let fixture = Fixture::new()?;
    let before = fixture.snapshot()?;
    let observed = observe_bundle_structured_source(&fixture.state, WORKER)?;
    assert_eq!(before, fixture.snapshot()?);
    assert_eq!(observed.canonical_ref, WORKER);
    assert!(!observed.bundle_generation_identity.is_empty());
    assert_eq!(
        observed.publisher_fingerprint,
        lillux::signature::compute_fingerprint(&fixture.signer.verifying_key())
    );
    assert_eq!(
        observed.expected_source_content_manifest_hash.as_deref(),
        Some(fixture.manifest.digest()?.as_str())
    );
    assert_eq!(observed.file_count, 4);
    assert_eq!(observed.total_bytes, fixture.manifest.totals.total_bytes);
    let helpers = observed.external_content.as_ref().unwrap();
    assert!(!helpers.ready_for_admission);
    assert_eq!(helpers.declarations.len(), 5);
    for (index, helper) in helpers.declarations.iter().enumerate() {
        assert_eq!(
            helper.id,
            if index == 0 {
                "fixture-runtime".to_owned()
            } else {
                format!("fixture-helper-{index}")
            }
        );
        assert_eq!(
            helper.expected_digest.as_deref(),
            Some(format!("{index:064x}").as_str())
        );
        assert_eq!(helper.status, "missing_manifest");
        assert!(helper.observed_digest.is_none() && helper.binding_digest.is_none());
    }
    let authority = fixture.state.state_store.pinned_state_authority()?;
    let cas = authority.cas_store()?;
    assert!(
        cas.get_object(&observed.candidate_source_binding_hash)?
            .is_none()
    );
    assert!(
        cas.get_object(&observed.candidate_source_content_manifest_hash)?
            .is_none()
    );
    // This is the existing source/profile admission helper, not ordinary
    // executable/product admission. Its unrelated product slot is still absent.
    let admitted = admit_bundle_structured_worker_profile(&fixture.state, WORKER)?;
    assert_eq!(
        observed.candidate_source_binding_hash,
        admitted.source.binding_hash
    );
    assert_eq!(
        observed.candidate_source_content_manifest_hash,
        admitted.source.content_manifest_hash
    );
    assert_eq!(
        observed.candidate_profile_hash,
        admitted.profile.profile_hash
    );
    assert_eq!(
        observed.signed_effective_definition_digest,
        admitted.signed_effective_definition_digest
    );
    Ok(())
}

#[test]
fn signed_observer_binds_worker_and_kind_and_refuses_untrusted_or_changed_source()
-> anyhow::Result<()> {
    let mut fixture = Fixture::new()?;
    let before = fixture.snapshot()?;
    let original = observe_bundle_structured_source(&fixture.state, WORKER)?;
    fixture.worker_body = fixture.worker_body.replace(
        "signed generic session source fixture",
        "changed signed generic fixture",
    );
    fixture.write_worker()?;
    fixture.reload_engine()?;
    let changed_worker = observe_bundle_structured_source(&fixture.state, WORKER)?;
    assert_ne!(
        original.candidate_source_binding_hash,
        changed_worker.candidate_source_binding_hash
    );
    assert_ne!(
        original.signed_effective_definition_digest,
        changed_worker.signed_effective_definition_digest
    );
    assert_eq!(
        original.candidate_source_content_manifest_hash,
        changed_worker.candidate_source_content_manifest_hash
    );
    assert_eq!(
        original.candidate_profile_hash,
        changed_worker.candidate_profile_hash
    );
    fixture.change_signed_kind()?;
    let changed_kind = observe_bundle_structured_source(&fixture.state, WORKER)?;
    assert_ne!(
        changed_worker.candidate_source_binding_hash,
        changed_kind.candidate_source_binding_hash
    );
    assert_eq!(
        changed_worker.candidate_source_content_manifest_hash,
        changed_kind.candidate_source_content_manifest_hash
    );
    assert_eq!(
        changed_worker.candidate_profile_hash,
        changed_kind.candidate_profile_hash
    );
    // The unchanged profile bytes do not mask the changed signed owner/kind
    // binding. A missing subject or broken testimony cannot produce candidates.
    assert!(observe_bundle_structured_source(&fixture.state, "worker:fixture/missing").is_err());
    fs::write(
        fixture
            .bundle
            .join(".ai/workers/fixture/lib/session/baseline.conf"),
        b"fixture=changed\n",
    )?;
    assert!(observe_bundle_structured_source(&fixture.state, WORKER).is_err());
    fs::write(
        fixture
            .bundle
            .join(".ai/workers/fixture/lib/session/baseline.conf"),
        b"fixture=true\n",
    )?;
    fs::write(fixture.worker_path(), &fixture.worker_body)?;
    fixture.reload_engine()?;
    assert!(observe_bundle_structured_source(&fixture.state, WORKER).is_err());
    assert_eq!(before, fixture.snapshot()?);
    Ok(())
}

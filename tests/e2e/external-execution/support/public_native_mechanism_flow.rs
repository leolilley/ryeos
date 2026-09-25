//! Public daemon mechanism observation using exact retained prebuilt inputs.
//! This is not runtime/product qualification, Worker admission or an external
//! candidate turn. The installed trusted observer has its own BundleExecutor
//! identity; its input tree is independently captured, imported and bound.

use crate::common::{DaemonHarness, fast_fixture};
use crate::{public_launch, retained_runtime_producer};
use anyhow::{Context as _, Result, ensure};
use ryeos_app::execution_policy::{ExecutionPolicy, ExecutionResponse};
use ryeos_state::external_content::products::ProductCaptureEvidence;
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    ffi::OsStr,
    path::{Path, PathBuf},
    time::Duration,
};

const TOOL: &str = "tool:fixtures/native-mechanism/observe";
const MOUNT: &str = "mechanism/runtime";
const SCENARIO: &str = "held-live-descendant-settlement-capture";
const PRODUCER_LAUNCH: &str = "L-c6f8afba8dca4c908d4de885f51d2c7a";
const OBSERVER_LAUNCH: &str = "L-ea7ad98cafaa4d2a947dfba2b08aaee1";
const MAX_MEMBER: u64 = 32 * 1024 * 1024;
const MAX_TOTAL: u64 = 40 * 1024 * 1024;
const MEMBERS: [&str; 5] = [
    "bin/probe",
    "lib64/ld-linux-x86-64.so.2",
    "usr/lib/libc.so.6",
    "usr/lib/libgcc_s.so.1",
    "usr/lib/libm.so.6",
];
const DIRECTORIES: [&str; 4] = ["bin", "lib64", "usr", "usr/lib"];
const ENTRY_COUNT: usize = DIRECTORIES.len() + MEMBERS.len();

struct RuntimeInput {
    members: BTreeMap<String, Vec<u8>>,
    hashes: BTreeMap<String, String>,
    manifest_hash: String,
    total_bytes: u64,
}

impl RuntimeInput {
    fn load() -> Result<Self> {
        let mut members = BTreeMap::new();
        for (member, variable, limit) in [
            (MEMBERS[0], "RYEOS_TEST_NATIVE_MECHANISM_PROBE", MAX_MEMBER),
            (
                MEMBERS[1],
                "RYEOS_TEST_NATIVE_MECHANISM_LOADER",
                2 * 1024 * 1024,
            ),
            (
                MEMBERS[2],
                "RYEOS_TEST_NATIVE_MECHANISM_LIBC",
                4 * 1024 * 1024,
            ),
            (
                MEMBERS[3],
                "RYEOS_TEST_NATIVE_MECHANISM_LIBGCC",
                2 * 1024 * 1024,
            ),
            (MEMBERS[4], "RYEOS_TEST_NATIVE_LIBM", 2 * 1024 * 1024),
        ] {
            let path = PathBuf::from(
                std::env::var_os(variable)
                    .with_context(|| format!("explicit {variable} is required"))?,
            );
            ensure!(path.is_absolute(), "{variable} must be absolute");
            let file = lillux::secure_fs::open_pinned_regular_file_no_follow(&path)?;
            let bytes = file.read_stable_bounded(&file.observation()?, limit)?;
            ensure!(
                bytes.len() >= 64
                    && &bytes[..4] == b"\x7fELF"
                    && bytes[4] == 2
                    && bytes[5] == 1
                    && bytes[18..20] == [62, 0],
                "{variable} must be Linux x86_64 ELF"
            );
            members.insert(member.to_owned(), bytes);
        }
        Self::from_bytes(members)
    }

    fn from_bytes(members: BTreeMap<String, Vec<u8>>) -> Result<Self> {
        ensure!(
            members.len() == MEMBERS.len() && MEMBERS.iter().all(|m| members.contains_key(*m)),
            "runtime member set differs"
        );
        let total_bytes = members.values().map(|b| b.len() as u64).sum::<u64>();
        ensure!(
            total_bytes <= MAX_TOTAL
                && members
                    .values()
                    .all(|b| !b.is_empty() && b.len() as u64 <= MAX_MEMBER),
            "runtime bytes exceed authored bounds"
        );
        let hashes = members
            .iter()
            .map(|(p, b)| (p.clone(), lillux::sha256_hex(b)))
            .collect::<BTreeMap<_, _>>();
        // Authored expectation only. This is never inserted into daemon CAS or
        // supplied as a replacement capture witness. Capture must reproduce it.
        let mut entries = DIRECTORIES
            .into_iter()
            .map(|p| json!({"kind":"dir","path":p}))
            .collect::<Vec<_>>();
        for (member, bytes) in &members {
            entries.push(json!({"kind":"file","path":member,"mode":0o755,
                "size":bytes.len(),"blob_hash":hashes[member]}));
        }
        entries.sort_by(|a, b| a["path"].as_str().cmp(&b["path"].as_str()));
        let manifest = ryeos_state::objects::ExternalContentManifestObject::from_value(&json!({
            "kind":"external_content_manifest","schema":"ryeos.external_content.tree.v2",
            "entries":entries,"entry_count":ENTRY_COUNT,"total_bytes":total_bytes,
        }))?;
        let manifest_hash = lillux::sha256_hex(
            lillux::canonical_json(&serde_json::to_value(&manifest)?)?.as_bytes(),
        );
        Ok(Self {
            members,
            hashes,
            manifest_hash,
            total_bytes,
        })
    }

    fn request(&self) -> Value {
        json!({"schema":"test.native_mechanism_request.v1","scenario":SCENARIO,"runtime_sha256":self.hashes})
    }

    fn stage(&self, project: &Path, keys: &fast_fixture::FastFixture) -> Result<()> {
        let recipe = json!({
            "category":"fixtures","version":"1.0.0","description":"Prebuilt native-mechanism input, not qualification",
            "build_products":{"schema":"ryeos.build_products.v1","output_roots":[],"products":[{
                "name":retained_runtime_producer::PRODUCT_NAME,"source":{"kind":"retained_project"},
                "path":"products/external-runtime","shape":"tree","storage":"content","required":true,
                "bounds":{"maximum_entries":ENTRY_COUNT,"maximum_depth":3,"maximum_file_bytes":MAX_MEMBER,"maximum_total_bytes":MAX_TOTAL}}]},
            "product_relationships":{"schema":"ryeos.product_relationships.v1","relationships":[]},
        });
        retained_runtime_producer::write_retained_runtime_producer_sources(
            project,
            &keys.publisher,
            &serde_yaml::to_string(&recipe)?,
        )?;
        let root = lillux::PinnedDirectory::open(project)?.context("producer root absent")?;
        let products = root.create_child(OsStr::new("products"), 0o755)?;
        let runtime = products.create_child(OsStr::new("external-runtime"), 0o755)?;
        for (member, bytes) in &self.members {
            let path = Path::new(member);
            let mut directory = runtime.try_clone()?;
            for component in path.parent().context("member parent")?.components() {
                directory = directory.open_or_create_child(component.as_os_str(), 0o755)?;
            }
            directory
                .atomic_create_pinned_regular(
                    path.file_name().context("member filename")?,
                    bytes,
                    0o755,
                )?
                .context("staged member already exists")?;
        }
        Ok(())
    }
}

fn tool(input: &RuntimeInput) -> Value {
    json!({
        "category":"fixtures/native-mechanism","name":"observe","version":"1.0.0",
        "description":"Trusted native mechanism observation only, not subject qualification",
        "executor_id":"@subprocess","execution_protocol":"protocol:ryeos/core/opaque",
        "effects":"live","filesystem_authority":"node_policy","network_authority":"node_policy",
        "external_content":[{"id":"runtime","kind":"tree","mode":"pinned","digest":input.manifest_hash,
            "mount_root":"project","mount":MOUNT}],
        "config":{"command":"bin:native-mechanism-probe","args":["observe",MOUNT],
            "input_data":serde_json::to_string(&input.request()).expect("bounded fixture JSON"),"timeout_secs":90},
        "config_schema":{"type":"object","properties":{},"additionalProperties":false},
    })
}

fn install(state: &Path, keys: &fast_fixture::FastFixture, input: &RuntimeInput) -> Result<()> {
    let source = std::fs::read_to_string(state.join(".ai/node/policies/isolation.yaml"))?;
    let policy: Value = serde_yaml::from_str(&lillux::signature::strip_signature_lines(&source))?;
    ensure!(
        policy.pointer("/policy/mode") == Some(&json!("disabled")),
        "native observation requires existing trusted fixture lane; no policy override"
    );
    let root = state.join("public-native-mechanism-fixture");
    ensure!(!root.exists(), "fixture bundle already exists");
    let binary = fast_fixture::install_signed_bundle_binary(
        &root,
        "native-mechanism-probe",
        &input.members[MEMBERS[0]],
        &keys.publisher,
    )?;
    ensure!(
        binary.ends_with("/native-mechanism-probe"),
        "installed binary coordinate differs"
    );
    let path = root.join(".ai/tools/fixtures/native-mechanism/observe.yaml");
    std::fs::create_dir_all(path.parent().context("observer source parent")?)?;
    std::fs::write(
        path,
        lillux::signature::sign_content_at(
            &serde_yaml::to_string(&tool(input))?,
            &keys.publisher,
            "#",
            None,
            fast_fixture::FAST_FIXTURE_TIME,
        ),
    )?;
    let manifest = "name: public-native-mechanism-fixture\nversion: 1.0.0\ndescription: Native mechanism observation only\nprovides_kinds: []\nrequires_kinds: [tool]\nuses_kinds: []\n";
    std::fs::write(
        root.join(".ai/manifest.yaml"),
        lillux::signature::sign_content_at(
            manifest,
            &keys.publisher,
            "#",
            None,
            fast_fixture::FAST_FIXTURE_TIME,
        ),
    )?;
    fast_fixture::register_presigned_fixture_bundle(
        state,
        "public-native-mechanism-fixture",
        &root,
        keys,
    )
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Observation {
    schema: String,
    scenario: String,
    runtime_sha256: BTreeMap<String, String>,
    held_counter: u64,
    double_release_refused: bool,
    writer_observations: [u64; 2],
    namespace_exit: String,
    capture_bytes: u64,
    capture_limit_bytes: u64,
    refused_limit_bytes: u64,
    captured_files: BTreeMap<String, String>,
    captured_blob_sha256: BTreeMap<String, String>,
    over_budget_returned_tree: bool,
}

fn verify_observation(result: &Value, input: &RuntimeInput) -> Result<()> {
    ensure!(
        result.is_object(),
        "mechanism observation must be an object"
    );
    let observed: Observation = serde_json::from_value(result.clone())?;
    ensure!(
        observed.schema == "test.native_mechanism_observation.v1"
            && observed.scenario == SCENARIO
            && observed.runtime_sha256 == input.hashes,
        "mechanism input identity differs"
    );
    ensure!(
        observed.held_counter == 0
            && observed.double_release_refused
            && observed.writer_observations[0] > 0
            && observed.writer_observations[1] > observed.writer_observations[0],
        "held launch or live descendant observation differs"
    );
    ensure!(
        observed.namespace_exit == "Signal(9)",
        "live namespace was not observed terminated by requested kill"
    );
    ensure!(
        observed.capture_bytes == 17
            && observed.capture_limit_bytes == 17
            && observed.refused_limit_bytes == 16
            && !observed.over_budget_returned_tree,
        "exact-bound capture or one-byte-over refusal differs"
    );
    ensure!(
        observed.captured_blob_sha256.len() == 2 && observed.captured_files.len() == 2,
        "unexpected captured member set"
    );
    for (member, size) in [("counter", 16), ("parent.ready", 1)] {
        let hash = observed
            .captured_blob_sha256
            .get(member)
            .context("captured blob absent")?;
        ensure!(
            lillux::valid_hash(hash) && !hash.bytes().any(|b| b.is_ascii_uppercase()),
            "noncanonical captured blob hash"
        );
        if member == "parent.ready" {
            ensure!(
                hash == &lillux::sha256_hex(b"1"),
                "captured handshake differs"
            );
        }
        let file = ryeos_state::objects::ProjectFile {
            blob_hash: hash.clone(),
            size,
            normalized_mode: 0o644,
        };
        let file_hash = lillux::sha256_hex(lillux::canonical_json(&file.to_value())?.as_bytes());
        ensure!(
            observed.captured_files.get(member) == Some(&file_hash),
            "captured file descriptor differs"
        );
    }
    Ok(())
}

fn verify_capsule(
    state: &Path,
    detail: &Value,
    input: &RuntimeInput,
    signer: &str,
) -> Result<String> {
    let hash = detail
        .pointer("/thread/admitted_launch_capsule_hash")
        .and_then(Value::as_str)
        .context("observer capsule coordinate")?;
    let capsule = ryeos_state::objects::AdmittedLaunchCapsule::from_current_value(
        lillux::CasStore::new(state.join(".ai/state/objects"))
            .get_object(hash)?
            .context("observer retained capsule")?,
    )?;
    capsule.validate()?;
    ensure!(
        matches!(
            &capsule.project_authority,
            ryeos_state::objects::ExecutionProjectAuthority::Projectless { .. }
        ),
        "observer acquired logical project authority"
    );
    let ryeos_state::objects::AdmittedLaunchArtifactIdentity::DirectItemExecutor {
        executable_identity,
        ..
    } = &capsule.artifact_identity
    else {
        anyhow::bail!("observer is not a direct executor")
    };
    ensure!(
        matches!(executable_identity, ryeos_state::objects::DirectExecutableIdentity::BundleExecutor {
        content_hash, executor_manifest_signer_fingerprint, ..
    } if content_hash == &input.hashes[MEMBERS[0]] && executor_manifest_signer_fingerprint == signer),
        "observer lost exact signed BundleExecutor identity"
    );
    ensure!(
        detail.pointer("/thread/item_ref") == Some(&json!(TOOL))
            && detail.pointer("/thread/project_authority")
                == Some(&serde_json::to_value(&capsule.project_authority)?),
        "observer thread and retained capsule differ"
    );
    Ok(hash.to_owned())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires current daemon and explicit native mechanism probe/loader/libc/libgcc/libm inputs; mechanism-only observation"]
async fn public_native_mechanism_probe_settles_writer_then_captures() -> Result<()> {
    let input = RuntimeInput::load()?;
    let (mut harness, keys) = DaemonHarness::start_fast_with(
        |state, _, keys| {
            fast_fixture::register_standard_bundle(state, keys)?;
            install(state, keys, &input)
        },
        |_| {},
    )
    .await?;
    harness.retain_evidence_on_drop(true);
    let mut project = tempfile::tempdir()?;
    project.disable_cleanup(true);
    eprintln!(
        "native mechanism retained node={} producer_project={} manifest={}",
        harness.state_path.display(),
        project.path().display(),
        input.manifest_hash
    );
    input.stage(project.path(), &keys)?;
    let (producer_accepted, producer_detail) = public_launch::completed_launch(&harness, json!({
        "item_ref":retained_runtime_producer::PRODUCER_REF,"launch_id":PRODUCER_LAUNCH,"project_path":project.path(),
        "ref_bindings":{},"parameters":{},
        "execution_policy":ExecutionPolicy::local_pinned_capture(ExecutionResponse::Accepted).exclude_operator_vault(),
    }), PRODUCER_LAUNCH).await?;
    let root = producer_accepted["thread_id"]
        .as_str()
        .context("accepted producer root")?;
    let producer = crate::RetainedRuntimeProducer {
        thread_id: root.to_owned(),
        chain_root_id: root.to_owned(),
    };
    let captured = tokio::time::timeout(
        Duration::from_secs(60),
        crate::capture_retained_runtime(&harness, &producer),
    )
    .await
    .context("native input capture observation expired; retain producer, no relaunch")??;
    let evidence: ProductCaptureEvidence = serde_json::from_value(captured["evidence"].clone())?;
    evidence.validate()?;
    ensure!(
        evidence.thread_id == root
            && evidence.chain_root_id == root
            && evidence.owner_principal == format!("fp:{}", keys.user_fp())
            && evidence.manifest_hash == input.manifest_hash
            && evidence.manifest_kind == "external_content_manifest"
            && evidence.entry_count == ENTRY_COUNT
            && evidence.total_bytes == input.total_bytes
            && evidence.recipe_binding == retained_runtime_producer::RECIPE_BINDING
            && evidence.recipe_ref == retained_runtime_producer::RECIPE_REF
            && evidence.workspace_output_capture_hash.is_none()
            && producer_detail.pointer("/thread/result_project_snapshot_hash")
                == Some(&json!(evidence.result_project_snapshot_hash)),
        "public capture differs from original retained producer and authored runtime"
    );
    let imported = crate::production_service(
        &harness,
        "service:external-content/import",
        json!({
            "source":"retained_product","witness_hash":captured["witness_hash"],
            "witness_source":{"kind":"local_capture"},"maximum_bytes":MAX_TOTAL,
        }),
    )
    .await?;
    ensure!(
        imported["manifest_hash"] == input.manifest_hash
            && imported["total_bytes"] == input.total_bytes
            && imported["entry_count"] == ENTRY_COUNT,
        "public import changed exact runtime"
    );
    let bound = crate::production_service(&harness, "service:external-content/bind", json!({
        "staging_id":imported["staging_id"],"request_digest":imported["request_digest"],
        "manifest_hash":input.manifest_hash,"consumer_ref":TOOL,"consumer_kind":"installed_bundle",
    })).await?;
    ensure!(
        bound["manifest_hash"] == input.manifest_hash
            && bound["consumer_ref"] == TOOL
            && bound["publisher_fingerprint"] == keys.publisher_fp(),
        "binding changed installed observer identity"
    );
    let (observer_accepted, detail) = public_launch::completed_launch(&harness, json!({
        "item_ref":TOOL,"launch_id":OBSERVER_LAUNCH,"ref_bindings":{},"parameters":{},
        "execution_policy":ExecutionPolicy::projectless(ExecutionResponse::Accepted).exclude_operator_vault(),
    }), OBSERVER_LAUNCH).await?;
    let capsule = verify_capsule(&harness.state_path, &detail, &input, &keys.publisher_fp())?;
    let result = detail
        .pointer("/result/result")
        .context("observer typed terminal result")?;
    verify_observation(result, &input)?;
    ensure!(
        detail["artifacts"] == json!([]),
        "mechanism observer unexpectedly published artifacts"
    );
    let diagnostic = serde_json::to_string(&json!({
        "scope":"native-mechanism-observation-only","node":harness.state_path,"project":project.path(),
        "producer_accepted":producer_accepted,"observer_accepted":observer_accepted,
        "witness_hash":captured["witness_hash"],"manifest_hash":input.manifest_hash,"binding":bound,
        "observer_capsule_hash":capsule,"observer_artifacts":detail["artifacts"],"observation":result,
    }))?;
    ensure!(
        diagnostic.len() <= 16384,
        "native mechanism evidence exceeds diagnostic bound"
    );
    eprintln!("exact public native mechanism observation: {diagnostic}");
    harness.kill_daemon().await?;
    Ok(())
}

#[test]
fn authored_manifest_and_tool_are_fixed_before_capture() {
    let members = MEMBERS
        .into_iter()
        .map(|m| (m.to_owned(), m.as_bytes().to_vec()))
        .collect::<BTreeMap<_, _>>();
    let input = RuntimeInput::from_bytes(members.clone()).unwrap();
    let mut changed = members;
    changed.get_mut(MEMBERS[0]).unwrap().push(1);
    assert_ne!(
        input.manifest_hash,
        RuntimeInput::from_bytes(changed).unwrap().manifest_hash
    );
    let source = tool(&input);
    assert_eq!(source["config"]["command"], "bin:native-mechanism-probe");
    assert_eq!(source["external_content"][0]["digest"], input.manifest_hash);
    assert_eq!(source["external_content"][0]["mount_root"], "project");
    assert_eq!(source["config"]["args"], json!(["observe", MOUNT]));
    assert_eq!(
        serde_json::from_str::<Value>(source["config"]["input_data"].as_str().unwrap()).unwrap(),
        input.request()
    );
    assert!(source.get("execution_endpoint").is_none());
}

#[test]
fn observation_rejects_authority_extras_and_bad_boundaries() {
    let input = RuntimeInput::from_bytes(
        MEMBERS
            .into_iter()
            .map(|m| (m.to_owned(), vec![1]))
            .collect(),
    )
    .unwrap();
    let blobs = BTreeMap::from([
        ("counter", lillux::sha256_hex(b"0000000000000002")),
        ("parent.ready", lillux::sha256_hex(b"1")),
    ]);
    let files = blobs
        .iter()
        .map(|(name, hash)| {
            let file = ryeos_state::objects::ProjectFile {
                blob_hash: hash.clone(),
                size: if *name == "counter" { 16 } else { 1 },
                normalized_mode: 0o644,
            };
            (
                *name,
                lillux::sha256_hex(lillux::canonical_json(&file.to_value()).unwrap().as_bytes()),
            )
        })
        .collect::<BTreeMap<_, _>>();
    let valid = json!({"schema":"test.native_mechanism_observation.v1","scenario":SCENARIO,"runtime_sha256":input.hashes,
        "held_counter":0,"double_release_refused":true,"writer_observations":[1,2],"namespace_exit":"Signal(9)",
        "capture_bytes":17,"capture_limit_bytes":17,"refused_limit_bytes":16,"captured_files":files,
        "captured_blob_sha256":blobs,"over_budget_returned_tree":false});
    verify_observation(&valid, &input).unwrap();
    for (field, replacement) in [
        ("writer_observations", json!([1, 1])),
        ("namespace_exit", json!("Code(0)")),
        ("held_counter", json!(1)),
        ("capture_limit_bytes", json!(18)),
        ("over_budget_returned_tree", json!(true)),
        ("qualification", json!(true)),
        ("runtime_sha256", json!({})),
    ] {
        let mut changed = valid.clone();
        changed[field] = replacement;
        assert!(verify_observation(&changed, &input).is_err(), "{field}");
    }
}

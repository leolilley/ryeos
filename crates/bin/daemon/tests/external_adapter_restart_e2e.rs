//! Real daemon process restart with a signed synthetic external adapter bundle.
//!
//! Checks adapter admission/restart and public retained-runtime production and
//! capture prerequisites, not candidate-turn qualification. The latter must
//! dispatch a signed worker and exercise its authenticated provider channel
//! through this same daemon harness.

#![recursion_limit = "256"]

mod common;

#[path = "../../../../tests/e2e/external-execution/support/signed_bundle.rs"]
mod signed_bundle;

#[path = "../../../../tests/e2e/external-execution/support/retained_runtime_producer.rs"]
mod retained_runtime_producer;

#[path = "../../../../tests/e2e/external-execution/support/offline_runtime_input.rs"]
mod offline_runtime_input;

#[path = "../../../../tests/e2e/external-execution/support/native_evaluation.rs"]
mod native_evaluation;

#[path = "../../../../tests/e2e/external-execution/support/public_enrollment.rs"]
mod public_enrollment;

#[path = "../../../../tests/e2e/external-execution/support/public_launch.rs"]
mod public_launch;

#[path = "../../../../tests/e2e/external-execution/support/public_enrollment_flow.rs"]
mod public_enrollment_flow;

#[path = "../../../../tests/e2e/external-execution/support/public_native_evaluation_flow.rs"]
mod public_native_evaluation_flow;

#[path = "../../../../tests/e2e/external-execution/support/public_verifier_input_flow.rs"]
mod public_verifier_input_flow;

#[path = "../../../../tests/e2e/external-execution/support/public_native_mechanism_flow.rs"]
mod public_native_mechanism_flow;

#[path = "../../../../tests/e2e/external-execution/support/codex_runtime_producer.rs"]
mod codex_runtime_producer;

#[path = "../../../../tests/e2e/external-execution/support/candidate_authoring.rs"]
mod candidate_authoring;

#[path = "../../../../tests/e2e/external-execution/support/independent_verifier_scenario.rs"]
mod independent_verifier_scenario;

#[path = "../../../../tests/e2e/external-execution/support/admitted_worker_evidence.rs"]
mod admitted_worker_evidence;

#[path = "../../../../tests/e2e/external-execution/support/ordinary_direct.rs"]
mod ordinary_direct;

#[path = "../../../../tests/e2e/external-execution/support/ordinary_evidence.rs"]
mod ordinary_evidence;

use std::path::{Path, PathBuf};

use anyhow::Context as _;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use common::DaemonHarness;
use ryeos_app::execution_policy::{ExecutionPolicy, ExecutionResponse};
use ryeos_state::external_content::products::ProductCaptureEvidence;
use serde_json::{Value, json};

#[test]
fn signed_codex_worker_source_admits_direct_qualification_profile() -> anyhow::Result<()> {
    use anyhow::ensure;

    let admitted = admit_signed_codex_worker()?;
    let requirement = admitted
        .profile
        .external_candidate_requirement()?
        .ok_or_else(|| anyhow::anyhow!("admitted Codex Worker has no external candidate"))?;
    let mut fixture_requirement = candidate_authoring::real_codex_requirement();
    fixture_requirement.runtime_product_declaration_id = "guest-runtime".into();
    fixture_requirement
        .required_lifecycle_capabilities
        .insert(ryeos_external_execution_contract::LifecycleCapability::ExactTerminalObservation);
    ensure!(
        requirement == fixture_requirement
            && requirement.provider_declaration_id == "codex-hosted"
            && requirement.runtime_product_declaration_id == "guest-runtime"
            && lillux::valid_hash(&admitted.source.binding_hash)
            && lillux::valid_hash(&admitted.source.content_manifest_hash)
            && lillux::valid_hash(&admitted.profile.profile_hash),
        "signed Codex Worker admission changed the direct qualification identity"
    );
    eprintln!(
        "signed Codex Worker admission: {}",
        json!({
            "source_binding_hash":admitted.source.binding_hash,
            "source_content_manifest_hash":admitted.source.content_manifest_hash,
            "profile_hash":admitted.profile.profile_hash,
        })
    );
    Ok(())
}

fn admit_signed_codex_worker() -> anyhow::Result<admitted_worker_evidence::AdmittedWorkerEvidence> {
    use std::sync::Arc;

    let root = tempfile::tempdir()?;
    let mut state = ryeos_app::state::test_support::build(root.path())?;
    let core = ryeos_engine::test_support::core_bundle_root();
    let standard = ryeos_engine::test_support::standard_bundle_root();
    let codex = ryeos_engine::test_support::workspace_root().join("bundles/codex");
    let roots = vec![core.clone(), standard.clone(), codex];
    let trust = ryeos_engine::test_support::live_trust_store();
    let kinds = ryeos_engine::kind_registry::KindRegistry::load_base(
        &[
            core.join(".ai/node/engine/kinds"),
            standard.join(".ai/node/engine/kinds"),
        ],
        &trust,
    )?;
    let (parsers, _) = ryeos_engine::parsers::ParserRegistry::load_base(&roots, &trust, &kinds)?;
    let handlers = ryeos_engine::test_support::load_live_handler_registry();
    let dispatcher = ryeos_engine::parsers::ParserDispatcher::new(parsers, Arc::clone(&handlers));
    let composers = ryeos_engine::composers::ComposerRegistry::from_kinds(&kinds, &handlers)?;
    let registered = ["core", "standard", "codex"]
        .into_iter()
        .zip(roots.iter().cloned())
        .map(
            |(name, canonical_root)| ryeos_engine::item_resolution::RegisteredBundleRoot {
                name: name.to_owned(),
                canonical_root,
            },
        )
        .collect();
    state.engine = Arc::new(
        ryeos_engine::engine::Engine::new(kinds, dispatcher, roots)
            .with_trust_store(trust.clone())
            .with_node_trust_store(trust)
            .with_composers(composers)
            .with_registered_bundle_roots(registered),
    );
    admitted_worker_evidence::admit_worker(&state, "worker:codex/external-hosted-authoring")
}

fn artifact(directory: &Path, name: &str) -> PathBuf {
    let path = directory.join(name);
    assert!(
        path.is_file(),
        "missing exact synthetic test artifact {}",
        path.display()
    );
    path
}

fn ordinary_observation_diagnostic(
    elapsed_ms: u128,
    daemon_before_drop: &str,
    error: &anyhow::Error,
) -> String {
    // Include every cause, subject to an explicit diagnostic bound; never
    // inspect request bodies, channel capabilities or provider credentials.
    let details = format!("daemon_before_drop={daemon_before_drop}; error_chain={error:#}");
    let mut chars = details.chars();
    let bounded: String = chars.by_ref().take(8192).collect();
    let suffix = if chars.next().is_some() {
        " [diagnostic truncated]"
    } else {
        ""
    };
    format!(
        "ordinary execution observation failed after {elapsed_ms}ms; {bounded}{suffix}; do not relaunch"
    )
}

#[test]
fn ordinary_observation_diagnostic_preserves_exit_elapsed_and_causes() {
    let error = anyhow::anyhow!("connection closed before message completed")
        .context("HTTP execute request failed");
    let diagnostic = ordinary_observation_diagnostic(60001, "exited (signal: 9)", &error);
    assert!(diagnostic.contains("after 60001ms"));
    assert!(diagnostic.contains("daemon_before_drop=exited (signal: 9)"));
    assert!(diagnostic.contains(
        "error_chain=HTTP execute request failed: connection closed before message completed"
    ));
    assert!(diagnostic.ends_with("do not relaunch"));
}

#[test]
fn ordinary_observation_diagnostic_bounds_multibyte_error_text() {
    let error = anyhow::anyhow!("{}", "界".repeat(9000));
    let diagnostic = ordinary_observation_diagnostic(1, "running at observation", &error);
    assert!(diagnostic.contains("daemon_before_drop=running at observation"));
    assert!(diagnostic.contains("[diagnostic truncated]"));
    assert!(diagnostic.chars().count() < 8400);
    assert!(diagnostic.len() < 4 * 8400);
}

/// Fixture-only outer TLS termination for DaemonHarness's HTTP listener. The
/// relay transports exact channel bodies to the real daemon routes; it cannot
/// mint occurrence authentication, Ready, Release or journal observations.
/// This does not qualify production listener/TLS or native guest isolation.
struct OrdinaryControllerRelay(tokio::task::JoinHandle<anyhow::Result<()>>);

impl Drop for OrdinaryControllerRelay {
    fn drop(&mut self) {
        self.0.abort();
    }
}

impl OrdinaryControllerRelay {
    fn start(
        listener: tokio::net::TcpListener,
        upstream: std::net::SocketAddr,
    ) -> anyhow::Result<Self> {
        use ryeos_external_candidate_supervisor::test_support::{
            TEST_SERVER_DER_BASE64, TEST_SERVER_KEY_DER_BASE64,
        };
        anyhow::ensure!(
            upstream.ip().is_loopback(),
            "relay upstream must be loopback"
        );
        let config = rustls::ServerConfig::builder_with_provider(std::sync::Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .with_no_client_auth()
        .with_single_cert(
            vec![rustls::pki_types::CertificateDer::from(
                STANDARD.decode(TEST_SERVER_DER_BASE64)?,
            )],
            rustls::pki_types::PrivateKeyDer::Pkcs8(rustls::pki_types::PrivatePkcs8KeyDer::from(
                STANDARD.decode(TEST_SERVER_KEY_DER_BASE64)?,
            )),
        )?;
        let acceptor = tokio_rustls::TlsAcceptor::from(std::sync::Arc::new(config));
        Ok(Self(tokio::spawn(async move {
            // One bounded connection at a time; dropping the task drops the
            // listener and current connection, with no detached guest tasks.
            loop {
                let (stream, peer) = listener.accept().await?;
                anyhow::ensure!(peer.ip().is_loopback(), "nonlocal fixture client");
                let connection = tokio::time::timeout(std::time::Duration::from_secs(10), async {
                    let stream = acceptor.accept(stream).await?;
                    hyper::server::conn::http1::Builder::new()
                        .keep_alive(false)
                        .serve_connection(
                            hyper_util::rt::TokioIo::new(stream),
                            hyper::service::service_fn(move |request| {
                                ordinary_channel_relay(request, upstream)
                            }),
                        )
                        .await?;
                    Ok::<_, anyhow::Error>(())
                })
                .await;
                // A disconnected guest or a temporarily stopped upstream is
                // one failed observation, not loss of the listener. Never
                // resend its request: drop this connection and accept a new
                // independently authenticated exchange.
                match connection {
                    Ok(Ok(())) => {}
                    Ok(Err(error)) => eprintln!("fixture relay connection failed: {error}"),
                    Err(_) => eprintln!("fixture relay connection exceeded absolute deadline"),
                }
            }
        })))
    }
}

#[tokio::test]
async fn ordinary_relay_survives_disconnected_tls_client() -> anyhow::Result<()> {
    use ryeos_external_candidate_supervisor::test_support::TEST_CA_DER_BASE64;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let upstream = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let relay = OrdinaryControllerRelay::start(listener, upstream.local_addr()?)?;
    // EOF before TLS negotiation must discard only this connection.
    drop(tokio::net::TcpStream::connect(address).await?);
    let mut roots = rustls::RootCertStore::empty();
    roots.add(rustls::pki_types::CertificateDer::from(
        STANDARD.decode(TEST_CA_DER_BASE64)?,
    ))?;
    let client = rustls::ClientConfig::builder_with_provider(std::sync::Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_protocol_versions(&[&rustls::version::TLS13])?
    .with_root_certificates(roots)
    .with_no_client_auth();
    let connector = tokio_rustls::TlsConnector::from(std::sync::Arc::new(client));
    let stream = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        connector
            .connect(
                rustls::pki_types::ServerName::try_from("localhost")?,
                tokio::net::TcpStream::connect(address).await?,
            )
            .await
            .map_err(anyhow::Error::from)
    })
    .await
    .context("relay failed to accept a new TLS connection after EOF")??;
    assert!(!relay.0.is_finished());
    drop(stream);
    Ok(())
}

async fn ordinary_channel_relay(
    request: hyper::Request<hyper::body::Incoming>,
    upstream: std::net::SocketAddr,
) -> anyhow::Result<hyper::Response<http_body_util::Full<bytes::Bytes>>> {
    use http_body_util::BodyExt as _;
    use ryeos_state::external_execution::transport::{
        EXTERNAL_CHANNEL_ATTACH_PATH, EXTERNAL_CHANNEL_EXCHANGE_PATH,
    };
    anyhow::ensure!(
        request.method() == hyper::Method::POST
            && request.uri().query().is_none()
            && matches!(
                request.uri().path(),
                EXTERNAL_CHANNEL_ATTACH_PATH | EXTERNAL_CHANNEL_EXCHANGE_PATH
            ),
        "relay accepts only exact external channel POST routes"
    );
    let path = request.uri().path().to_owned();
    let body = http_body_util::Limited::new(request.into_body(), 2 * 1024 * 1024)
        .collect()
        .await
        .map_err(anyhow::Error::from_boxed)?
        .to_bytes();
    let stream = tokio::net::TcpStream::connect(upstream).await?;
    let (mut sender, connection) =
        hyper::client::conn::http1::handshake(hyper_util::rt::TokioIo::new(stream)).await?;
    // The connection is polled inline with the request, so cancellation or
    // timeout cannot leave an orphan background HTTP driver.
    let exchange = async {
        let response = sender
            .send_request(
                hyper::Request::builder()
                    .method(hyper::Method::POST)
                    .uri(path)
                    .header(hyper::header::HOST, upstream.to_string())
                    .header(hyper::header::CONTENT_TYPE, "application/json")
                    .body(http_body_util::Full::new(body))?,
            )
            .await?;
        let status = response.status();
        let body = http_body_util::Limited::new(response.into_body(), 2 * 1024 * 1024)
            .collect()
            .await
            .map_err(anyhow::Error::from_boxed)?
            .to_bytes();
        Ok(hyper::Response::builder()
            .status(status)
            .header(hyper::header::CONTENT_TYPE, "application/json")
            .body(http_body_util::Full::new(body))?)
    };
    tokio::pin!(exchange);
    tokio::select! {
        result = &mut exchange => result,
        result = connection => {
            result?;
            // A Connection: close response may complete its driver before
            // the already-buffered response body is polled by `exchange`.
            exchange.await
        }
    }
}

struct OrdinaryPublicFixture {
    harness: DaemonHarness,
    project: tempfile::TempDir,
    provider: tempfile::TempDir,
    relay: OrdinaryControllerRelay,
    snapshot_hash: String,
}

async fn prepare_public_ordinary_fixture(recorded: bool) -> anyhow::Result<OrdinaryPublicFixture> {
    use ryeos_state::external_execution::transport::*;
    anyhow::ensure!(
        std::env::consts::OS == "linux" && std::env::consts::ARCH == "x86_64",
        "the retained syscall-only test ELF requires its exact Linux x86_64 target"
    );
    let directory = PathBuf::from(
        std::env::var_os("RYEOS_TEST_SYNTHETIC_BIN_DIR")
            .context("set RYEOS_TEST_SYNTHETIC_BIN_DIR to exact built fixture binaries")?,
    );
    let adapter = artifact(&directory, "ryeos-synthetic-external-lifecycle-adapter");
    let supervisor = artifact(&directory, "ryeos-synthetic-external-candidate-supervisor");
    let launcher = artifact(&directory, "ryeos-synthetic-external-candidate-launcher");
    let connector = artifact(&directory, "ryeos-synthetic-external-candidate-connector");
    let configuration = artifact(&directory, "ryeos-synthetic-codex-external-configuration");
    let artifacts = signed_bundle::SyntheticExternalArtifacts {
        adapter: &adapter,
        supervisor: &supervisor,
        launcher: &launcher,
        connector: &connector,
        configuration: &configuration,
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let roots =
        vec![ryeos_external_candidate_supervisor::test_support::TEST_CA_DER_BASE64.to_owned()];
    let transport = ExternalControllerTransportContract {
        schema: 2,
        https_origin: format!("https://localhost:{}", listener.local_addr()?.port()),
        route_contract: EXTERNAL_CHANNEL_ROUTE_CONTRACT.into(),
        tls_root_bundle_digest: external_tls_root_bundle_digest(&roots)?,
        connect_timeout_ms: 1000,
        request_timeout_ms: 5000,
        maximum_response_bytes: 2 * 1024 * 1024,
        network_inputs: ExternalNetworkInputPolicy {
            resolver: ExternalNetworkInputSelection {
                source: "/etc/resolv.conf".into(),
                max_bytes: 65536,
            },
            hosts: ExternalNetworkInputSelection {
                source: "/etc/hosts".into(),
                max_bytes: 65536,
            },
        },
    };
    let mut provider = tempfile::tempdir()?;
    provider.disable_cleanup(true);
    // The adapter requires an owner-private provider root. A generic temporary
    // directory inherits the harness's creation mask and is not that contract.
    let provider_parent = lillux::PinnedDirectory::open(provider.path())?
        .context("fixture provider parent is absent")?;
    let provider_root = provider_parent.create_child(std::ffi::OsStr::new("provider"), 0o700)?;
    provider_root.require_owner_private_directory()?;
    let provider_state_path = provider.path().join("provider");
    let credential = "ordinary-direct-disposable-fixture-only";
    let generation = lillux::sha256_hex(b"ordinary-direct-fixture-generation-v1");
    let (mut harness, fixture) = DaemonHarness::start_fast_with(|state, _user, fixture| {
        common::fast_fixture::register_standard_bundle(state, fixture)?;
        ordinary_direct::install_before_start(state, fixture, &artifacts, ordinary_direct::EndpointInputs {
            transport: &transport, tls_roots_der_base64: &roots,
            credential_generation: &generation, credential_sha256: &lillux::sha256_hex(credential.as_bytes()),
            provider_state_root: &provider_state_path,
        })?;
        // Reuse normal signed node-config admission before the existing
        // test-only vault provisioner; no raw reserved vault key is authored.
        let trust = ryeos_engine::trust::TrustStore::load(None, &state.join(".ai/config"))?;
        let loader = ryeos_app::node_config::loader::BootstrapLoader { app_root: state, trust_store: &trust };
        let table = ryeos_app::node_policy::NodePolicyTable::new();
        let policy = ryeos_app::node_policy::load_snapshot(state, &trust, &table)?;
        let config = loader.load_full(&ryeos_app::node_config::NodeConfigTable::new(),
            &loader.load_bundle_section()?,
            policy.require::<ryeos_app::node_policy::sections::command_registration::CommandRegistrationAuthority>()?,
            &table)?;
        anyhow::ensure!(config.external_execution.len() == 1, "expected one installed fixture binding");
        ryeos_app::node_config::sections::external_execution::provision_installed_test_placement_credential(
            state, &config.external_execution[0], credential)
    }, |_| {}).await?;
    harness.retain_evidence_on_drop(true);
    let relay = OrdinaryControllerRelay::start(listener, harness.bind)?;
    let mut project = tempfile::tempdir()?;
    project.disable_cleanup(true);
    eprintln!(
        "ordinary external public fixture: node={}, project={}, provider={}",
        harness.state_path.display(),
        project.path().display(),
        provider_state_path.display()
    );
    let runtime = STANDARD.decode(
        include_str!("../../../../tests/e2e/environment-products/runtime-program.b64")
            .split_whitespace()
            .collect::<String>(),
    )?;
    ordinary_direct::write_producer(project.path(), &fixture, &runtime)?;
    let producer = tokio::time::timeout(
        std::time::Duration::from_secs(90),
        run_retained_runtime_producer(&harness, project.path()),
    )
    .await
    .context("producer timed out; retain exact state without relaunch")??;
    let captured = tokio::time::timeout(
        std::time::Duration::from_secs(60),
        capture_retained_runtime(&harness, &producer),
    )
    .await
    .context("capture observation timed out; retain exact producer")??;
    eprintln!(
        "ordinary external producer={}, witness={}",
        producer.thread_id, captured["witness_hash"]
    );
    let evidence: ProductCaptureEvidence = serde_json::from_value(captured["evidence"].clone())?;
    anyhow::ensure!(
        evidence.chain_root_id == producer.chain_root_id
            && evidence.thread_id == producer.thread_id,
        "capture changed the authoritative producer coordinates"
    );
    if recorded {
        ordinary_direct::write_recorded_consumer(project.path(), &fixture, &evidence)?;
    } else {
        ordinary_direct::write_consumer(project.path(), &fixture, &evidence)?;
    }
    // Product capture retains bytes, not consumer authority. Capture the new
    // signed consumer once through the ordinary snapshot Tool, then explicitly
    // import/bind the original witness to that exact pinned generation.
    let (snapshot_status, snapshot_response) = tokio::time::timeout(
        std::time::Duration::from_secs(150),
        harness.post_json("/execute", json!({
            "item_ref":"tool:core/snapshot-create", "ref_bindings":{},
            "project_path":project.path(),
            "parameters":{"project_path":project.path(),
                "message":"ordinary external consumer with captured runtime", "allow_empty":true},
            "execution_policy":ExecutionPolicy::local_live(ExecutionResponse::Wait)
                .exclude_operator_vault(),
        })),
    ).await.context("consumer snapshot observation timed out; do not relaunch")??;
    anyhow::ensure!(
        snapshot_status.is_success()
            && snapshot_response.pointer("/thread/status") == Some(&json!("completed")),
        "consumer snapshot did not complete: {snapshot_status}: {snapshot_response}"
    );
    let snapshot = snapshot_response
        .pointer("/result/result")
        .context("snapshot-create terminal payload")?;
    anyhow::ensure!(
        snapshot["kind"] == "snapshot_create"
            && snapshot["created"] == true
            && snapshot["project_path"] == json!(project.path())
            && snapshot["snapshot_hash"] == snapshot["head_snapshot_hash"],
        "snapshot-create returned an unexpected capture: {snapshot}"
    );
    let snapshot_hash = snapshot["snapshot_hash"]
        .as_str()
        .context("exact consumer snapshot")?;
    anyhow::ensure!(
        lillux::valid_hash(snapshot_hash),
        "invalid public snapshot hash"
    );
    let imported = production_service(
        &harness,
        "service:external-content/import",
        json!({
            "source":"retained_product", "witness_hash":captured["witness_hash"],
            "witness_source":{"kind":"local_capture"},
            "maximum_bytes":evidence.declaration.bounds.maximum_total_bytes,
        }),
    )
    .await?;
    assert_eq!(imported["manifest_hash"], evidence.manifest_hash);
    assert_eq!(imported["manifest_kind"], evidence.manifest_kind);
    assert_eq!(imported["entry_count"], evidence.entry_count);
    assert_eq!(imported["total_bytes"], evidence.total_bytes);
    let bound = production_service(
        &harness,
        "service:external-content/bind",
        json!({
            "staging_id":imported["staging_id"], "request_digest":imported["request_digest"],
            "manifest_hash":imported["manifest_hash"], "consumer_ref":ordinary_direct::TOOL_REF,
            "consumer_kind":"pinned_project", "project_snapshot_hash":snapshot_hash,
            "project_path":project.path(),
        }),
    )
    .await?;
    assert_eq!(bound["manifest_hash"], evidence.manifest_hash);
    assert_eq!(bound["consumer_ref"], ordinary_direct::TOOL_REF);
    assert_eq!(bound["publisher_fingerprint"], fixture.publisher_fp());
    eprintln!("ordinary consumer snapshot={snapshot_hash}, import={imported}, binding={bound}");
    Ok(OrdinaryPublicFixture {
        harness,
        project,
        provider,
        relay,
        snapshot_hash: snapshot_hash.to_owned(),
    })
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires current daemon/synthetic binaries and native guest isolation prerequisites"]
async fn public_ordinary_external_tool_uses_captured_runtime_and_authenticated_channel()
-> anyhow::Result<()> {
    let OrdinaryPublicFixture {
        mut harness,
        project,
        provider: _provider,
        relay: _relay,
        snapshot_hash,
    } = prepare_public_ordinary_fixture(false).await?;
    let snapshot_hash = snapshot_hash.as_str();
    let observation_timer = lillux::time::MonotonicTimer::start();
    let observed = tokio::time::timeout(
        std::time::Duration::from_secs(180),
        harness.post_json(
            "/execute",
            ordinary_direct::request(project.path(), snapshot_hash, ExecutionResponse::Wait)?,
        ),
    )
    .await;
    let (status, response) = match observed {
        Ok(Ok(response)) => response,
        failure => {
            let elapsed_ms = observation_timer.elapsed().as_millis();
            // Observe the existing harness-owned child before its Drop kills
            // it. This is diagnostic evidence, not a lifecycle operation or
            // permission to retry the possibly accepted execution.
            let daemon_before_drop = match harness.child.try_wait() {
                Ok(Some(status)) => format!("exited ({status})"),
                Ok(None) => "running at observation".to_owned(),
                Err(error) => format!("exit observation failed ({error})"),
            };
            let error = match failure {
                Ok(Err(error)) => error,
                Err(error) => {
                    anyhow::Error::new(error).context("ordinary execution observation timed out")
                }
                Ok(Ok(_)) => unreachable!("successful response handled above"),
            };
            anyhow::bail!(ordinary_observation_diagnostic(
                elapsed_ms,
                &daemon_before_drop,
                &error,
            ));
        }
    };
    eprintln!("ordinary external public response: {status}: {response}");
    anyhow::ensure!(status.is_success(), "ordinary admission failed: {response}");
    anyhow::ensure!(
        response.pointer("/thread/status") == Some(&json!("completed"))
            && response.pointer("/result/outcome_code") == Some(&json!("exit:0"))
            && response.pointer("/result/error") == Some(&Value::Null),
        "ordinary execution did not complete: {response}"
    );
    let authority: ryeos_state::objects::ExecutionProjectAuthority =
        serde_json::from_value(response["thread"]["project_authority"].clone())?;
    let ryeos_state::objects::ExecutionProjectAuthority::PinnedGeneration {
        base_snapshot_hash,
        ..
    } = authority
    else {
        anyhow::bail!("ordinary external consumer lost its pinned generation")
    };
    assert_eq!(base_snapshot_hash, snapshot_hash);
    anyhow::ensure!(
        response
            .pointer("/result/result")
            .is_some_and(|output| output.to_string().contains("selected-runtime-program-v1")),
        "actual target output missing"
    );
    ordinary_evidence::verify(
        &harness.state_path,
        &response["thread"],
        &response["result"]["result"],
        &response["result"]["artifacts"],
        snapshot_hash,
    )?;
    Ok(())
}

fn ordinary_linked_children(state: &Path, root: &str) -> anyhow::Result<Vec<String>> {
    // Exact-parent authoritative launch links, not thread-list discovery.
    let db = rusqlite::Connection::open_with_flags(
        state.join(".ai/state/runtime.sqlite3"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )?;
    let mut statement = db.prepare(
        "SELECT child_thread_id FROM thread_child_link WHERE parent_thread_id=?1 ORDER BY child_thread_id",
    )?;
    Ok(statement
        .query_map([root], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?)
}

/// Read the daemon's one-shot scoped-producer testimony for an exact accepted
/// verifier root. This is an operational assertion for the real-daemon fixture,
/// not a substitute for the verifier's independent semantic claims.
fn exact_scoped_producer_observation(state: &Path, root: &str) -> anyhow::Result<Value> {
    use anyhow::{Context as _, ensure};

    let db = rusqlite::Connection::open_with_flags(
        state.join(".ai/state/runtime.sqlite3"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )?;
    let mut statement = db.prepare(
        "SELECT attempt_id, recipe_digest, recipe_generation, scenario_digest, \
         natural_empty_receipt_digest, observation_object_hash, phase \
         FROM scoped_child_attempt WHERE owner_thread_id=?1",
    )?;
    let rows = statement
        .query_map([root], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, Option<String>>(4)?,
                row.get::<_, Option<String>>(5)?,
                row.get::<_, String>(6)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    ensure!(
        rows.len() == 1,
        "expected exactly one scoped attempt for root {root}"
    );
    let (attempt, recipe, generation, scenario, receipt, object, phase) = &rows[0];
    ensure!(
        matches!(phase.as_str(), "natural_scope_empty" | "retired"),
        "scoped producer has not settled naturally"
    );
    let receipt = receipt
        .as_deref()
        .context("missing natural-empty receipt")?;
    let object = object
        .as_deref()
        .context("missing scoped observation object")?;
    let cas = lillux::CasStore::new(state.join(".ai/state/objects"));
    let observation = cas
        .get_object(object)?
        .context("retained scoped observation absent")?;
    ensure!(
        observation["schema"] == "ryeos.scoped_producer_observation.v4"
            && observation["attempt_id"] == *attempt
            && observation["recipe_digest"] == *recipe
            && observation["recipe_generation"] == *generation
            && observation["producer_source"]["recipe_digest"] == *recipe
            && observation["producer_source"]["bundle_generation_identity"] == *generation
            && observation["scenario_digest"] == *scenario
            && observation["natural_empty_receipt_digest"] == receipt,
        "scoped observation differs from exact journal attempt"
    );
    ensure!(
        observation["subprocess_success"] == true
            && observation["producer_exit_clean"] == true
            && observation["timed_out"] == false
            && observation["stdout_truncated"] == false
            && observation["stderr_truncated"] == false
            && observation["output_limit_exceeded"].is_null()
            && observation["launcher_refusal"].is_null(),
        "scoped producer did not complete fault-free"
    );
    Ok(observation)
}

fn install_signed_independent_verifier_fixture(
    state: &Path,
    keys: &common::fast_fixture::FastFixture,
    scenario: &independent_verifier_scenario::IndependentVerifierScenario,
    verifier_bytes: &[u8],
    reserved_resume_race: bool,
) -> anyhow::Result<()> {
    use anyhow::{Context as _, ensure};

    ensure!(!verifier_bytes.is_empty(), "verifier executable is empty");
    let bundle_name = "independent-runtime-fixture";
    let bundle = state.join(bundle_name);
    ensure!(
        !bundle.exists(),
        "independent verifier bundle already exists"
    );
    let binary = common::fast_fixture::install_signed_bundle_binary(
        &bundle,
        independent_verifier_scenario::VERIFIER_BIN,
        verifier_bytes,
        &keys.publisher,
    )?;
    ensure!(
        binary.ends_with("/independent-runtime-verifier"),
        "installed verifier binary changed coordinate"
    );
    let signed_sources = if reserved_resume_race {
        scenario.signed_sources_for_reserved_resume_race(
            &keys.publisher,
            common::fast_fixture::FAST_FIXTURE_TIME,
        )?
    } else {
        scenario.signed_sources(&keys.publisher, common::fast_fixture::FAST_FIXTURE_TIME)?
    };
    for (relative, signed) in signed_sources {
        let target = bundle.join(relative);
        std::fs::create_dir_all(target.parent().context("signed source has no parent")?)?;
        std::fs::write(target, signed)?;
    }
    let manifest = "name: independent-runtime-fixture\nversion: 1.0.0\ndescription: Exact signed independent verifier fixture\nprovides_kinds: []\nrequires_kinds: [tool, config]\nuses_kinds: []\n";
    std::fs::write(
        bundle.join(".ai/manifest.yaml"),
        lillux::signature::sign_content_at(
            manifest,
            &keys.publisher,
            "#",
            None,
            common::fast_fixture::FAST_FIXTURE_TIME,
        ),
    )?;
    common::fast_fixture::register_presigned_fixture_bundle(state, bundle_name, &bundle, keys)
}

fn admit_independent_verifier_input_root(
    state: &Path,
    keys: &common::fast_fixture::FastFixture,
    input_root: &Path,
) -> anyhow::Result<()> {
    use anyhow::{Context as _, ensure};
    use ryeos_app::node_policy::sections::external_content::{
        ExternalContentImportPolicyRecord, ExternalContentImportRoot,
    };

    ensure!(
        input_root.is_absolute(),
        "verifier input root must be absolute"
    );
    let root = lillux::PinnedDirectory::open(input_root)?
        .context("exact independent verifier input root absent")?;
    let (containing_device, root_inode) = root.device_inode()?;
    let path = state.join(".ai/node/policies/external_content.yaml");
    let source = std::fs::read_to_string(&path)?;
    let mut policy: ExternalContentImportPolicyRecord =
        serde_yaml::from_str(&lillux::signature::strip_signature_lines(&source))?;
    ensure!(policy.roots.is_empty(), "fixture has ambient import roots");
    ensure!(
        policy.limits.max_file_bytes >= 268_435_456 && policy.limits.max_total_bytes >= 268_435_456,
        "current import policy cannot admit the bounded Codex executable"
    );
    policy.roots.insert(
        "independent-verifier-inputs".into(),
        ExternalContentImportRoot {
            path: input_root.to_path_buf(),
            containing_device,
            root_inode,
        },
    );
    policy.validate()?;
    std::fs::write(
        path,
        lillux::signature::sign_content_at(
            &serde_yaml::to_string(&policy)?,
            &keys.node,
            "#",
            None,
            common::fast_fixture::FAST_FIXTURE_TIME,
        ),
    )?;
    Ok(())
}

async fn import_independent_verifier_tree(
    harness: &DaemonHarness,
    member: &str,
    storage: &str,
    maximum_bytes: u64,
) -> anyhow::Result<Value> {
    use anyhow::{Context as _, ensure};

    ensure!(
        matches!(
            member,
            "subject" | "controller" | "tools" | "configurations"
        ) && matches!(storage, "content" | "large_content")
            && maximum_bytes > 0,
        "unknown verifier import request"
    );
    let imported = production_service(
        harness,
        "service:external-content/import",
        json!({
            "source":"filesystem", "root":"independent-verifier-inputs",
            "path":member, "shape":"tree", "storage":storage,
            "maximum_bytes":maximum_bytes,
        }),
    )
    .await?;
    let manifest = imported["manifest_hash"]
        .as_str()
        .context("verifier import has no manifest hash")?;
    ensure!(
        lillux::valid_hash(manifest)
            && imported["entry_count"].as_u64().is_some_and(|n| n > 0)
            && imported["total_bytes"]
                .as_u64()
                .is_some_and(|n| n <= maximum_bytes),
        "verifier import returned an invalid exact tree"
    );
    Ok(imported)
}

/// The public service, not fixture-side hashing, produces the four identities
/// that can subsequently be committed into the signed verifier scenario.
async fn import_independent_verifier_trees(harness: &DaemonHarness) -> anyhow::Result<[Value; 4]> {
    let mut imports = Vec::with_capacity(4);
    for (member, storage, limit) in [
        ("subject", "large_content", 268_435_456),
        ("controller", "content", 64 * 1024 * 1024),
        ("tools", "content", 64 * 1024 * 1024),
        ("configurations", "content", 1024 * 1024),
    ] {
        imports.push(import_independent_verifier_tree(harness, member, storage, limit).await?);
    }
    Ok(imports.try_into().expect("four fixed verifier imports"))
}

fn require_independent_verifier_imports_match_scenario(
    imports: &[Value; 4],
    scenario: &independent_verifier_scenario::IndependentVerifierScenario,
) -> anyhow::Result<()> {
    use anyhow::ensure;

    for ((member, imported), expected) in ["subject", "controller", "tools", "configurations"]
        .into_iter()
        .zip(imports)
        .zip([
            &scenario.subject_manifest_hash,
            &scenario.controller_manifest_hash,
            &scenario.tools_manifest_hash,
            &scenario.configurations_manifest_hash,
        ])
    {
        ensure!(
            imported["manifest_hash"] == expected.as_str(),
            "public {member} import differs from the signed direct scenario"
        );
    }
    Ok(())
}

async fn bind_independent_verifier_tree(
    harness: &DaemonHarness,
    imported: &Value,
    expected_publisher: &str,
) -> anyhow::Result<Value> {
    use anyhow::ensure;

    let bound = production_service(
        harness,
        "service:external-content/bind",
        json!({
            "staging_id":imported["staging_id"],
            "request_digest":imported["request_digest"],
            "manifest_hash":imported["manifest_hash"],
            "consumer_ref":independent_verifier_scenario::TOOL_REF,
            "consumer_kind":"installed_bundle",
        }),
    )
    .await?;
    ensure!(
        bound["manifest_hash"] == imported["manifest_hash"]
            && bound["consumer_ref"] == independent_verifier_scenario::TOOL_REF
            && bound["publisher_fingerprint"] == expected_publisher,
        "verifier tree binding changed its signed consumer or manifest"
    );
    Ok(bound)
}

fn stage_independent_verifier_inputs(
    scenario: &independent_verifier_scenario::IndependentVerifierScenario,
) -> anyhow::Result<tempfile::TempDir> {
    use anyhow::{Context as _, ensure};

    let root = tempfile::tempdir()?;
    for member in [
        "subject/bin",
        "controller/bin",
        "tools/bin",
        "configurations",
    ] {
        std::fs::create_dir_all(root.path().join(member))?;
    }
    let link_binary = |variable: &str,
                       relative: &str,
                       limit: u64,
                       expected_hash: Option<&str>|
     -> anyhow::Result<()> {
        let source = PathBuf::from(
            std::env::var_os(variable).with_context(|| format!("{variable} is required"))?,
        );
        ensure!(source.is_absolute(), "{variable} must be absolute");
        let pinned = lillux::secure_fs::open_pinned_regular_file_no_follow(&source)?;
        let observation = pinned.observation()?;
        ensure!(
            (1..=limit).contains(&observation.size()) && pinned.permission_mode()? == 0o755,
            "{variable} changed executable shape"
        );
        if let Some(expected) = expected_hash {
            ensure!(
                pinned.digest_stable_exact(&observation)? == expected,
                "{variable} differs from the signed scenario"
            );
        }
        let destination = root.path().join(relative);
        // The opt-in fixture lives on the same filesystem as its retained
        // source binaries. Fail rather than silently duplicating a large
        // executable when the exact hard-link boundary is unavailable.
        std::fs::hard_link(&source, &destination)
            .with_context(|| format!("hard-link exact {variable} into fixture"))?;
        Ok(())
    };
    link_binary(
        "RYEOS_TEST_CODEX_0147_BIN",
        "subject/bin/codex",
        268_435_456,
        Some(&scenario.codex_sha256),
    )?;
    link_binary(
        "RYEOS_TEST_ROUTED_GUEST_BIN",
        "controller/bin/ryeos-synthetic-routed-guest",
        64 * 1024 * 1024,
        Some(&scenario.relay_sha256),
    )?;
    link_binary(
        "RYEOS_TEST_ZSH_BIN",
        "tools/bin/zsh",
        64 * 1024 * 1024,
        None,
    )?;
    link_binary("RYEOS_TEST_RG_BIN", "tools/bin/rg", 64 * 1024 * 1024, None)?;
    let scripted = include_str!(
        "../../../../tests/e2e/external-execution/fixtures/independent-scripted-config.toml.template"
    )
    .replace("{ORIGIN}", &scenario.responses_origin);
    ensure!(
        lillux::sha256_hex(scripted.as_bytes()) == scenario.scripted_baseline_sha256,
        "credential-free scripted baseline differs from signed scenario"
    );
    let template = include_str!(
        "../../../../tests/e2e/external-execution/fixtures/independent-environments.toml.template"
    );
    ensure!(
        lillux::sha256_hex(template.as_bytes()) == scenario.command_environment_template_sha256,
        "command environment template differs from signed scenario"
    );
    let admitted = admit_signed_codex_worker()?;
    let admitted_profile = admitted.profile;
    let source_projection = admitted.source;
    let profile = lillux::canonical_json(&admitted_profile.contract)?.into_bytes();
    ensure!(
        profile.len() <= 64 * 1024,
        "admitted profile exceeds fixture bound"
    );
    let parsed = &admitted_profile.contract;
    let mut configuration = serde_json::to_value(scenario)?;
    configuration
        .as_object_mut()
        .context("scenario configuration is not an object")?
        .remove("qualification_use");
    let derived = independent_verifier_scenario::IndependentVerifierScenario::from_admitted_inputs(
        serde_json::from_value(configuration)?,
        &admitted_profile,
        &source_projection,
    )?;
    ensure!(
        derived.qualification_use == scenario.qualification_use
            && lillux::sha256_hex(&profile) == scenario.qualification_use.profile_hash
            && parsed["external_candidate"] == serde_json::to_value(&scenario.requirement)?
            && parsed["transport"] == "stdio_jsonrpc"
            && parsed["workload_client"].is_null()
            && parsed["workload_realization_id"] == "codex",
        "exact compiled profile differs from signed qualification use"
    );
    let configurations = root.path().join("configurations");
    std::fs::write(configurations.join("scripted.config.toml"), scripted)?;
    std::fs::write(configurations.join("environments.toml.template"), template)?;
    std::fs::write(configurations.join("admitted-profile.json"), &profile)?;
    Ok(root)
}

#[cfg(all(unix, feature = "handoff-test-support"))]
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires exact Codex and verifier binaries, four input trees, and authored admitted evidence; uses only a local scripted provider"]
async fn signed_independent_verifier_runs_direct_codex_and_refuses_unqualified_claims()
-> anyhow::Result<()> {
    use anyhow::{Context as _, ensure};

    let scenario_path = PathBuf::from(
        std::env::var_os("RYEOS_TEST_INDEPENDENT_SCENARIO_JSON")
            .context("RYEOS_TEST_INDEPENDENT_SCENARIO_JSON is required")?,
    );
    let verifier_path = PathBuf::from(
        std::env::var_os("RYEOS_TEST_INDEPENDENT_VERIFIER_BIN")
            .context("RYEOS_TEST_INDEPENDENT_VERIFIER_BIN is required")?,
    );
    let scenario: independent_verifier_scenario::IndependentVerifierScenario =
        serde_json::from_slice(&lillux::secure_fs::read_regular_file_bounded_no_follow(
            &scenario_path,
            16 * 1024,
        )?)?;
    let parameters = scenario.parameters()?;
    let input_root = stage_independent_verifier_inputs(&scenario)?;
    let verifier_bytes =
        lillux::secure_fs::read_regular_file_bounded_no_follow(&verifier_path, 64 * 1024 * 1024)?;
    let (mut harness, keys) = DaemonHarness::start_fast_with(
        |state, _, fixture| {
            common::fast_fixture::register_standard_bundle(state, fixture)?;
            admit_independent_verifier_input_root(state, fixture, input_root.path())
        },
        |_| {},
    )
    .await?;
    harness.retain_evidence_on_drop(true);
    let staged = import_independent_verifier_trees(&harness).await?;
    require_independent_verifier_imports_match_scenario(&staged, &scenario)?;
    harness.kill_daemon().await?;
    install_signed_independent_verifier_fixture(
        &harness.state_path,
        &keys,
        &scenario,
        &verifier_bytes,
        false,
    )?;
    harness.respawn_with(|_| {}).await?;
    for imported in &staged {
        bind_independent_verifier_tree(&harness, imported, &keys.publisher_fp()).await?;
    }
    let launch_id = "L-f321054b98a409f7da7cd63e7bdacc09";
    let (accepted, terminal) = public_launch::terminal_launch(
        &harness,
        json!({
            "item_ref":independent_verifier_scenario::TOOL_REF,
            "launch_id":launch_id,"ref_bindings":{},"parameters":parameters,
            "execution_policy":ExecutionPolicy::projectless(ExecutionResponse::Accepted)
                .exclude_operator_vault(),
        }),
        launch_id,
        std::time::Duration::from_secs(330),
    )
    .await?;
    let root = accepted["thread_id"]
        .as_str()
        .context("accepted direct verifier root absent")?;
    ensure!(
        terminal.pointer("/thread/status") == Some(&json!("failed")),
        "incomplete direct qualification incorrectly succeeded"
    );
    let error = terminal
        .pointer("/thread/error")
        .context("failed direct verifier has no authoritative error")?;
    ensure!(
        serde_json::to_string(error)?.contains(
            "effective namespace environment and complete qualification evidence remain unproven"
        ),
        "direct run failed before its explicit no-claims boundary: {error}"
    );
    let scoped = exact_scoped_producer_observation(&harness.state_path, root)?;
    let recipe_digest = scenario.expected_producer_recipe.digest()?;
    ensure!(
        scoped["launch_owner"]["thread_id"] == root
            && scoped["recipe_digest"] == recipe_digest
            && scoped["producer_source"]["recipe_digest"] == recipe_digest
            && scoped["relay_handoff"].is_object()
            && scoped["relay_handoff"]["attempt_id"] == scoped["attempt_id"]
            && scoped["applied_launch"].is_object()
            && scoped["process_identity"].is_object(),
        "direct scoped attempt lacks joined daemon execution evidence"
    );
    let output = scoped["stdout"]
        .as_str()
        .context("direct Codex target has no daemon-retained app-server output")?;
    let mut response_ids = std::collections::BTreeSet::new();
    let mut completed_turns = 0usize;
    for line in output.lines() {
        let frame: Value = serde_json::from_str(line)
            .context("daemon-retained direct Codex output contains a non-JSON frame")?;
        ensure!(frame.is_object(), "direct Codex emitted a non-object frame");
        if let Some(id) = frame["id"].as_u64() {
            response_ids.insert(id);
        }
        if frame["method"] == "turn/completed"
            && frame["params"]["turn"]["status"] == "completed"
            && frame["params"]["threadId"].as_str().is_some()
            && frame["params"]["turn"]["id"].as_str().is_some()
        {
            completed_turns += 1;
        }
    }
    ensure!(
        response_ids == [1, 2, 3].into() && completed_turns == 1,
        "daemon-retained direct target did not exchange the exact scripted app-server turn"
    );
    eprintln!(
        "signed direct verifier fail-closed evidence: {}",
        json!({
            "launch_id":launch_id,"root":root,"terminal":terminal,
            "scoped_attempt":scoped["attempt_id"],
            "scoped_receipt":scoped["natural_empty_receipt_digest"],
            "provider_contact_kind":"credential-free scripted local peer",
            "qualification_claims_issued":false,
        })
    );
    Ok(())
}

#[cfg(all(unix, feature = "handoff-test-support"))]
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires exact verifier executable, four input trees and authored scenario; no provider credentials"]
async fn signed_independent_verifier_proves_reserved_resume_race_and_fails_closed()
-> anyhow::Result<()> {
    use anyhow::{Context as _, ensure};

    let scenario_path = PathBuf::from(
        std::env::var_os("RYEOS_TEST_INDEPENDENT_SCENARIO_JSON")
            .context("RYEOS_TEST_INDEPENDENT_SCENARIO_JSON is required")?,
    );
    let verifier_path = PathBuf::from(
        std::env::var_os("RYEOS_TEST_INDEPENDENT_VERIFIER_BIN")
            .context("RYEOS_TEST_INDEPENDENT_VERIFIER_BIN is required")?,
    );
    let scenario_bytes =
        lillux::secure_fs::read_regular_file_bounded_no_follow(&scenario_path, 16 * 1024)?;
    let scenario: independent_verifier_scenario::IndependentVerifierScenario =
        serde_json::from_slice(&scenario_bytes)?;
    let parameters = scenario.parameters()?;
    let input_root = stage_independent_verifier_inputs(&scenario)?;
    let verifier_bytes =
        lillux::secure_fs::read_regular_file_bounded_no_follow(&verifier_path, 64 * 1024 * 1024)?;
    let (mut harness, keys) = DaemonHarness::start_fast_with(
        |state, _, fixture| {
            common::fast_fixture::register_standard_bundle(state, fixture)?;
            admit_independent_verifier_input_root(state, fixture, input_root.path())
        },
        |_| {},
    )
    .await?;
    harness.retain_evidence_on_drop(true);
    let staged = import_independent_verifier_trees(&harness).await?;
    require_independent_verifier_imports_match_scenario(&staged, &scenario)?;
    harness.kill_daemon().await?;
    install_signed_independent_verifier_fixture(
        &harness.state_path,
        &keys,
        &scenario,
        &verifier_bytes,
        true,
    )?;
    let (mut gate, child) = common::ScopedReservedAttemptGate::pair()?;
    harness
        .respawn_with(move |command| {
            common::ScopedReservedAttemptGate::attach(command, child)
                .expect("bind signed scoped race gate");
        })
        .await?;
    for imported in &staged {
        bind_independent_verifier_tree(&harness, imported, &keys.publisher_fp()).await?;
    }
    let launch_id = "L-45e7fd8950c6f85790be5301dcd066cb";
    let (launched, held) = tokio::join!(
        public_launch::terminal_launch(
            &harness,
            json!({
                "item_ref":independent_verifier_scenario::TOOL_REF,
                "launch_id":launch_id,"ref_bindings":{},"parameters":parameters,
                "execution_policy":ExecutionPolicy::projectless(ExecutionResponse::Accepted)
                    .exclude_operator_vault(),
            }),
            launch_id,
            std::time::Duration::from_secs(330),
        ),
        async {
            let evidence = gate.wait_reached().await?;
            gate.release()?;
            anyhow::Ok(evidence)
        },
    );
    let (accepted, terminal) = launched?;
    let held = held?;
    ensure!(
        terminal.pointer("/thread/status") == Some(&json!("failed")),
        "incomplete verifier incorrectly qualified or failed to terminate"
    );
    let error = terminal
        .pointer("/thread/error")
        .context("failed verifier has no authoritative terminal error")?;
    ensure!(
        serde_json::to_string(error)?.contains("no qualification claims issued"),
        "verifier failed for a reason other than its explicit no-claims gate"
    );
    let root = accepted["thread_id"]
        .as_str()
        .context("accepted verifier root absent")?;
    ensure!(
        held["root_thread_id"] == root
            && held["attempt_id"]
                .as_str()
                .is_some_and(|id| id.starts_with("scoped-"))
            && held["launch_owner"]["thread_id"] == root,
        "reserved RESUME gate differs from accepted verifier root"
    );
    ensure!(
        serde_json::to_string(error)?.contains("scoped race START/RESUME exact locators matched"),
        "signed verifier did not report exact START/RESUME equality"
    );
    let scoped = exact_scoped_producer_observation(&harness.state_path, root)?;
    ensure!(
        held["attempt_id"] == scoped["attempt_id"]
            && held["launch_owner"] == scoped["launch_owner"],
        "reserved gate does not match the sole observed scoped attempt"
    );
    eprintln!(
        "signed independent verifier fail-closed evidence: {}",
        json!({
            "launch_id":launch_id,"root":root,"terminal":terminal,
            "scoped_attempt":scoped["attempt_id"],
            "scoped_receipt":scoped["natural_empty_receipt_digest"],
            "scoped_success":scoped["producer_exit_clean"],
            "resume_observed_at_reserved_before_release":true,
            "provider_contact_kind":"credential-free scripted local peer",
            "explicit_no_claims_gate_observed":true,
        })
    );
    Ok(())
}

fn ordinary_lifecycle_contacts(provider: &Path) -> anyhow::Result<Vec<u8>> {
    let root = lillux::PinnedDirectory::open(provider)?.context("retained provider root")?;
    let file = root
        .open_pinned_regular(std::ffi::OsStr::new("lifecycle-contacts.json"), false)?
        .context("provider lifecycle contact witness absent")?;
    let bytes = file.read_bounded(256 * 1024)?;
    let value: Value = serde_json::from_slice(&bytes)?;
    anyhow::ensure!(
        lillux::canonical_json(&value)?.as_bytes() == bytes,
        "noncanonical contact witness"
    );
    anyhow::ensure!(value["schema"] == 1, "unknown contact witness schema");
    let contacts = value["contacts"].as_array().context("contact entries")?;
    anyhow::ensure!(
        !contacts.is_empty() && contacts.len() <= 256,
        "invalid contact witness size"
    );
    for (index, contact) in contacts.iter().enumerate() {
        anyhow::ensure!(
            contact["sequence"] == (index + 1) as u64,
            "contact sequence changed"
        );
        anyhow::ensure!(
            contact["request_sha256"]
                .as_str()
                .is_some_and(lillux::valid_hash),
            "invalid contact digest"
        );
    }
    for operation in ["allocate", "activate_supervisor", "terminate"] {
        anyhow::ensure!(
            contacts.iter().any(|entry| entry["operation"] == operation),
            "missing lifecycle boundary {operation}"
        );
    }
    Ok(bytes)
}

fn verify_ordinary_chain(chain: &Value, root: &str, children: &[String]) -> anyhow::Result<()> {
    let threads = chain["threads"]
        .as_array()
        .context("exact graph chain threads")?;
    anyhow::ensure!(
        threads.len() == 1,
        "ordinary graph changed chain membership"
    );
    let thread = &threads[0];
    anyhow::ensure!(
        thread["thread_id"] == root
            && thread["chain_root_id"] == root
            && thread["status"] == "completed"
            && thread.get("successor_thread_id") == Some(&Value::Null),
        "ordinary graph root is not completed without a successor"
    );
    // Spawned children own independent chains. The public chain service still
    // returns their outbound edges; these are not continuation membership.
    let edges = chain["edges"]
        .as_array()
        .context("exact graph chain edges")?;
    anyhow::ensure!(
        edges.len() == children.len(),
        "ordinary child edge count differs"
    );
    let expected: std::collections::BTreeSet<_> = children.iter().map(String::as_str).collect();
    anyhow::ensure!(
        expected.len() == children.len(),
        "duplicate ordinary child identities"
    );
    let mut observed = std::collections::BTreeSet::new();
    for edge in edges {
        let target = edge["target_thread_id"]
            .as_str()
            .context("ordinary edge target")?;
        anyhow::ensure!(
            edge["chain_root_id"] == root
                && edge["source_thread_id"] == root
                && edge["edge_type"] == "spawned"
                && edge["metadata"] == "dispatch"
                && expected.contains(target)
                && observed.insert(target),
            "ordinary chain edge does not match the exact dispatched child"
        );
    }
    anyhow::ensure!(
        observed == expected,
        "ordinary chain lost a dispatched child"
    );
    Ok(())
}

#[test]
fn ordinary_chain_distinguishes_spawned_children_from_continuations() {
    let root = "T-root";
    let child = "T-child".to_owned();
    let chain = json!({
        "threads":[{"thread_id":root,"chain_root_id":root,"status":"completed","successor_thread_id":null}],
        "edges":[{"chain_root_id":root,"source_thread_id":root,"target_thread_id":child,"edge_type":"spawned","metadata":"dispatch"}]
    });
    let children = vec![child];
    verify_ordinary_chain(&chain, root, &children).unwrap();
    let mut replay = chain.clone();
    replay["edges"] = json!([]);
    verify_ordinary_chain(&replay, root, &[]).unwrap();
    assert!(verify_ordinary_chain(&chain, root, &[]).is_err());
    assert!(verify_ordinary_chain(&replay, root, &children).is_err());
    for field in [
        "chain_root_id",
        "source_thread_id",
        "target_thread_id",
        "edge_type",
        "metadata",
    ] {
        let mut bad = chain.clone();
        bad["edges"][0][field] = json!("wrong");
        assert!(
            verify_ordinary_chain(&bad, root, &children).is_err(),
            "accepted wrong {field}"
        );
    }
    let mut duplicate = chain.clone();
    duplicate["edges"]
        .as_array_mut()
        .unwrap()
        .push(chain["edges"][0].clone());
    assert!(verify_ordinary_chain(&duplicate, root, &children).is_err());
    assert!(
        verify_ordinary_chain(&duplicate, root, &[children[0].clone(), "T-other".into()]).is_err()
    );
    for field in [
        "thread_id",
        "chain_root_id",
        "status",
        "successor_thread_id",
    ] {
        let mut bad = chain.clone();
        bad["threads"][0][field] = json!("wrong");
        assert!(
            verify_ordinary_chain(&bad, root, &children).is_err(),
            "accepted wrong root {field}"
        );
    }
    let mut continued = chain.clone();
    continued["threads"]
        .as_array_mut()
        .unwrap()
        .push(chain["threads"][0].clone());
    assert!(verify_ordinary_chain(&continued, root, &children).is_err());
}

async fn ordinary_recorded_pass(
    fixture: &OrdinaryPublicFixture,
    launch_id: &str,
) -> anyhow::Result<(
    Value,
    ryeos_runtime::callback_contract::RuntimeDispatchEvidence,
    Vec<String>,
)> {
    let mut request = ordinary_direct::recorded_request(
        fixture.project.path(),
        &fixture.snapshot_hash,
        ExecutionResponse::Accepted,
    )?;
    request["launch_id"] = json!(launch_id);
    // Ingress observation is never retried, including timeout or lost ACK.
    let (status, accepted) = tokio::time::timeout(
        std::time::Duration::from_secs(60),
        fixture.harness.post_json("/execute/launch", request),
    )
    .await
    .context("recorded external acceptance uncertain; retain launch ID, never relaunch")??;
    anyhow::ensure!(
        status == reqwest::StatusCode::ACCEPTED,
        "recorded graph refused: {status}: {accepted}"
    );
    assert_eq!(accepted["launch_id"], launch_id);
    let root = accepted["thread_id"]
        .as_str()
        .context("accepted recorded root")?
        .to_owned();
    eprintln!("recorded external accepted: launch_id={launch_id}, root={root}");
    let detail = tokio::time::timeout(std::time::Duration::from_secs(180), async {
        loop {
            let response = production_service(
                &fixture.harness,
                "service:threads/get",
                json!({"thread_id":root}),
            )
            .await?;
            let status = response
                .pointer("/thread/status")
                .and_then(Value::as_str)
                .context("exact recorded thread status")?;
            if ryeos_state::objects::ThreadStatus::from_str_lossy(status)
                .is_some_and(|status| status.is_terminal())
            {
                return Ok::<_, anyhow::Error>(response);
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
    })
    .await
    .context("recorded root observation expired; inspect original root, never relaunch")??;
    let receipts = production_service(
        &fixture.harness,
        "service:threads/receipts",
        json!({"thread_id":root}),
    )
    .await?;
    let chain = production_service(
        &fixture.harness,
        "service:threads/chain",
        json!({"thread_id":root}),
    )
    .await?;
    let children = ordinary_linked_children(&fixture.harness.state_path, &root)?;
    eprintln!(
        "recorded external evidence: {}",
        json!({
            "launch_id":launch_id,"detail":detail,"receipts":receipts,"chain":chain,"children":children,
        })
    );
    anyhow::ensure!(
        detail["thread"]["status"] == "completed",
        "recorded graph did not complete: {detail}"
    );
    assert_eq!(detail["thread"]["thread_id"], root);
    assert_eq!(detail["thread"]["chain_root_id"], root);
    assert_eq!(
        detail["thread"]["project_authority"]["base_snapshot_hash"],
        fixture.snapshot_hash
    );
    assert_eq!(
        detail["thread"]["project_authority"]["snapshot_hash"],
        fixture.snapshot_hash
    );
    verify_ordinary_chain(&chain, &root, &children)?;
    let nodes = receipts["receipts"]
        .as_array()
        .context("recorded graph receipts")?;
    assert_eq!(nodes.len(), 1);
    assert_eq!(nodes[0]["node"], "run");
    assert!(nodes[0]["error"].is_null());
    let dispatch: ryeos_runtime::callback_contract::RuntimeDispatchEvidence =
        serde_json::from_value(nodes[0]["dispatch"].clone())?;
    dispatch.validate()?;
    Ok((detail, dispatch, children))
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires current daemon/synthetic binaries and native guest isolation prerequisites"]
async fn recorded_external_tool_replays_after_real_daemon_restart() -> anyhow::Result<()> {
    use ryeos_runtime::callback_contract::{
        RuntimeDispatchEffectClass, RuntimeDispatchPublication, RuntimeDispatchSource,
    };
    let mut fixture = prepare_public_ordinary_fixture(true).await?;
    let (first, executed, children) =
        ordinary_recorded_pass(&fixture, "L-f87889391aeb628e19d2cf0ab3444501").await?;
    assert_eq!(executed.source, RuntimeDispatchSource::Executed);
    assert_eq!(executed.effect_class, RuntimeDispatchEffectClass::Recorded);
    assert_eq!(executed.publication, RuntimeDispatchPublication::Inserted);
    assert!(executed.effect_identity.is_some() && executed.record_hash.is_some());
    assert_eq!(executed.replayed_from, None);
    assert_eq!(
        children.len(),
        1,
        "first action must launch exactly one external Tool"
    );
    let child = production_service(
        &fixture.harness,
        "service:threads/get",
        json!({"thread_id":children[0]}),
    )
    .await?;
    eprintln!("recorded external authoritative child: {child}");
    assert_eq!(child["thread"]["status"], "completed");
    assert_eq!(child["thread"]["item_ref"], ordinary_direct::TOOL_REF);
    ordinary_evidence::verify(
        &fixture.harness.state_path,
        &child["thread"],
        &child["result"]["result"],
        &child["artifacts"],
        &fixture.snapshot_hash,
    )?;
    let provider = fixture.provider.path().join("provider");
    let contacts = ordinary_lifecycle_contacts(&provider)?;
    let first_result = first
        .pointer("/result/result/result")
        .context("recorded graph return")?;
    assert!(
        first_result
            .to_string()
            .contains("selected-runtime-program-v1")
    );
    assert!(
        !fixture.relay.0.is_finished(),
        "channel relay stopped before restart"
    );

    // Only after authoritative completion and independently proved guest
    // death. Restart this disposable daemon, preserving its exact state,
    // endpoint, source, product binding and action. No installed node changes.
    fixture.harness.kill_daemon().await?;
    fixture.harness.respawn_with(|_| {}).await?;
    assert_eq!(
        ordinary_lifecycle_contacts(&provider)?,
        contacts,
        "restart contacted provider lifecycle"
    );
    assert!(
        !fixture.relay.0.is_finished(),
        "channel relay stopped during restart"
    );
    let (second, replayed, children) =
        ordinary_recorded_pass(&fixture, "L-f87889391aeb628e19d2cf0ab3444502").await?;
    assert_eq!(replayed.source, RuntimeDispatchSource::EffectRecord);
    assert_eq!(replayed.effect_class, RuntimeDispatchEffectClass::Recorded);
    assert_eq!(
        replayed.publication,
        RuntimeDispatchPublication::NotApplicable
    );
    assert_eq!(replayed.action_digest, executed.action_digest);
    assert_eq!(replayed.effect_identity, executed.effect_identity);
    assert_eq!(replayed.record_hash, executed.record_hash);
    assert_eq!(replayed.replayed_from, executed.record_hash);
    assert_eq!(second.pointer("/result/result/result"), Some(first_result));
    assert!(children.is_empty(), "effect replay launched a child");
    assert_eq!(
        ordinary_lifecycle_contacts(&provider)?,
        contacts,
        "effect replay contacted provider lifecycle"
    );
    eprintln!(
        "recorded external replay summary: {}",
        json!({
            "first_root":first["thread"]["thread_id"],"second_root":second["thread"]["thread_id"],
            "snapshot":fixture.snapshot_hash,"executed":executed,"replayed":replayed,
            "lifecycle_contact_witness_sha256":lillux::sha256_hex(&contacts),
            "executed_count":1,"effect_record_count":1,"new_provider_lifecycle_calls":0,
            "inspection_subprocesses_outside_contact_claim":true,
        })
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "build synthetic adapter bins and set RYEOS_TEST_SYNTHETIC_BIN_DIR"]
async fn signed_external_adapter_remains_admitted_after_real_daemon_restart() {
    let directory = PathBuf::from(
        std::env::var_os("RYEOS_TEST_SYNTHETIC_BIN_DIR")
            .expect("RYEOS_TEST_SYNTHETIC_BIN_DIR must identify exact built test binaries"),
    );
    let adapter = artifact(&directory, "ryeos-synthetic-external-lifecycle-adapter");
    let supervisor = artifact(&directory, "ryeos-synthetic-external-candidate-supervisor");
    let launcher = artifact(&directory, "ryeos-synthetic-external-candidate-launcher");
    let connector = artifact(&directory, "ryeos-synthetic-external-candidate-connector");
    let configuration = artifact(&directory, "ryeos-synthetic-codex-external-configuration");
    let artifacts = signed_bundle::SyntheticExternalArtifacts {
        adapter: &adapter,
        supervisor: &supervisor,
        launcher: &launcher,
        connector: &connector,
        configuration: &configuration,
    };

    let (mut harness, _) = DaemonHarness::start_fast_with(
        |state_path, _user_space, fixture| {
            common::fast_fixture::register_standard_bundle(state_path, fixture)?;
            let (bundle, _, _) = signed_bundle::install_signed_test_bundle(
                state_path,
                true,
                &fixture.publisher,
                &artifacts,
            );
            common::fast_fixture::register_presigned_fixture_bundle(
                state_path,
                "synthetic-external",
                &bundle,
                fixture,
            )
        },
        |_| {},
    )
    .await
    .expect("daemon must admit signed external adapter bundle");

    let manifest = harness
        .state_path
        .join("synthetic-external/.ai/manifest.yaml");
    let signed_before = std::fs::read(&manifest).expect("signed fixture manifest");
    assert!(
        String::from_utf8_lossy(&signed_before).contains("exact_activation_reconciliation"),
        "fixture manifest lost external lifecycle capabilities"
    );

    harness
        .kill_daemon()
        .await
        .expect("kill exact daemon process");
    harness
        .respawn_with(|_| {})
        .await
        .expect("restart with retained signed adapter bundle");
    assert_eq!(
        std::fs::read(&manifest).expect("retained fixture manifest"),
        signed_before,
        "restart must retain the exact signed adapter manifest"
    );
}

/// Exact public launch coordinates, not a synthesized capture authority.
struct RetainedRuntimeProducer {
    chain_root_id: String,
    thread_id: String,
}

impl RetainedRuntimeProducer {
    fn capture_parameters(&self) -> Value {
        json!({
            "chain_root_id": self.chain_root_id,
            "thread_id": self.thread_id,
            "recipe_binding": retained_runtime_producer::RECIPE_BINDING,
            "product_name": retained_runtime_producer::PRODUCT_NAME,
        })
    }
}

/// Reusable public producer launch for the subsequent joined daemon fixture.
/// The project must already carry the exact signed recipe/source it will use.
async fn run_retained_runtime_producer(
    harness: &DaemonHarness,
    project: &Path,
) -> anyhow::Result<RetainedRuntimeProducer> {
    let (status, response) = harness
        .post_json(
            "/execute",
            json!({
                "item_ref": retained_runtime_producer::PRODUCER_REF,
                "ref_bindings": {},
                "project_path": project,
                "parameters": {},
                "execution_policy": ExecutionPolicy::local_pinned_capture(ExecutionResponse::Wait)
                    .exclude_operator_vault(),
            }),
        )
        .await?;
    anyhow::ensure!(
        status.is_success(),
        "producer dispatch failed: {status}: {response}"
    );
    anyhow::ensure!(
        response.pointer("/result/success") == Some(&json!(true))
            && response.pointer("/thread/status") == Some(&json!("completed")),
        "producer did not complete: {response}"
    );
    let thread = response.get("thread").context("producer thread envelope")?;
    let thread_id = thread["thread_id"]
        .as_str()
        .context("producer thread id")?
        .to_owned();
    let chain_root_id = thread["chain_root_id"]
        .as_str()
        .context("producer chain root")?
        .to_owned();
    anyhow::ensure!(
        thread_id == chain_root_id,
        "return-only producer unexpectedly continued"
    );
    ryeos_runtime::validate_runtime_thread_id(&thread_id).map_err(anyhow::Error::msg)?;
    Ok(RetainedRuntimeProducer {
        chain_root_id,
        thread_id,
    })
}

async fn capture_retained_runtime(
    harness: &DaemonHarness,
    producer: &RetainedRuntimeProducer,
) -> anyhow::Result<Value> {
    let (status, response) = harness
        .post_execute(
            "service:external-content/capture-product",
            ".",
            producer.capture_parameters(),
        )
        .await?;
    anyhow::ensure!(
        status.is_success(),
        "product capture failed: {status}: {response}"
    );
    let captured = response
        .get("result")
        .context("product capture result")?
        .clone();
    anyhow::ensure!(
        captured["state"] == "captured",
        "product was not captured: {captured}"
    );
    Ok(captured)
}

/// This fixture deliberately has no consumer relationship: it proves retained
/// input capture only. The joined worker fixture must pass the shared complete
/// recipe source to `write_retained_runtime_producer`, not substitute this one.
const RETAINED_RUNTIME_CAPTURE_RECIPE: &str = r#"category: fixtures
version: "1.0.0"
description: Exact bounded prebuilt input capture; not compiler production
build_products:
  schema: ryeos.build_products.v1
  output_roots: []
  products:
    - name: auxiliary
      source: {kind: retained_project}
      path: products/external-runtime
      shape: tree
      storage: content
      required: true
      bounds:
        maximum_entries: 2
        maximum_depth: 2
        maximum_file_bytes: 32768
        maximum_total_bytes: 32768
product_relationships:
  schema: ryeos.product_relationships.v1
  relationships: []
"#;

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires current populated signed core/standard daemon test artifacts"]
async fn public_retained_runtime_capture_uses_real_pinned_producer() -> anyhow::Result<()> {
    let (mut harness, fixture) = DaemonHarness::start_fast().await?;
    harness.retain_evidence_on_drop(true);
    let mut project = tempfile::tempdir()?;
    project.disable_cleanup(true);
    eprintln!(
        "retained producer fixture: node={}, project={}",
        harness.state_path.display(),
        project.path().display()
    );
    // This existing finite, credential-free executable is retained, not run.
    // A joined caller supplies its actual prebuilt candidate runtime instead.
    let runtime_bytes = STANDARD.decode(
        include_str!("../../../../tests/e2e/environment-products/runtime-program.b64")
            .split_whitespace()
            .collect::<String>(),
    )?;
    anyhow::ensure!(
        runtime_bytes.len() <= 32768,
        "prebuilt fixture exceeds its recipe"
    );
    retained_runtime_producer::write_retained_runtime_producer(
        project.path(),
        &fixture.publisher,
        RETAINED_RUNTIME_CAPTURE_RECIPE,
        &runtime_bytes,
    )?;
    let producer = tokio::time::timeout(
        std::time::Duration::from_secs(90),
        run_retained_runtime_producer(&harness, project.path()),
    )
    .await
    .context("producer observation timed out; retain state, do not relaunch")??;
    eprintln!(
        "retained producer: root={}, thread={}",
        producer.chain_root_id, producer.thread_id
    );

    // Deliberately invalidate the live locator after the exact producer has
    // completed. Public capture must select its retained snapshot, not re-read
    // this input or claim these replacement bytes as the produced runtime.
    std::fs::write(
        project.path().join(retained_runtime_producer::INPUT_PATH),
        b"not the retained runtime",
    )?;
    let captured = tokio::time::timeout(
        std::time::Duration::from_secs(60),
        capture_retained_runtime(&harness, &producer),
    )
    .await
    .context("capture observation timed out; retain exact producer state")??;
    let evidence: ProductCaptureEvidence = serde_json::from_value(captured["evidence"].clone())?;
    evidence.validate()?;
    assert_eq!(
        evidence.owner_principal,
        format!("fp:{}", fixture.user_fp())
    );
    assert_eq!(
        captured["coordinate_id"],
        ryeos_state::external_content::products::publication::ProductCaptureCoordinate::from_evidence(
            &evidence,
        )?
        .coordinate_id()?
    );
    let witness_hash = captured["witness_hash"]
        .as_str()
        .context("capture must return an exact witness hash")?;
    anyhow::ensure!(
        witness_hash.len() == 64
            && witness_hash
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)),
        "capture witness hash is not canonical"
    );
    eprintln!(
        "retained-runtime-capture: {}",
        json!({
            "chain_root_id": evidence.chain_root_id,
            "thread_id": evidence.thread_id,
            "owner_principal": evidence.owner_principal,
            "capsule_hash": evidence.admitted_launch_capsule_hash,
            "producer": evidence.producer,
            "result_snapshot_hash": evidence.result_project_snapshot_hash,
            "coordinate_id": captured["coordinate_id"],
            "witness_hash": witness_hash,
            "manifest_hash": evidence.manifest_hash,
        })
    );
    assert_eq!(evidence.chain_root_id, producer.chain_root_id);
    assert_eq!(evidence.thread_id, producer.thread_id);
    assert_eq!(evidence.producer, evidence.root_producer);
    assert_eq!(
        evidence.producer.canonical_ref,
        retained_runtime_producer::PRODUCER_REF
    );
    assert_eq!(evidence.recipe_ref, retained_runtime_producer::RECIPE_REF);
    assert_eq!(
        evidence.recipe_binding,
        retained_runtime_producer::RECIPE_BINDING
    );
    assert_eq!(
        evidence.declaration.name,
        retained_runtime_producer::PRODUCT_NAME
    );
    assert_eq!(evidence.workspace_output_capture_hash, None);
    assert_eq!(evidence.producer_partition_identity, None);
    assert_eq!(evidence.total_bytes, runtime_bytes.len() as u64);
    assert_eq!(evidence.entry_count, 2);
    assert_eq!(captured["idempotent"], false);

    // Pure expected-content calculation only: no CAS publication and no
    // manufactured ProductCaptureEvidence. The signed witness comes solely
    // from the public capture owner above.
    let expected_manifest = ryeos_state::objects::ExternalContentManifestObject {
        schema: ryeos_state::objects::EXTERNAL_CONTENT_TREE_SCHEMA.into(),
        kind: ryeos_state::objects::EXTERNAL_CONTENT_MANIFEST_KIND.into(),
        entries: vec![
            ryeos_state::objects::ExternalContentManifestEntry {
                path: "bin".into(),
                kind: ryeos_state::objects::ExternalContentManifestEntryKind::Dir,
                mode: None,
                blob_hash: None,
                size: None,
                target: None,
            },
            ryeos_state::objects::ExternalContentManifestEntry {
                path: "bin/codex".into(),
                kind: ryeos_state::objects::ExternalContentManifestEntryKind::File,
                mode: Some(0o755),
                blob_hash: Some(lillux::sha256_hex(&runtime_bytes)),
                size: Some(runtime_bytes.len() as u64),
                target: None,
            },
        ],
        entry_count: 2,
        total_bytes: runtime_bytes.len() as u64,
    };
    expected_manifest.validate()?;
    assert_eq!(evidence.manifest_kind, expected_manifest.kind);
    assert_eq!(
        evidence.manifest_hash,
        ryeos_state::objects::canonical_value_digest(&serde_json::to_value(&expected_manifest)?)?,
    );

    // Exact point-read of the returned producer, never a thread-list scan.
    let (status, response) = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        harness.post_execute(
            "service:threads/get",
            ".",
            json!({"thread_id": producer.thread_id}),
        ),
    )
    .await
    .context("exact producer point read timed out")??;
    anyhow::ensure!(
        status.is_success(),
        "producer point read failed: {status}: {response}"
    );
    let thread = response
        .pointer("/result/thread")
        .context("producer exact thread detail")?;
    assert_eq!(thread["thread_id"], producer.thread_id);
    assert_eq!(thread["chain_root_id"], producer.chain_root_id);
    assert_eq!(thread["status"], "completed");
    assert_eq!(thread["item_ref"], retained_runtime_producer::PRODUCER_REF);
    assert_eq!(thread["requested_by"], evidence.owner_principal);
    assert_eq!(
        thread["admitted_launch_capsule_hash"],
        evidence.admitted_launch_capsule_hash
    );
    assert_eq!(
        thread["result_project_snapshot_hash"],
        evidence.result_project_snapshot_hash
    );
    let project_authority: ryeos_state::objects::ExecutionProjectAuthority =
        serde_json::from_value(thread["project_authority"].clone())?;
    match project_authority {
        ryeos_state::objects::ExecutionProjectAuthority::PinnedGeneration {
            base_snapshot_hash,
            snapshot_hash,
            ..
        } => {
            assert_eq!(
                base_snapshot_hash,
                evidence.root_producer.producer_project_snapshot_hash
            );
            assert_eq!(
                snapshot_hash,
                evidence.producer.producer_project_snapshot_hash
            );
        }
        _ => anyhow::bail!("producer did not retain pinned project authority"),
    }

    // Repeat the idempotent capture operation, not the accepted producer launch.
    let repeated = tokio::time::timeout(
        std::time::Duration::from_secs(60),
        capture_retained_runtime(&harness, &producer),
    )
    .await
    .context("repeat capture observation timed out; retain exact producer state")??;
    assert_eq!(repeated["idempotent"], true);
    for field in ["coordinate_id", "witness_hash", "evidence"] {
        assert_eq!(repeated[field], captured[field], "capture changed {field}");
    }
    assert_eq!(
        std::fs::read(project.path().join(retained_runtime_producer::INPUT_PATH))?,
        b"not the retained runtime"
    );
    harness.kill_daemon().await?;
    harness.retain_evidence_on_drop(false);
    project.disable_cleanup(false);
    Ok(())
}

async fn production_service(
    harness: &DaemonHarness,
    item_ref: &str,
    parameters: Value,
) -> anyhow::Result<Value> {
    let (status, response) = tokio::time::timeout(
        std::time::Duration::from_secs(120),
        harness.post_execute(item_ref, ".", parameters),
    )
    .await
    .with_context(|| {
        format!("{item_ref} observation timed out; retain state, do not relaunch")
    })??;
    anyhow::ensure!(
        status.is_success(),
        "{item_ref} failed: {status}: {response}"
    );
    response
        .get("result")
        .cloned()
        .context("public service result")
}

/// Exercise realization admission before managed effect lookup on an actual
/// daemon, using the existing small workspace-output producer fixture. This
/// does not repeat the completed Codex runtime producer or qualify a guest.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires current populated signed core/standard daemon test artifacts"]
async fn recorded_managed_realization_replays_after_daemon_restart() -> anyhow::Result<()> {
    use ryeos_runtime::callback_contract::{
        RuntimeDispatchEffectClass, RuntimeDispatchEvidence, RuntimeDispatchPublication,
        RuntimeDispatchSource,
    };

    let (mut harness, fixture) = DaemonHarness::start_fast_with(
        |state_path, _, fixture| {
            common::fast_fixture::register_standard_bundle(state_path, fixture)?;
            let path = state_path.join(".ai/node/policies/isolation.yaml");
            let mut document: Value = serde_yaml::from_str(
                &lillux::signature::strip_signature_lines(&std::fs::read_to_string(&path)?),
            )?;
            let mut isolation: ryeos_engine::isolation::IsolationPolicy =
                serde_json::from_value(document["policy"].clone())?;
            anyhow::ensure!(
                !isolation.trusted_process_group_sessions,
                "unexpected trusted fixture"
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
            document["policy"] = serde_json::to_value(isolation)?;
            // Disposable node setup, before bootstrap seals its signed policy.
            // No installed policy or process-scope requirement is weakened.
            std::fs::write(
                &path,
                lillux::signature::sign_content_at(
                    &serde_yaml::to_string(&document)?,
                    &fixture.node,
                    "#",
                    None,
                    common::fast_fixture::FAST_FIXTURE_TIME,
                ),
            )?;
            Ok(())
        },
        |_| {},
    )
    .await?;
    harness.retain_evidence_on_drop(true);
    let mut project = tempfile::tempdir()?;
    project.disable_cleanup(true);
    eprintln!(
        "managed replay fixture: node={}, project={}",
        harness.state_path.display(),
        project.path().display()
    );
    for (relative, source) in [
        (
            "graphs/test/two-products.yaml",
            include_str!(
                "../../../../tests/e2e/environment-products/project/.ai/graphs/test/two-products.yaml"
            ),
        ),
        (
            "graphs/test/recorded-producer.yaml",
            include_str!(
                "../../../../tests/e2e/environment-products/project/.ai/graphs/test/recorded-producer.yaml"
            ),
        ),
        (
            "config/test/two-products.yaml",
            include_str!(
                "../../../../tests/e2e/environment-products/project/.ai/config/test/two-products.yaml"
            ),
        ),
        (
            "tools/test/produce.yaml",
            include_str!(
                "../../../../tests/e2e/environment-products/project/.ai/tools/test/produce.yaml"
            ),
        ),
    ] {
        let path = project.path().join(".ai").join(relative);
        std::fs::create_dir_all(path.parent().context("fixture definition parent")?)?;
        std::fs::write(
            path,
            lillux::signature::sign_content(source, &fixture.publisher, "#", None),
        )?;
    }

    let mut prior: Option<(Value, RuntimeDispatchEvidence)> = None;
    for pass in 0..2 {
        let launch_id = [
            "L-f87889391aeb628e19d2cf0ab3444301",
            "L-f87889391aeb628e19d2cf0ab3444302",
        ][pass];
        let (status, accepted) = tokio::time::timeout(
            std::time::Duration::from_secs(60),
            harness.post_json("/execute", json!({
                "launch_id": launch_id,
                "item_ref": "graph:test/recorded-producer",
                "ref_bindings": {}, "project_path": project.path(), "parameters": {},
                "execution_policy": ExecutionPolicy::local_pinned_capture(ExecutionResponse::Accepted)
                    .exclude_operator_vault(),
            })),
        ).await.context("managed probe acceptance uncertain; retain launch id, never relaunch")??;
        anyhow::ensure!(
            status == reqwest::StatusCode::ACCEPTED,
            "managed probe refused: {status}: {accepted}"
        );
        assert_eq!(accepted["launch_id"], launch_id);
        let root = accepted["thread_id"]
            .as_str()
            .context("accepted probe root")?
            .to_owned();
        eprintln!("managed replay accepted: launch_id={launch_id}, root={root}, pass={pass}");
        let detail = tokio::time::timeout(std::time::Duration::from_secs(90), async {
            loop {
                let response =
                    production_service(&harness, "service:threads/get", json!({"thread_id":root}))
                        .await?;
                let thread = response.get("thread").context("exact probe thread")?;
                let status = thread["status"].as_str().context("probe thread status")?;
                if ryeos_state::objects::ThreadStatus::from_str_lossy(status)
                    .is_some_and(|status| status.is_terminal())
                {
                    return Ok::<_, anyhow::Error>(response);
                }
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
        })
        .await
        .context("accepted managed probe did not settle; inspect retained root, never retry")??;
        let thread = detail.get("thread").context("terminal probe thread")?;
        let receipts = production_service(
            &harness,
            "service:threads/receipts",
            json!({"thread_id":root}),
        )
        .await?;
        let chain =
            production_service(&harness, "service:threads/chain", json!({"thread_id":root}))
                .await?;
        // Test-only, read-only observation of this exact parent's operational
        // launch links. The public continuation chain is not a child-process
        // inventory: a dispatched managed child has its own chain root.
        let linked_children = {
            let runtime = rusqlite::Connection::open_with_flags(
                harness.state_path.join(".ai/state/runtime.sqlite3"),
                rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
            )?;
            let mut statement = runtime.prepare(
                "SELECT child_thread_id FROM thread_child_link WHERE parent_thread_id=?1 ORDER BY child_thread_id",
            )?;
            statement
                .query_map([&root], |row| row.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?
        };
        eprintln!(
            "managed replay evidence: {}",
            json!({
                "pass":pass, "launch_id":launch_id, "detail":detail, "receipts":receipts, "chain":chain,
                "managed_children":linked_children,
            })
        );
        assert_eq!(thread["status"], "completed");
        assert_eq!(thread["thread_id"], root);
        assert_eq!(thread["chain_root_id"], root);
        let authority: ryeos_state::objects::ExecutionProjectAuthority =
            serde_json::from_value(thread["project_authority"].clone())?;
        let ryeos_state::objects::ExecutionProjectAuthority::PinnedGeneration {
            base_snapshot_hash,
            ..
        } = authority
        else {
            anyhow::bail!("managed probe lost its pinned generation")
        };
        let result = detail
            .pointer("/result/result/result")
            .context("authored graph return")?;
        let products = ryeos_state::external_content::products::accepted_result::ProductBuildAcceptedResult::from_value(result)?;
        assert_eq!(products.producer_ref, "graph:test/two-products");
        assert_eq!(
            products
                .products
                .iter()
                .map(|product| product.product_name.as_str())
                .collect::<Vec<_>>(),
            vec!["distribution", "runtime"]
        );
        assert!(
            products
                .products
                .iter()
                .all(|product| product.qualification_hash.is_none())
        );
        let nodes = receipts["receipts"].as_array().context("probe receipts")?;
        assert_eq!(nodes.len(), 1);
        assert_eq!(nodes[0]["node"], "produce");
        assert!(nodes[0]["error"].is_null());
        let dispatch: RuntimeDispatchEvidence =
            serde_json::from_value(nodes[0]["dispatch"].clone())?;
        dispatch.validate()?;
        assert_eq!(dispatch.effect_class, RuntimeDispatchEffectClass::Recorded);
        assert!(dispatch.effect_identity.is_some());
        assert!(dispatch.record_hash.is_some());
        if let Some((first_detail, first_dispatch)) = &prior {
            assert_eq!(dispatch.source, RuntimeDispatchSource::EffectRecord);
            assert_eq!(
                dispatch.publication,
                RuntimeDispatchPublication::NotApplicable
            );
            assert_eq!(dispatch.action_digest, first_dispatch.action_digest);
            assert_eq!(dispatch.effect_identity, first_dispatch.effect_identity);
            assert_eq!(dispatch.record_hash, first_dispatch.record_hash);
            assert_eq!(dispatch.replayed_from, first_dispatch.record_hash);
            assert_eq!(
                result,
                first_detail
                    .pointer("/result/result/result")
                    .context("original graph return")?
            );
            let first_authority: ryeos_state::objects::ExecutionProjectAuthority =
                serde_json::from_value(first_detail["thread"]["project_authority"].clone())?;
            let ryeos_state::objects::ExecutionProjectAuthority::PinnedGeneration {
                base_snapshot_hash: first_base,
                ..
            } = first_authority
            else {
                anyhow::bail!("original probe lost its pinned generation")
            };
            assert_eq!(base_snapshot_hash, first_base);
            assert!(
                linked_children.is_empty(),
                "effect replay launched a managed child"
            );
            // No continuation is expected either. This is separate from the
            // exact-parent launch-link assertion above.
            assert!(
                chain["edges"]
                    .as_array()
                    .context("replay chain edges")?
                    .is_empty()
            );
            let threads = chain["threads"]
                .as_array()
                .context("replay chain threads")?;
            assert_eq!(threads.len(), 1);
            assert_eq!(threads[0]["thread_id"], root);
        } else {
            assert_eq!(dispatch.source, RuntimeDispatchSource::Executed);
            assert_eq!(dispatch.publication, RuntimeDispatchPublication::Inserted);
            assert_eq!(dispatch.replayed_from, None);
            assert_eq!(
                linked_children.len(),
                1,
                "first action must execute its managed producer"
            );
            let child = production_service(
                &harness,
                "service:threads/get",
                json!({"thread_id":linked_children[0]}),
            )
            .await?;
            assert_eq!(child["thread"]["status"], "completed");
            assert_eq!(child["thread"]["item_ref"], "graph:test/two-products");
            eprintln!("managed producer child evidence: {child}");
            prior = Some((detail, dispatch));
            // Restart only the disposable process owned by this test. Keep all
            // node state and the unchanged source; this is a new equivalent
            // action after confirmed completion, not retry of uncertain work.
            harness.kill_daemon().await?;
            harness.respawn_with(|_| {}).await?;
        }
    }
    harness.kill_daemon().await?;
    harness.retain_evidence_on_drop(false);
    project.disable_cleanup(false);
    Ok(())
}

/// Real workspace-output production of the published Codex bytes. The input
/// is large-content, not a large file smuggled through retained_project/small
/// CAS. This stops at product capture: no synthetic qualification, worker
/// launch, model invocation, network activation, or publication is included.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires exact predownloaded Codex, static producer artifact/hash and current populated bundles"]
async fn public_codex_runtime_production_captures_real_workspace_output() -> anyhow::Result<()> {
    use ryeos_state::external_content::products::{ProductSource, ProductStorage};
    use ryeos_state::objects::{
        ExternalContentManifestEntryKind, ExternalLargeContentManifestEntry,
        ExternalLargeContentManifestObject,
    };

    let input_path =
        PathBuf::from(std::env::var_os("RYEOS_TEST_PINNED_CODEX").context(
            "set RYEOS_TEST_PINNED_CODEX to the exact already-downloaded Codex executable",
        )?);
    let producer_path = PathBuf::from(
        std::env::var_os("RYEOS_TEST_CODEX_GUEST_RUNTIME_PRODUCER").context(
            "set RYEOS_TEST_CODEX_GUEST_RUNTIME_PRODUCER to the exact prebuilt static producer",
        )?,
    );
    let producer_sha256 = std::env::var("RYEOS_TEST_CODEX_GUEST_RUNTIME_PRODUCER_SHA256")
        .context("set the expected producer artifact SHA256 explicitly")?;
    let mut inputs = None;
    let (mut harness, fixture) = DaemonHarness::start_fast_with(
        |state_path, _, fixture| {
            common::fast_fixture::register_standard_bundle(state_path, fixture)?;
            inputs = Some(codex_runtime_producer::prepare(
                &common::workspace_root(),
                state_path,
                fixture,
                &input_path,
                &producer_path,
                &producer_sha256,
            )?);
            Ok(())
        },
        |_| {},
    )
    .await?;
    let inputs = inputs.context("prepared exact production inputs")?;
    harness.retain_evidence_on_drop(true);
    let mut project = tempfile::tempdir()?;
    project.disable_cleanup(true);
    std::fs::create_dir(project.path().join(".ai"))?;
    // This exact caller coordinate survives an ambiguous HTTP acknowledgement.
    // There is one launch attempt; inspection never recovers IDs by list scan.
    let launch_id = "L-4bd4df404846f1d4a89ccab141c58d80";
    eprintln!(
        "Codex production node={}, project={}, launch_id={launch_id}",
        harness.state_path.display(),
        project.path().display()
    );

    let imported = production_service(
        &harness,
        "service:external-content/import",
        json!({
            "source": "filesystem", "root": codex_runtime_producer::IMPORT_ROOT,
            "path": "codex", "shape": "file", "storage": "large_content",
            "maximum_bytes": inputs.maximum_bytes, "expected_file_sha256": inputs.file_sha256,
        }),
    )
    .await?;
    assert_eq!(imported["manifest_hash"], inputs.input_manifest_hash);
    assert_eq!(
        imported["manifest_kind"],
        ryeos_state::objects::EXTERNAL_LARGE_CONTENT_MANIFEST_KIND
    );
    assert_eq!(imported["entry_count"], 1);
    assert_eq!(imported["total_bytes"], inputs.total_bytes);
    let bound = production_service(&harness, "service:external-content/bind", json!({
        "staging_id": imported["staging_id"], "request_digest": imported["request_digest"],
        "manifest_hash": imported["manifest_hash"],
        "consumer_ref": codex_runtime_producer::CONSUMER_REF, "consumer_kind": "installed_bundle",
    })).await?;
    assert_eq!(bound["manifest_hash"], inputs.input_manifest_hash);
    assert_eq!(bound["consumer_ref"], codex_runtime_producer::CONSUMER_REF);
    assert_eq!(bound["publisher_fingerprint"], fixture.publisher_fp());
    eprintln!("Codex runtime input binding: {bound}");

    let (status, accepted) = tokio::time::timeout(
        std::time::Duration::from_secs(90),
        harness.post_json("/execute/launch", json!({
            "item_ref": codex_runtime_producer::PRODUCER_REF, "launch_id": launch_id,
            "ref_bindings": {}, "project_path": project.path(), "parameters": {},
            "execution_policy": ExecutionPolicy::local_pinned_capture(ExecutionResponse::Accepted)
                .exclude_operator_vault(),
        })),
    ).await.context("production acceptance timed out; inspect the retained launch_id, do not retry")??;
    anyhow::ensure!(
        status == reqwest::StatusCode::ACCEPTED,
        "producer was not accepted: {status}: {accepted}"
    );
    assert_eq!(accepted["status"], "accepted");
    assert_eq!(accepted["launch_id"], launch_id);
    let root_id = accepted["thread_id"]
        .as_str()
        .context("durable accepted root")?
        .to_owned();
    ryeos_runtime::validate_runtime_thread_id(&root_id).map_err(anyhow::Error::msg)?;
    eprintln!("Codex runtime producer accepted: launch_id={launch_id}, root={root_id}");

    let thread = tokio::time::timeout(std::time::Duration::from_secs(420), async {
        loop {
            let result = production_service(
                &harness,
                "service:threads/get",
                json!({"thread_id": root_id}),
            )
            .await?;
            let thread = result.get("thread").context("exact producer point read")?;
            let status = thread["status"].as_str().context("producer status")?;
            if ryeos_state::objects::ThreadStatus::from_str_lossy(status)
                .is_some_and(|status| status.is_terminal())
            {
                return Ok::<_, anyhow::Error>(thread.clone());
            }
            tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        }
    })
    .await
    .context("accepted producer observation expired; retain root and launch_id without retry")??;
    let receipts = production_service(
        &harness,
        "service:threads/receipts",
        json!({"thread_id": root_id}),
    )
    .await?;
    // Keep the exact first failing operation, not only the terminal status.
    // This credential-free fixture's public receipt contains no secret inputs.
    eprintln!("Codex runtime producer terminal receipts: {receipts}");
    anyhow::ensure!(thread["status"] == "completed", "producer failed: {thread}");
    assert_eq!(thread["thread_id"], root_id);
    assert_eq!(thread["chain_root_id"], root_id);
    assert_eq!(thread["item_ref"], codex_runtime_producer::PRODUCER_REF);
    let capture_parameters = json!({
        "chain_root_id": root_id, "thread_id": root_id,
        "recipe_binding": "product_recipe", "product_name": "runtime",
    });
    let captured = production_service(
        &harness,
        "service:external-content/capture-product",
        capture_parameters.clone(),
    )
    .await?;
    anyhow::ensure!(
        captured["state"] == "captured",
        "product capture failed: {captured}"
    );
    assert_eq!(captured["idempotent"], false);
    let evidence: ProductCaptureEvidence = serde_json::from_value(captured["evidence"].clone())?;
    evidence.validate()?;
    assert_eq!(evidence.chain_root_id, root_id);
    assert_eq!(captured["coordinate_id"],
        ryeos_state::external_content::products::publication::ProductCaptureCoordinate::from_evidence(
            &evidence,
        )?.coordinate_id()?);
    anyhow::ensure!(
        captured["witness_hash"]
            .as_str()
            .is_some_and(lillux::valid_hash),
        "capture did not return a canonical retained witness hash"
    );
    assert_eq!(evidence.thread_id, root_id);
    assert_eq!(
        evidence.owner_principal,
        format!("fp:{}", fixture.user_fp())
    );
    assert_eq!(evidence.producer, evidence.root_producer);
    assert_eq!(
        evidence.producer.canonical_ref,
        codex_runtime_producer::PRODUCER_REF
    );
    assert_eq!(evidence.recipe_ref, codex_runtime_producer::RECIPE_REF);
    assert_eq!(evidence.recipe_binding, "product_recipe");
    assert_eq!(evidence.declaration.name, "runtime");
    assert_eq!(evidence.declaration.path, "products/codex-guest-runtime");
    assert_eq!(evidence.declaration.storage, ProductStorage::LargeContent);
    assert_eq!(
        evidence.declaration.source,
        ProductSource::WorkspaceOutput {
            root: "runtime".into()
        }
    );
    anyhow::ensure!(
        evidence
            .workspace_output_capture_hash
            .as_deref()
            .is_some_and(lillux::valid_hash),
        "real workspace-output capture missing"
    );
    anyhow::ensure!(
        evidence
            .producer_partition_identity
            .as_deref()
            .is_some_and(lillux::valid_hash),
        "real producer partition identity missing"
    );
    assert_eq!(evidence.total_bytes, inputs.total_bytes);
    assert_eq!(evidence.entry_count, 2);
    assert_eq!(
        thread["admitted_launch_capsule_hash"],
        evidence.admitted_launch_capsule_hash
    );
    assert_eq!(
        thread["result_project_snapshot_hash"],
        evidence.result_project_snapshot_hash
    );
    let authority: ryeos_state::objects::ExecutionProjectAuthority =
        serde_json::from_value(thread["project_authority"].clone())?;
    match authority {
        ryeos_state::objects::ExecutionProjectAuthority::PinnedGeneration {
            base_snapshot_hash,
            snapshot_hash,
            ..
        } => {
            assert_eq!(
                base_snapshot_hash,
                evidence.root_producer.producer_project_snapshot_hash
            );
            assert_eq!(
                snapshot_hash,
                evidence.producer.producer_project_snapshot_hash
            );
        }
        _ => anyhow::bail!("production did not retain pinned project authority"),
    }

    // Read-only assertion against the exact manifest admitted by public import.
    // This creates no CAS object and manufactures no producer testimony.
    let cas = lillux::CasStore::new(harness.state_path.join(".ai/state/objects"));
    let imported_manifest =
        ryeos_state::objects::load_if_large_content_manifest(&cas, &inputs.input_manifest_hash)?
            .context("public import's retained manifest")?;
    assert_eq!(imported_manifest.entries.len(), 1);
    let mut executable = imported_manifest.entries[0].clone();
    assert_eq!(executable.path, "content");
    assert_eq!(
        executable.file_sha256.as_deref(),
        Some(inputs.file_sha256.as_str())
    );
    assert_eq!(executable.mode, Some(0o755));
    executable.path = "bin/codex".into();
    let expected = ExternalLargeContentManifestObject {
        schema: ryeos_state::objects::EXTERNAL_LARGE_CONTENT_SCHEMA.into(),
        kind: ryeos_state::objects::EXTERNAL_LARGE_CONTENT_MANIFEST_KIND.into(),
        entries: vec![
            ExternalLargeContentManifestEntry {
                path: "bin".into(),
                kind: ExternalContentManifestEntryKind::Dir,
                mode: None,
                blob_hash: None,
                file_sha256: None,
                size: None,
                chunk_size: None,
                chunk_hashes: vec![],
                target: None,
            },
            executable,
        ],
        entry_count: 2,
        total_bytes: inputs.total_bytes,
    };
    expected.validate()?;
    assert_eq!(evidence.manifest_kind, expected.kind);
    assert_eq!(
        evidence.manifest_hash,
        ryeos_state::objects::canonical_value_digest(&expected.to_value()?)?
    );
    let retained =
        ryeos_state::objects::load_if_large_content_manifest(&cas, &evidence.manifest_hash)?
            .context("captured runtime manifest")?;
    assert_eq!(retained, expected);
    assert!(
        !project.path().join("products").exists(),
        "producer wrote outside its private generation"
    );

    let repeated = production_service(
        &harness,
        "service:external-content/capture-product",
        capture_parameters,
    )
    .await?;
    assert_eq!(repeated["idempotent"], true);
    for field in ["coordinate_id", "witness_hash", "evidence"] {
        assert_eq!(repeated[field], captured[field]);
    }
    let nodes = receipts["receipts"]
        .as_array()
        .context("producer graph node receipts")?;
    assert_eq!(receipts["thread_id"], root_id);
    assert_eq!(
        nodes.len(),
        1,
        "only the producer action emits a receipt; return completion is proved by the terminal root"
    );
    let produce = nodes
        .iter()
        .filter(|node| node["node"] == "produce")
        .collect::<Vec<_>>();
    assert_eq!(
        produce.len(),
        1,
        "producer must execute exactly one admitted action"
    );
    assert!(nodes.iter().all(|node| node["error"].is_null()));
    let dispatch: ryeos_runtime::callback_contract::RuntimeDispatchEvidence =
        serde_json::from_value(produce[0]["dispatch"].clone())?;
    dispatch.validate()?;
    assert_eq!(
        dispatch.source,
        ryeos_runtime::callback_contract::RuntimeDispatchSource::Executed
    );
    assert_eq!(
        dispatch.effect_class,
        ryeos_runtime::callback_contract::RuntimeDispatchEffectClass::Live
    );
    assert_eq!(
        dispatch.publication,
        ryeos_runtime::callback_contract::RuntimeDispatchPublication::NotApplicable
    );
    assert_eq!(dispatch.effect_identity, None);
    assert_eq!(dispatch.record_hash, None);
    eprintln!(
        "Codex runtime production/capture evidence: {}",
        json!({
            "launch_id": launch_id, "thread": thread, "capture": captured, "receipts": receipts,
            "input_binding_hash": bound["binding_hash"], "producer_sha256": inputs.producer_sha256,
            "executed_actions": 1, "effect_record_actions": 0,
            "provider_model_invocation_requested": false, "producer_network_policy": "isolated",
            "qualification_claimed": false, "worker_started": false,
        })
    );
    harness.kill_daemon().await?;
    harness.retain_evidence_on_drop(false);
    project.disable_cleanup(false);
    Ok(())
}

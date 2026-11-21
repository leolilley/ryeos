//! A real `ryeosd` started over what a daemon that died mid-lease leaves
//! behind: a dedicated session whose external allocation contacted its
//! provider and bound an occurrence, with no observed provider terminal.
//!
//! The node is a fast fixture daemon. While it is stopped the test seeds the
//! predecessor's rows through the node Store (a running placement thread whose
//! launch claim belongs to another daemon generation, the session holding its
//! credential profile lock, and the quarantine-bound allocation). The retained
//! binding names a backend no adapter provides, so the provider can never
//! answer: this is the "provider has not resolved" half of startup.
//!
//! The second half of this file seeds the same predecessor over a real
//! installed placement binding whose backend is the real synthetic lifecycle
//! adapter, so the provider answers as the test scripts it (terminated,
//! pending, not found, with a clock offset that moves the adapter's time). No
//! process scope and no placed session is involved. These cases prove the
//! daemon half of lease adapter protocol v4: the lease end it derives and
//! states, the v4 wire, the stored terminal basis, the settle and the skip
//! guards. The render and modal adapters' rule (b) decision stays proven by
//! their unit tests; nothing here proves those adapters apply it.
//!
//! Cases of the first half:
//!
//! 1. Two starts. Start 1 reaches ready with no operator action, drives the
//!    allocation toward its provider terminal, leaves it unsettled, and logs the
//!    predecessor-lease counts line. The daemon dies hard again. Start 2 over
//!    the same rows also reaches ready: the wedge that took two starts to show.
//! 2. The runtime database still refuses a local settlement of the session while
//!    its allocation is unsettled, after the restarts, and the refusal changes
//!    nothing.

#![cfg(unix)]

use ryeosd::test_support as common;

use anyhow::{Context, Result};
use common::DaemonHarness;
use ryeos_app::runtime_db::external_execution::ExternalAllocationPhase;
use serde_json::json;

#[path = "../../../../tests/e2e/external-execution/support/signed_bundle.rs"]
mod signed_bundle;

const PLACEMENT: &str = "T-leased-startup-e2e";
const WORKSPACE: &str = "W-leased-startup-e2e";
const WORKER: &str = "worker-leased-startup-e2e";
const COUNTS_LINE: &str = "predecessor external leases and session allocations driven toward";

fn strip_ansi_escapes(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            if chars.next() == Some('[') {
                for c in chars.by_ref() {
                    if ('@'..='~').contains(&c) {
                        break;
                    }
                }
            }
            continue;
        }
        out.push(c);
    }
    out
}

/// A fast fixture node that has completed one start, killed hard, with the
/// predecessor's session and unsettled allocation left in its Store.
async fn node_killed_mid_lease() -> Result<DaemonHarness> {
    let (mut harness, _fixture) = DaemonHarness::start_fast_core_only_prepared()
        .await
        .context("start fast daemon")?;
    harness
        .kill_daemon()
        .await
        .context("kill the daemon hard")?;
    {
        let store =
            common::open_stopped_node_store(&harness.state_path).context("open stopped store")?;
        store
            .install_unsettled_dedicated_session_allocation(PLACEMENT, WORKSPACE, WORKER)
            .context("seed the predecessor's session and allocation")?;
    }
    Ok(harness)
}

/// The log of the current start with terminal colouring removed, or a failure
/// naming the whole log.
fn require_counted_start(harness: &DaemonHarness, label: &str) -> Result<String> {
    let log = strip_ansi_escapes(&harness.stderr_so_far());
    anyhow::ensure!(
        !log.contains("daemon startup failed") && !log.contains("restart inhibited"),
        "{label} failed to start:\n{log}"
    );
    let counts = log
        .lines()
        .find(|line| line.contains(COUNTS_LINE))
        .with_context(|| format!("{label} logged no predecessor-lease counts line:\n{log}"))?;
    anyhow::ensure!(
        counts.contains("discovered=1") && counts.contains("terminated=0"),
        "{label} counted the lease wrongly: {counts}\n{log}"
    );
    Ok(log)
}

/// What the stopped node's Store holds for the seeded session.
struct Held {
    phase: ExternalAllocationPhase,
    session_state: String,
    lock_owner: Option<String>,
}

fn held_after_stop(harness: &DaemonHarness) -> Result<Held> {
    let store =
        common::open_stopped_node_store(&harness.state_path).context("open stopped store")?;
    let allocation = store
        .external_allocation(PLACEMENT)?
        .context("the allocation was lost")?;
    let session = store
        .dedicated_session(PLACEMENT)?
        .context("the session was lost")?;
    let profile = store
        .credential_profile(&format!("P-{PLACEMENT}"))?
        .context("the credential profile was lost")?;
    Ok(Held {
        phase: allocation.phase,
        session_state: session.state,
        lock_owner: profile.lock_owner,
    })
}

#[tokio::test(flavor = "multi_thread")]
async fn a_daemon_killed_mid_lease_restarts_twice_without_operator_action() -> Result<()> {
    let mut harness = node_killed_mid_lease().await?;

    // Start 1: ready without operator action.
    harness
        .respawn_with(|_| {})
        .await
        .context("start 1 over the unsettled allocation")?;
    let first = require_counted_start(&harness, "start 1")?;
    assert!(
        first.contains("cleared launch claim left by a dead daemon generation"),
        "start 1 did not clear the dead generation's launch claim:\n{first}"
    );

    // The daemon dies hard again, as the derived-image node did.
    harness.kill_daemon().await.context("kill start 1 hard")?;

    // Start 2: also ready, over the same unsettled allocation.
    harness
        .respawn_with(|_| {})
        .await
        .context("start 2 over the unsettled allocation")?;
    require_counted_start(&harness, "start 2")?;

    harness.stop_gracefully().await.context("stop start 2")?;
    let held = held_after_stop(&harness)?;
    assert_eq!(
        held.phase,
        ExternalAllocationPhase::Quarantined,
        "the allocation must stay quarantined without an observed provider terminal"
    );
    assert_ne!(
        held.session_state, "terminal",
        "the session settled while its allocation was unsettled"
    );
    assert_eq!(
        held.lock_owner.as_deref(),
        Some(WORKER),
        "the session's credential lock was released while its allocation was unsettled"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn local_settlement_of_a_session_with_an_unsettled_allocation_stays_refused() -> Result<()> {
    let mut harness = node_killed_mid_lease().await?;
    harness
        .respawn_with(|_| {})
        .await
        .context("start over the unsettled allocation")?;
    harness.stop_gracefully().await.context("stop the node")?;

    let before = held_after_stop(&harness)?;
    {
        let store = common::open_stopped_node_store(&harness.state_path)?;
        let refusal = store
            .fail_dedicated_session_start(PLACEMENT, WORKER, 1, "ended host lifetime", true)
            .expect_err("local settlement must be refused while the allocation is unsettled");
        eprintln!("local settlement refusal: {refusal:#}");
    }
    let after = held_after_stop(&harness)?;
    assert_eq!(after.phase, before.phase);
    assert_eq!(after.session_state, before.session_state);
    assert_ne!(after.session_state, "terminal");
    assert_eq!(after.lock_owner.as_deref(), Some(WORKER));
    Ok(())
}

// ---------------------------------------------------------------------------
// The same predecessor over the real synthetic lifecycle adapter.
// ---------------------------------------------------------------------------

mod real_adapter {
    use std::path::{Path, PathBuf};

    use anyhow::{Context, Result, ensure};
    use ryeos_app::leased_session_rig::{
        LeasedSessionSeed, install_leased_session, leased_session_program,
    };
    use ryeos_app::node_config::sections::external_execution::{
        InstalledExternalExecutionBinding, LeasedSessionRigArtifacts,
        leased_session_rig_binding_body, provision_installed_test_placement_credential,
    };
    use ryeos_state::external_execution::transport::{
        EXTERNAL_CHANNEL_ROUTE_CONTRACT, ExternalControllerTransportContract,
        ExternalNetworkInputPolicy, ExternalNetworkInputSelection, external_tls_root_bundle_digest,
    };
    use serde_json::{Value, json};

    use super::*;

    pub const PLACEMENT: &str = super::PLACEMENT;
    pub const WORKSPACE: &str = super::WORKSPACE;
    pub const WORKER: &str = super::WORKER;
    const BINDING_ID: &str = "leased-session-rig";
    const CREDENTIAL: &str = "leased-session-rig-credential";
    /// The reservation's own lease lifetime and the binding's timeout.
    const LEASE_SECONDS: u32 = 60;
    /// The lease-end grace the adapters apply (ten minutes), as a test-binary
    /// constant. No daemon code reads it; only the adapter decides with it.
    pub const GRACE_MS: i64 = 10 * 60 * 1000;

    /// Where the built synthetic adapter binaries are: the directory named by
    /// `RYEOS_TEST_SYNTHETIC_BIN_DIR`, else the cargo target directory this
    /// test binary sits in (a `cargo build -p
    /// ryeos-synthetic-external-lifecycle-adapter` puts them there).
    fn binary_directory() -> Result<PathBuf> {
        if let Some(directory) = std::env::var_os("RYEOS_TEST_SYNTHETIC_BIN_DIR") {
            return Ok(PathBuf::from(directory));
        }
        let executable = std::env::current_exe()?;
        Ok(executable
            .parent()
            .and_then(Path::parent)
            .context("test executable has no target directory")?
            .to_path_buf())
    }

    fn artifact(directory: &Path, name: &str) -> Result<PathBuf> {
        let path = directory.join(name);
        ensure!(
            path.is_file(),
            "synthetic binary {} is not built; build -p ryeos-synthetic-external-lifecycle-adapter",
            path.display()
        );
        Ok(path)
    }

    fn identity(path: &Path) -> Result<(String, u64)> {
        let bytes = std::fs::read(path).with_context(|| format!("read {}", path.display()))?;
        Ok((lillux::sha256_hex(&bytes), bytes.len() as u64))
    }

    /// The scripted provider's answer and clock.
    #[derive(Clone, Copy)]
    pub struct Provider {
        pub answer: &'static str,
        pub terminate_accepted: bool,
        pub clock_offset_ms: i64,
    }

    pub struct Rig {
        pub harness: DaemonHarness,
        fixture: common::fast_fixture::FastFixture,
        provider_root: PathBuf,
        // Keeps the provider's private root alive for the whole rig.
        _provider: tempfile::TempDir,
        bundle: PathBuf,
        trust: ryeos_engine::trust::TrustStore,
    }

    impl Rig {
        /// A core-only node with the signed synthetic adapter bundle
        /// installed, started once and killed hard, so its stores exist and
        /// the predecessor's rows can be seeded.
        pub async fn start() -> Result<Self> {
            let directory = binary_directory()?;
            let adapter = artifact(&directory, "ryeos-synthetic-external-lifecycle-adapter")?;
            let supervisor = artifact(&directory, "ryeos-synthetic-external-candidate-supervisor")?;
            let launcher = artifact(&directory, "ryeos-synthetic-external-candidate-launcher")?;
            let connector = artifact(&directory, "ryeos-synthetic-external-candidate-connector")?;
            let configuration =
                artifact(&directory, "ryeos-synthetic-codex-external-configuration")?;
            let artifacts = signed_bundle::SyntheticExternalArtifacts {
                adapter: &adapter,
                supervisor: &supervisor,
                launcher: &launcher,
                connector: &connector,
                configuration: &configuration,
            };

            let provider = tempfile::tempdir()?;
            let parent = lillux::PinnedDirectory::open(provider.path())?
                .context("provider parent is absent")?;
            parent
                .create_child(std::ffi::OsStr::new("provider"), 0o700)?
                .require_owner_private_directory()?;
            let provider_root = provider.path().join("provider");

            let mut installed = None;
            let (mut harness, fixture) =
                DaemonHarness::start_fast_core_only_with(|state_path, _user_space, fixture| {
                    let (bundle, trust, _) =
                        signed_bundle::install_signed_test_bundle_with_guest_admission(
                            state_path,
                            true,
                            false,
                            &fixture.publisher,
                            &artifacts,
                        );
                    common::fast_fixture::register_presigned_fixture_bundle(
                        state_path,
                        "synthetic-external",
                        &bundle,
                        fixture,
                    )?;
                    installed = Some((bundle, trust));
                    Ok(())
                })
                .await
                .context("start the core-only node with the synthetic adapter bundle")?;
            let (bundle, trust) = installed.context("the adapter bundle was not installed")?;
            harness
                .kill_daemon()
                .await
                .context("kill the first start")?;
            let rig = Self {
                harness,
                fixture,
                provider_root,
                _provider: provider,
                bundle,
                trust,
            };
            rig.install_binding(&directory)?;
            Ok(rig)
        }

        /// Sign the placement binding for the fixture program as the node,
        /// as an operator's composed binding is, and provision its credential.
        fn install_binding(&self, directory: &Path) -> Result<()> {
            let state = &self.harness.state_path;
            let (backend_hash, backend_bytes) =
                identity(&directory.join("ryeos-synthetic-external-lifecycle-adapter"))?;
            let (supervisor_hash, supervisor_bytes) =
                identity(&directory.join("ryeos-synthetic-external-candidate-supervisor"))?;
            let (launcher_hash, launcher_bytes) =
                identity(&directory.join("ryeos-synthetic-external-candidate-launcher"))?;
            let (configuration_hash, configuration_bytes) =
                identity(&directory.join("ryeos-synthetic-codex-external-configuration"))?;
            let (connector_hash, connector_bytes) =
                identity(&directory.join("ryeos-synthetic-external-candidate-connector"))?;
            let guest_runtime_manifest_hash =
                ryeos_engine::registry::binary_resolver::verify_bundle_executor_manifest_ref_identity(
                    &self.bundle,
                    &self.trust,
                )?
                .context("signed synthetic executor manifest is absent")?
                .manifest_hash;
            let roots = vec![
                ryeos_external_candidate_supervisor::test_support::TEST_CA_DER_BASE64.to_owned(),
            ];
            let network_inputs = ExternalNetworkInputPolicy {
                resolver: ExternalNetworkInputSelection {
                    source: "/etc/resolv.conf".into(),
                    max_bytes: 65536,
                    required: true,
                },
                hosts: ExternalNetworkInputSelection {
                    source: "/etc/hosts".into(),
                    max_bytes: 65536,
                    required: true,
                },
            };
            let transport = ExternalControllerTransportContract {
                schema: 3,
                https_origin: "https://localhost:7443".into(),
                route_contract: EXTERNAL_CHANNEL_ROUTE_CONTRACT.into(),
                tls_root_bundle_digest: external_tls_root_bundle_digest(&roots)?,
                connect_timeout_ms: 1000,
                request_timeout_ms: 5000,
                maximum_response_bytes: 2 * 1024 * 1024,
                max_clock_skew_ms: None,
                network_inputs,
            };
            let settings = json!({
                "schema": 1,
                "state_root": self.provider_root,
                "expected_credential_sha256": lillux::sha256_hex(CREDENTIAL.as_bytes()),
                "maximum_copy_entries": 4096,
                "maximum_copy_depth": 32,
                "startup_timeout_ms": 30_000,
            });
            let body = leased_session_rig_binding_body(
                &leased_session_program()?,
                transport,
                roots,
                LeasedSessionRigArtifacts {
                    backend: "synthetic-local".into(),
                    backend_artifact_hash: backend_hash,
                    backend_artifact_bytes: backend_bytes,
                    settings_schema_digest: "9".repeat(64),
                    supervisor_artifact_hash: supervisor_hash,
                    supervisor_artifact_bytes: supervisor_bytes,
                    launcher_artifact_hash: launcher_hash,
                    launcher_artifact_bytes: launcher_bytes,
                    configuration_adapter_artifact_hash: configuration_hash,
                    configuration_adapter_artifact_bytes: configuration_bytes,
                    connector_artifact_hash: connector_hash,
                    connector_artifact_bytes: connector_bytes,
                    guest_runtime_manifest_hash,
                },
                settings,
                LEASE_SECONDS,
            )?;
            let path = state.join(format!(".ai/node/external_execution/{BINDING_ID}.yaml"));
            std::fs::create_dir_all(path.parent().context("binding parent")?)?;
            std::fs::write(
                &path,
                lillux::signature::sign_content_at(
                    &body,
                    &self.fixture.node,
                    "#",
                    None,
                    common::fast_fixture::FAST_FIXTURE_TIME,
                ),
            )?;
            let installed = self.installed_binding()?;
            provision_installed_test_placement_credential(state, &installed, CREDENTIAL)
        }

        /// The binding the restarted node will load, read the way it reads it.
        pub fn installed_binding(&self) -> Result<InstalledExternalExecutionBinding> {
            let state = self.harness.state_path.as_path();
            let trust = ryeos_engine::trust::TrustStore::load_node_registry(state)?;
            let loader = ryeos_node_runtime::bundle_bootstrap::BundleBootstrapLoader {
                app_root: state,
                trust_store: &trust,
            };
            let table = ryeos_node_runtime::node_policy::NodePolicyTable::new();
            let policy = ryeos_node_runtime::node_policy::load_snapshot(state, &trust, &table)?;
            let mut config = ryeos_app::node_config::loader::NodeConfigLoader {
                app_root: state,
                trust_store: &trust,
            }
            .load_full(
                &ryeos_app::node_config::NodeConfigTable::new(),
                &loader.load_bundle_section()?,
                policy.require::<ryeos_node_runtime::node_policy::sections::command_registration::CommandRegistrationAuthority>()?,
                &table,
            )?;
            ensure!(
                config.external_execution.len() == 1,
                "expected exactly the rig's installed binding"
            );
            Ok(config.external_execution.remove(0))
        }

        /// Seed the predecessor's rows. `lease_admitted` false leaves the
        /// reservation without an admitted lease request, so its lease end is
        /// unbounded.
        pub fn seed(&self, lease_admitted: bool) -> Result<()> {
            let installed = self.installed_binding()?;
            let store = common::open_stopped_node_store(&self.harness.state_path)?;
            install_leased_session(
                &store,
                &self.harness.state_path,
                &installed,
                &LeasedSessionSeed {
                    placement_thread_id: PLACEMENT,
                    workspace_id: WORKSPACE,
                    worker_instance_id: WORKER,
                    contact_deadline_ms: lillux::time::timestamp_millis() + 60_000,
                    timeout_seconds: LEASE_SECONDS,
                    lease_admitted,
                },
            )
        }

        /// Script what the provider answers and what time its adapter keeps.
        pub fn script(&self, provider: Provider) -> Result<()> {
            let document: Value = json!({
                "schema": 1,
                "answer": provider.answer,
                "terminate_accepted": provider.terminate_accepted,
                "clock_offset_ms": provider.clock_offset_ms,
            });
            let path = self.provider_root.join("provider-switch.json");
            let _ = std::fs::remove_file(&path);
            std::fs::write(&path, serde_json::to_vec(&document)?)?;
            Ok(())
        }

        pub async fn restart(&mut self) -> Result<()> {
            self.harness
                .respawn_with(|_| {})
                .await
                .context("start over the seeded predecessor")
        }

        /// What the stopped node's Store holds.
        pub fn held(&self) -> Result<Held> {
            held_after_stop(&self.harness)
        }

        /// The terminal observations the settlement stored.
        pub fn stored_observations(&self) -> Result<Vec<Value>> {
            let store = common::open_stopped_node_store(&self.harness.state_path)?;
            store.external_terminal_observation_documents()
        }

        /// Take the seeded session's credential for a new worker of the same
        /// owner, on the stopped node's Store. Succeeds only when no earlier
        /// session still holds the credential lock.
        pub fn acquire_credential(&self, new_worker: &str) -> Result<()> {
            let store = common::open_stopped_node_store(&self.harness.state_path)?;
            store.acquire_credential_profile(
                &format!("P-{PLACEMENT}"),
                "fp:operator",
                new_worker,
            )?;
            Ok(())
        }

        pub fn log(&self) -> String {
            strip_ansi_escapes(&self.harness.stderr_so_far())
        }
    }

    /// The counts line of the start, with its failure text if the node did
    /// not start.
    pub fn counts_line(log: &str) -> Result<String> {
        anyhow::ensure!(
            !log.contains("daemon startup failed") && !log.contains("restart inhibited"),
            "the node failed to start:\n{log}"
        );
        log.lines()
            .find(|line| line.contains(COUNTS_LINE))
            .map(str::to_owned)
            .with_context(|| format!("no predecessor-lease counts line:\n{log}"))
    }
}

use real_adapter::{GRACE_MS, Provider, Rig};

fn scripted(answer: &'static str, terminate_accepted: bool, clock_offset_ms: i64) -> Provider {
    Provider {
        answer,
        terminate_accepted,
        clock_offset_ms,
    }
}

/// Start over a predecessor whose provider answers as `provider`, stop
/// gracefully, and return the rig, the start's log and the Store's state.
async fn restarted_over(
    lease_admitted: bool,
    provider: Provider,
) -> Result<(Rig, String, Held, Vec<serde_json::Value>)> {
    let mut rig = Rig::start().await?;
    rig.seed(lease_admitted)?;
    rig.script(provider)?;
    rig.restart().await?;
    let log = rig.log();
    real_adapter::counts_line(&log)?;
    report_lease_lines(&log);
    rig.harness
        .stop_gracefully()
        .await
        .context("stop the node")?;
    let held = rig.held()?;
    let observations = rig.stored_observations()?;
    Ok((rig, log, held, observations))
}

/// Print the start's lines about leases, so a run's log shows what the node
/// decided.
fn report_lease_lines(log: &str) {
    for line in log.lines().filter(|line| {
        line.contains("lease") || line.contains("external") || line.contains("predecessor")
    }) {
        eprintln!("node: {line}");
    }
}

fn require_unsettled(held: &Held, log: &str) {
    assert_eq!(
        held.phase,
        ExternalAllocationPhase::Quarantined,
        "the allocation settled without an observed provider terminal:\n{log}"
    );
    assert_ne!(
        held.session_state, "terminal",
        "the session settled while its allocation was unsettled"
    );
    assert_eq!(
        held.lock_owner.as_deref(),
        Some(real_adapter::WORKER),
        "the session's credential lock was released while its allocation was unsettled"
    );
}

/// After a provider terminal the predecessor's session is settled as an
/// interrupted, recoverable session: it holds no worker, its credential lock
/// is released, and a new session may take the credential.
fn require_session_settled(rig: &Rig, held: &Held, log: &str) -> Result<()> {
    assert_eq!(
        held.session_state, "recovering",
        "the session did not settle after the provider terminal:\n{log}"
    );
    assert_eq!(
        held.lock_owner, None,
        "the credential lock outlived the terminal allocation:\n{log}"
    );
    rig.acquire_credential("worker-next-session")
        .context("a new session could not take the released credential")
}

fn require_basis(observations: &[serde_json::Value], basis: &str) {
    assert_eq!(
        observations.len(),
        1,
        "expected exactly one stored terminal observation: {observations:?}"
    );
    assert_eq!(observations[0]["basis"], json!(basis));
}

/// S1: a provider that reports the occurrence terminated settles the
/// allocation at startup, whatever the time, and stores the plain basis.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the built synthetic adapter binaries; see the module doc"]
async fn a_terminated_provider_settles_the_allocation_at_startup() -> Result<()> {
    let (rig, log, held, observations) =
        restarted_over(true, scripted("terminated", false, 0)).await?;
    let counts = real_adapter::counts_line(&log)?;
    assert!(
        counts.contains("discovered=1") && counts.contains("terminated=1"),
        "{counts}\n{log}"
    );
    assert_eq!(held.phase, ExternalAllocationPhase::Terminated, "{log}");
    require_basis(&observations, "observed_terminated");
    require_session_settled(&rig, &held, &log)
}

/// S2: a provider that cannot say keeps the allocation quarantined and the
/// session unsettled, and never holds readiness.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the built synthetic adapter binaries; see the module doc"]
async fn a_pending_provider_keeps_the_session_unsettled_and_the_node_ready() -> Result<()> {
    let (rig, log, held, observations) =
        restarted_over(true, scripted("pending", true, 20 * GRACE_MS)).await?;
    let counts = real_adapter::counts_line(&log)?;
    assert!(
        counts.contains("discovered=1") && counts.contains("terminated=0"),
        "{counts}\n{log}"
    );
    require_unsettled(&held, &log);
    assert!(observations.is_empty(), "{observations:?}");
    assert!(
        rig.acquire_credential("worker-next-session").is_err(),
        "a new session took a credential an unsettled lease still holds"
    );
    Ok(())
}

/// S3, rule (a): the daemon's own terminate was accepted and the provider then
/// answers not found, which is terminal before the lease end.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the built synthetic adapter binaries; see the module doc"]
async fn not_found_after_an_accepted_terminate_settles_by_rule_a() -> Result<()> {
    let (rig, log, held, observations) =
        restarted_over(true, scripted("not_found", true, 0)).await?;
    assert_eq!(held.phase, ExternalAllocationPhase::Terminated, "{log}");
    require_basis(&observations, "accepted_terminate_then_not_found");
    require_session_settled(&rig, &held, &log)
}

/// S4: not found with no accepted terminate, before the lease end, is only the
/// rule-(b) hold: the allocation stays quarantined and the guards keep the
/// session unsettled.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the built synthetic adapter binaries; see the module doc"]
async fn not_found_before_the_lease_end_holds_the_session() -> Result<()> {
    let (_rig, log, held, observations) =
        restarted_over(true, scripted("not_found", false, 0)).await?;
    require_unsettled(&held, &log);
    assert!(observations.is_empty(), "{observations:?}");
    Ok(())
}

/// S5, rule (b): not found, no accepted terminate, a bounded lease whose end
/// plus grace has passed on the adapter's clock. Time moves only through the
/// adapter's clock offset; no stored row is edited. The daemon records the
/// basis the adapter reports, so this also shows it accepts an offset-derived
/// past-end basis.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the built synthetic adapter binaries; see the module doc"]
async fn not_found_past_the_lease_end_and_grace_settles_by_rule_b() -> Result<()> {
    // The lease ends within two minutes of seeding; twenty minutes on is past
    // the end and its ten-minute grace.
    let (rig, log, held, observations) =
        restarted_over(true, scripted("not_found", false, 20 * 60 * 1000)).await?;
    assert_eq!(held.phase, ExternalAllocationPhase::Terminated, "{log}");
    require_basis(&observations, "past_latest_lease_end_not_found");
    require_session_settled(&rig, &held, &log)
}

/// S6: past the lease end but inside the grace the answer is still pending.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the built synthetic adapter binaries; see the module doc"]
async fn not_found_between_the_lease_end_and_the_grace_stays_pending() -> Result<()> {
    // Four minutes on: past the end (under two minutes out), inside the grace.
    let (_rig, log, held, observations) =
        restarted_over(true, scripted("not_found", false, 4 * 60 * 1000)).await?;
    require_unsettled(&held, &log);
    assert!(observations.is_empty(), "{observations:?}");
    Ok(())
}

/// S7: a reservation that retains no admitted lease request states an
/// unbounded lease end, which no clock passes.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the built synthetic adapter binaries; see the module doc"]
async fn not_found_for_an_unbounded_lease_stays_pending_however_late() -> Result<()> {
    let (_rig, log, held, observations) =
        restarted_over(false, scripted("not_found", false, 1_000 * GRACE_MS)).await?;
    require_unsettled(&held, &log);
    assert!(observations.is_empty(), "{observations:?}");
    Ok(())
}

/// S8: pending at startup (ready, quarantined), then the provider reports the
/// occurrence terminated and the periodic pass settles the allocation and then
/// the session.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the built synthetic adapter binaries; see the module doc"]
async fn pending_at_startup_is_settled_by_the_periodic_pass() -> Result<()> {
    let mut rig = Rig::start().await?;
    rig.seed(true)?;
    rig.script(scripted("pending", false, 0))?;
    rig.restart().await?;
    real_adapter::counts_line(&rig.log())?;
    rig.script(scripted("terminated", false, 0))?;
    let settled = "external occurrence termination observed terminal";
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(240);
    while !rig.log().contains(settled) {
        anyhow::ensure!(
            std::time::Instant::now() < deadline,
            "the periodic pass never observed the terminal:\n{}",
            rig.log()
        );
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    }
    rig.harness
        .stop_gracefully()
        .await
        .context("stop the node")?;
    let held = rig.held()?;
    assert_eq!(held.phase, ExternalAllocationPhase::Terminated);
    require_basis(&rig.stored_observations()?, "observed_terminated");
    require_session_settled(&rig, &held, &rig.log())
}

/// Open question 1: is a stored reservation's contact deadline editable in
/// place? Seeds a legitimate lease, rewrites the stored deadline into the past
/// while the node is stopped, and reports what the row and the restarted node
/// do. It records the outcome rather than assuming one.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the built synthetic adapter binaries; see the module doc"]
async fn experiment_a_stored_reservation_deadline_edit() -> Result<()> {
    let mut rig = Rig::start().await?;
    rig.seed(true)?;
    let past = lillux::time::timestamp_millis() - 3 * 60 * 60 * 1000;
    let edit = {
        let store = common::open_stopped_node_store(&rig.harness.state_path)?;
        store.rewrite_reservation_contact_deadline(real_adapter::PLACEMENT, past)
    };
    match edit {
        Err(error) => {
            eprintln!("Q1 RESULT: the stored row refuses a deadline edit: {error:#}");
            return Ok(());
        }
        Ok(()) => eprintln!("Q1: the stored row accepted the deadline edit"),
    }
    rig.script(scripted("not_found", false, 0))?;
    let started = rig.restart().await;
    let log = rig.log();
    match started {
        Err(error) => eprintln!(
            "Q1 RESULT: the edited row is accepted by the store but the node refuses to start: \
             {error:#}\n{log}"
        ),
        Ok(()) => {
            rig.harness.stop_gracefully().await?;
            let held = rig.held()?;
            let observations = rig.stored_observations()?;
            eprintln!(
                "Q1 RESULT: the node started over the edited row; phase={:?} observations={observations:?}\n{log}",
                held.phase
            );
        }
    }
    Ok(())
}

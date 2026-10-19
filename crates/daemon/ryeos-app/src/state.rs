use std::sync::Arc;
use std::time::Instant;

use serde::Serialize;

use ryeos_engine::engine::Engine;
use ryeos_engine::isolation::{IsolationMode, IsolationRuntime};
use ryeos_runtime::CommandRegistry;
use ryeos_runtime::authorizer::Authorizer;

use crate::callback_token::{CallbackCapabilityStore, ThreadAuthStore};
use crate::command_service::CommandService;
use crate::engine_cache::EngineCache;
use crate::event_store_service::EventStoreService;
use crate::event_stream::ThreadEventHub;
use crate::extension_state::ExtensionState;
use crate::ignore::IgnoreMatcher;
use crate::live_input_queue::LiveInputQueue;
use crate::node_config::NodeConfigSnapshot;
use crate::projection_health::ThreadProjectionHealthSnapshot;
use crate::resolution_cache::ResolutionCache;
use crate::service_registry::{ServiceDescriptor, ServiceRegistry};
use crate::state_store::StateStore;
use crate::thread_lifecycle::ThreadLifecycleService;
use crate::vault::NodeVault;
use crate::write_barrier::WriteBarrier;
use ryeos_node_runtime::config::Config;
use ryeos_node_runtime::identity::NodeIdentity;
use ryeos_node_runtime::node_policy::NodePolicySnapshot;

#[derive(Debug, Clone, Serialize)]
pub struct CatalogHealth {
    pub status: String,
    pub missing_services: Vec<String>,
}

#[derive(Clone)]
pub struct AppState {
    pub config: Arc<Config>,
    /// Build identity of the composed daemon binary. This must be injected by
    /// the composition root: `ryeos-app` has its own internal crate version,
    /// which is not the externally installed `ryeosd` release version.
    pub daemon_build: crate::build_info::BuildInfo,
    /// Node-owned isolation runtime resolved once at daemon composition.
    /// Execution paths share this immutable state instead of re-reading
    /// activation or backend policy from the general daemon config.
    pub isolation: Arc<IsolationRuntime>,
    pub state_store: Arc<StateStore>,
    pub engine: Arc<Engine>,
    /// Per-snapshot engine cache used for `pushed_head` requests.
    /// The cache materialises user content + builds a per-request
    /// engine overlay, keyed by `(system_install_generation,
    /// snapshot_hash)`. `LiveFs` requests bypass this cache and
    /// use `engine` directly.
    pub engine_cache: EngineCache,
    /// Admission-time resolution cache: memoizes the resolve/compose pipeline
    /// keyed by `(system_install_generation, ref, project_root)`, serving a hit
    /// only after content revalidation. See [`crate::resolution_cache`].
    pub resolution_cache: Arc<ResolutionCache>,
    pub identity: Arc<NodeIdentity>,
    pub threads: Arc<ThreadLifecycleService>,
    /// Operator live-input staging for *running* directive threads. The
    /// `threads.input` handler enqueues here; the running runtime drains it via
    /// `runtime.poll_input`, which persists each input as a durable
    /// `cognition_in` through the running-guarded append path. Shares its `Arc`
    /// with `ThreadLifecycleService` (wired by the composition root via
    /// `set_live_input_queue`) so finalization closes a thread's entry.
    pub live_input: Arc<LiveInputQueue>,
    pub events: Arc<EventStoreService>,
    /// Per-thread live broadcast hub for SSE subscribers. Populated by
    /// the UDS callback handler after persistence so subscribers see
    /// the same `PersistedEventRecord` instances the event store
    /// recorded.
    pub event_streams: Arc<ThreadEventHub>,
    pub commands: Arc<CommandService>,
    pub callback_tokens: Arc<CallbackCapabilityStore>,
    pub thread_auth: Arc<ThreadAuthStore>,
    /// Exact live-controller exclusion lifetime. This is a required
    /// composition-root authority rather than an optional extension: work
    /// admitted by this controller may retain it while a blocking host call
    /// settles, preventing a replacement controller from overlapping the old
    /// generation.
    pub controller_lifetime: Arc<ryeos_node_runtime::state_lock::StateLockLease>,
    /// Generic extension state bag for composition-root state that
    /// doesn't belong in core (e.g., UI state). Populated by the
    /// daemon composition root. Use `extensions.get::<T>()` to
    /// retrieve typed state.
    pub extensions: Arc<ExtensionState>,
    pub write_barrier: Arc<WriteBarrier>,
    pub started_at: Instant,
    pub started_at_iso: String,
    /// Result of the installed in-process registry self-check at startup.
    pub catalog_health: CatalogHealth,
    /// Handler registry for items whose signed kind schema selects the
    /// Services in-process terminator.
    pub services: Arc<ServiceRegistry>,
    /// Catalog of compiled Services-registry implementations. Signed installed
    /// items remain the operational catalog; this table supplies handler and
    /// per-endpoint availability mechanics for exact matching items.
    pub service_descriptors: &'static [ServiceDescriptor],
    /// Node-config snapshot loaded at startup.
    pub node_config: Arc<NodeConfigSnapshot>,
    /// Installed trusted lifecycle adapters. Empty is a fail-closed supported
    /// state: external profiles cannot reserve or contact an allocator.
    pub external_placement_backends:
        Arc<crate::external_placement::ExternalPlacementBackendRegistry>,
    /// Exact controller-side connector artifacts. Empty is fail-closed and
    /// external profile admission refuses before provider contact.
    pub external_candidate_connectors:
        Arc<crate::external_placement::ExternalCandidateConnectorRegistry>,
    /// Signed provider-owned configuration serializers. These are distinct
    /// from lifecycle adapters and from the connector executable itself.
    pub external_provider_configurations:
        Arc<crate::external_placement::ExternalProviderConfigurationRegistry>,
    /// Process-local coalescing for durable controller-side candidate import.
    /// The signed transcript and CAS remain authoritative across restart.
    pub external_candidate_imports:
        Arc<crate::external_candidate_import::ExternalCandidateImportPool>,
    /// Exact atomic node-owned semantic policy generation loaded at startup.
    pub node_policy: Arc<NodePolicySnapshot>,
    /// Operator-secret store. Delivering launches read it only through
    /// `vault::authorize_secret_delivery` after admission verification. The
    /// daemon stays vendor-agnostic — this trait moves opaque
    /// `String -> String` pairs and never enumerates provider names.
    pub vault: Arc<dyn NodeVault>,
    /// Provider-delivered environment values taken once at daemon startup
    /// for exactly the names the node policy declares. Read only through
    /// authorized secret delivery, alongside the vault.
    pub provider_env: Arc<crate::vault::ProviderEnvironment>,
    /// Command registry for token routing. Built once at startup
    /// from node-config command YAMLs.
    pub command_registry: Arc<CommandRegistry>,
    /// Unified capability evaluator. Built once at startup.
    /// All enforcement sites use this shared instance instead of constructing
    /// per-request.
    pub authorizer: Arc<Authorizer>,
    /// Ingest ignore matcher compiled from the retained node-policy snapshot.
    pub ignore_matcher: Arc<IgnoreMatcher>,
    /// Vault X25519 public key fingerprint. `None` when using
    /// `EmptyVault` or when the vault public key file doesn't exist.
    /// Populated at startup from `<system>/.ai/node/vault/public_key.pem`.
    pub vault_fingerprint: Option<String>,
    /// Daemon-owned financial accounting ledger (`accounting.sqlite3` plus
    /// its external monotonic anchor). `None` means the ledger failed to
    /// open or verify — every hard-budget admission then fails closed while
    /// unrelated non-hard work continues.
    pub accounting: Option<Arc<crate::accounting_db::AccountingDb>>,
    /// Meaning-blind daemon ownership for admitted persistent subprocesses.
    pub persistent_sessions: Arc<crate::persistent_session::PersistentSessionPool>,
    /// Immutable observation of the resources admitted by the retained node
    /// policy generation. Allocation remains owned by exact durable process
    /// occurrences, not by this selection catalog.
    pub execution_resources: Arc<crate::execution_resources::ExecutionResourcePool>,
    /// Process-local exact launch materials for scoped producer children.
    /// Empty after daemon restart; never reconstructed from durable paths.
    pub scoped_producer_authorities:
        Arc<crate::scoped_producer_authority::ScopedProducerAuthorityRegistry>,
    /// Exact live scoped children and their consumed launch-material lifelines.
    /// Empty after restart; callbacks cannot reconstruct process ownership.
    pub scoped_producer_processes:
        Arc<crate::scoped_producer_process::ScopedProducerProcessRegistry>,
    /// Original Q workspace and pinned state-root guards retained across
    /// fallible local start/pool binding in this daemon generation. Empty on
    /// restart; a durable row alone cannot recreate the backend view owner.
    pub qualification_workspace_custody:
        Arc<crate::qualification_workspace_custody::QualificationWorkspaceCustodyRegistry>,
}

#[derive(Debug, Serialize)]
pub struct IsolationStatus {
    pub mode: IsolationMode,
    /// Filesystem isolation follows the independently admitted isolation mode.
    pub filesystem_mode: IsolationMode,
    /// Network policy is reported independently; host networking is not
    /// described as isolation merely because filesystem isolation is active.
    pub network_mode: ryeos_engine::isolation::IsolationNetworkMode,
    pub version: u32,
    pub source: Option<String>,
    pub policy_digest: Option<String>,
    pub backend: ryeos_engine::isolation::IsolationBackendInspection,
    /// Protocol-specific process-control readiness. This is deliberately
    /// separate from general daemon health and isolation-backend availability.
    pub process_scopes: ryeos_engine::isolation::ProcessScopeReadiness,
}

#[derive(Debug, Serialize)]
pub struct StatusResponse {
    pub version: String,
    /// Git revision the binary was built from. Injected at build time
    /// (`RYEOS_VCS_REF`) or derived from the local git short SHA; `"unknown"`
    /// when neither is available. Lets an operator answer "which code is
    /// actually running?" — the version string alone can be stale/cosmetic.
    pub revision: String,
    /// Build timestamp injected at build time (`RYEOS_BUILD_DATE`), or
    /// `"unknown"` for local dev builds.
    pub build_date: String,
    pub started_at: String,
    pub uptime_seconds: u64,
    pub bind: String,
    pub uds_path: String,
    pub db_path: String,
    /// Immutable isolation snapshot currently shared by every execution path.
    pub isolation: IsolationStatus,
    pub active_threads: i64,
    pub thread_projection: ThreadProjectionHealthSnapshot,
    pub pending_head_transitions: crate::state_store::PendingHeadTransitionStatus,
    /// Accounting-ledger health. The node can be ready while accounting is
    /// degraded or unavailable; only hard-budget admission fails closed.
    pub accounting: AccountingStatus,
}

#[derive(Debug, Serialize)]
pub struct AccountingStatus {
    pub ledger_available: bool,
    pub hard_admission_enabled: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub outbox_unpublished: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub outbox_oldest_created_at_ms: Option<i64>,
}

impl AppState {
    /// Every secret source an authorized delivery may read.
    pub fn secret_sources(&self) -> crate::vault::SecretSources<'_> {
        crate::vault::SecretSources {
            vault: self.vault.as_ref(),
            provider_env: self.provider_env.as_ref(),
        }
    }

    /// The same one-shot participant used by durable lifecycle observations.
    pub fn scheduler(&self) -> Option<Arc<dyn crate::scheduler_boundary::SchedulerParticipant>> {
        self.threads.scheduler_participant()
    }

    /// Install exactly once during composition, before recovery or readiness.
    pub fn install_scheduler_participant(
        &self,
        participant: Arc<dyn crate::scheduler_boundary::SchedulerParticipant>,
    ) -> anyhow::Result<()> {
        self.threads.install_scheduler_participant(participant)
    }

    /// Register a composition part that may hold durable thread references.
    /// Allowed only before [`Self::seal_thread_pin_sources`].
    pub fn register_thread_pin_source(
        &self,
        source: Arc<dyn crate::retention_exclusion::ThreadPinSource>,
    ) -> anyhow::Result<()> {
        self.threads.retention().register(source)
    }

    /// Close the pin-source set exactly once, before any recovery and before
    /// readiness. A composition with no pin holders seals an empty set.
    pub fn seal_thread_pin_sources(&self) -> anyhow::Result<()> {
        self.threads.retention().seal()
    }

    /// Take the retention gate; complete it with the sealed sources' pins.
    pub async fn acquire_retention_gate(
        &self,
        mode: crate::retention_exclusion::ExclusionMode,
    ) -> anyhow::Result<crate::retention_exclusion::RetentionGate> {
        self.threads.retention().acquire_retention_gate(mode).await
    }

    /// Withdraw the same participant after request and scheduler-task drain.
    /// This is part of native shutdown custody, not ordinary capability removal.
    pub fn take_scheduler_participant_for_shutdown(
        &self,
    ) -> Option<Arc<dyn crate::scheduler_boundary::SchedulerParticipant>> {
        self.threads.take_scheduler_participant_for_shutdown()
    }

    pub fn node_history_policy(
        &self,
    ) -> anyhow::Result<&ryeos_engine::history_policy::ResolvedNodeThreadHistoryPolicy> {
        self.node_policy.require()
    }

    /// Acquire one descriptor-bound CAS read capability. The shared guard is
    /// retained for the lifetime of the store so a concurrent sweep cannot
    /// remove objects between traversal and payload reads.
    pub fn acquire_cas_read(&self) -> anyhow::Result<PinnedCasRead> {
        let authority = self.state_store.pinned_state_authority()?;
        let guard = authority.acquire_shared_guard()?;
        let cas = authority.cas_store()?;
        Ok(PinnedCasRead { _guard: guard, cas })
    }

    pub fn status(&self) -> anyhow::Result<StatusResponse> {
        let pending_head_transitions = self.state_store.pending_head_transition_status()?;
        self.state_store
            .projection_health()
            .observe_pending_transitions(pending_head_transitions.pending);
        Ok(StatusResponse {
            version: self.daemon_build.version.to_string(),
            revision: self.daemon_build.revision.to_string(),
            build_date: self.daemon_build.build_date.to_string(),
            started_at: self.started_at_iso.clone(),
            uptime_seconds: self.started_at.elapsed().as_secs(),
            bind: self.config.bind.to_string(),
            uds_path: self.config.uds_path.display().to_string(),
            db_path: self.config.db_path.display().to_string(),
            isolation: IsolationStatus {
                mode: self.isolation.mode(),
                filesystem_mode: self.isolation.mode(),
                network_mode: self.isolation.inspection().network.mode,
                version: self.isolation.version(),
                source: self
                    .isolation
                    .source()
                    .map(|path| path.display().to_string()),
                policy_digest: self.isolation.digest().map(str::to_owned),
                backend: self.isolation.inspection().backend.clone(),
                process_scopes: self.isolation.inspection().process_scope_readiness.clone(),
            },
            active_threads: self.state_store.active_thread_count().unwrap_or(0),
            thread_projection: self.state_store.projection_health_snapshot(),
            pending_head_transitions,
            accounting: match &self.accounting {
                Some(ledger) => {
                    let (outbox_unpublished, outbox_oldest_created_at_ms) = ledger
                        .unpublished_outbox_stats()
                        .map(|(count, oldest)| (Some(count), oldest))
                        .unwrap_or((None, None));
                    AccountingStatus {
                        ledger_available: true,
                        hard_admission_enabled: ledger.hard_admission_enabled(),
                        outbox_unpublished,
                        outbox_oldest_created_at_ms,
                    }
                }
                None => AccountingStatus {
                    ledger_available: false,
                    hard_admission_enabled: false,
                    outbox_unpublished: None,
                    outbox_oldest_created_at_ms: None,
                },
            },
        })
    }
}

/// Minimal real application composition for cross-crate protocol tests. The
/// caller owns the directory lifetime. No API handlers, placement binding, or
/// vault authority is preinstalled; composed tests must add only the exact
/// production boundary they exercise.
///
/// The Store opens through the native test gate. Its owner comes first in the
/// returned pair so that `let (store_owner, state) = build(..)` drops the
/// application, and every Store `Arc` it holds, before the owner closes the
/// Store. The owner stays on the opening thread.
#[cfg(any(test, feature = "test-support"))]
pub mod test_support {
    use std::sync::Arc;

    use anyhow::Result;

    use super::{AppState, CatalogHealth};
    use crate::state_store::BundleAdmissionTrust;
    use crate::state_store::test_native::TestStateStore;

    pub fn build(root: &std::path::Path) -> Result<(TestStateStore, AppState)> {
        build_with_bundle_admission(root, |_| BundleAdmissionTrust::unbound())
    }

    /// [`build`] with the Store bound to the Bundle admission trust a node
    /// binds: the live platform signers plus this node's own identity, which
    /// is what the composed test Engine admits Bundles with.
    pub fn build_bound(root: &std::path::Path) -> Result<(TestStateStore, AppState)> {
        build_with_bundle_admission(root, |identity| {
            let mut node_trust = ryeos_engine::test_support::live_trust_store();
            node_trust.extend_from(&ryeos_engine::trust::TrustStore::from_signers(vec![
                ryeos_engine::trust::TrustedSigner {
                    fingerprint: identity.fingerprint().to_owned(),
                    verifying_key: *identity.verifying_key(),
                    label: Some("test node identity".into()),
                },
            ]));
            BundleAdmissionTrust::bound(node_trust)
        })
    }

    fn build_with_bundle_admission(
        root: &std::path::Path,
        admission: impl FnOnce(&ryeos_node_runtime::identity::NodeIdentity) -> BundleAdmissionTrust,
    ) -> Result<(TestStateStore, AppState)> {
        let (config, identity) = test_identity(root)?;
        let bundle_admission_trust = admission(&identity);
        let signer = Arc::new(crate::state_store::NodeIdentitySigner::from_identity(
            &identity,
        ));
        let mut head_trust = ryeos_state::refs::TrustStore::new();
        head_trust.insert(identity.fingerprint().to_owned(), *identity.verifying_key());
        let write_barrier = crate::write_barrier::WriteBarrier::new();
        let store_owner = TestStateStore::open_with_bundle_admission_trust(
            root,
            signer,
            write_barrier.clone(),
            Arc::new(head_trust),
            bundle_admission_trust,
        )?;
        let state_store = Arc::clone(store_owner.store());
        // The owner already holds this App root's operator lock; a second
        // acquisition would conflict with it.
        let controller_lifetime = store_owner.controller_lifetime();
        let state = compose(
            config,
            identity,
            state_store,
            write_barrier,
            controller_lifetime,
        )?;
        Ok((store_owner, state))
    }

    /// Compose an application over a Store the caller already holds, without
    /// opening another. A thread holds one test Store at a time, so a test
    /// that needs an application over an existing Store uses this. `root`
    /// receives the application's own identity keys and configuration.
    pub fn build_with_store(
        root: &std::path::Path,
        state_store: Arc<crate::state_store::StateStore>,
        controller_lifetime: Arc<ryeos_node_runtime::state_lock::StateLockLease>,
    ) -> Result<AppState> {
        let (config, identity) = test_identity(root)?;
        compose(
            config,
            identity,
            state_store,
            crate::write_barrier::WriteBarrier::new(),
            controller_lifetime,
        )
    }

    fn test_identity(
        root: &std::path::Path,
    ) -> Result<(
        ryeos_node_runtime::config::Config,
        ryeos_node_runtime::identity::NodeIdentity,
    )> {
        let runtime_db_path = TestStateStore::runtime_state_dir(root).join("runtime.sqlite3");
        let key_path = root.join("identity/node-key.pem");
        let config = ryeos_node_runtime::config::Config {
            bind: "127.0.0.1:0".parse()?,
            db_path: runtime_db_path,
            uds_path: root.join("test.sock"),
            app_root: root.to_path_buf(),
            node_signing_key_path: key_path.clone(),
            operator_signing_key_path: root.join("identity/operator-key.pem"),
            authorized_keys_dir: root.join("auth"),
        };
        let identity = ryeos_node_runtime::identity::NodeIdentity::create(&key_path)?;
        ryeos_node_runtime::identity::NodeIdentity::create(&config.operator_signing_key_path)?;
        Ok((config, identity))
    }

    fn compose(
        config: ryeos_node_runtime::config::Config,
        identity: ryeos_node_runtime::identity::NodeIdentity,
        state_store: Arc<crate::state_store::StateStore>,
        write_barrier: crate::write_barrier::WriteBarrier,
        controller_lifetime: Arc<ryeos_node_runtime::state_lock::StateLockLease>,
    ) -> Result<AppState> {
        let engine = Arc::new(ryeos_engine::engine::Engine::new(
            ryeos_engine::kind_registry::KindRegistry::empty(),
            ryeos_engine::parsers::ParserDispatcher::new(
                ryeos_engine::parsers::ParserRegistry::empty(),
                Arc::new(ryeos_engine::handlers::HandlerRegistry::empty()),
            ),
            Vec::new(),
        ));
        let kind_profiles = Arc::new(crate::kind_profiles::KindProfileRegistry::build(None));
        let events = Arc::new(crate::event_store_service::EventStoreService::new(
            state_store.clone(),
        ));
        let event_streams = Arc::new(crate::event_stream::ThreadEventHub::new(16));
        let threads = Arc::new(
            crate::thread_lifecycle::ThreadLifecycleService::new_for_test_with_site_id(
                state_store.clone(),
                engine.clone(),
                kind_profiles.clone(),
                events.clone(),
                event_streams.clone(),
                "site:composed-test",
            )?,
        );
        let commands = Arc::new(crate::command_service::CommandService::new(
            state_store.clone(),
            kind_profiles,
            events.clone(),
        ));
        Ok(AppState {
            config: Arc::new(config),
            daemon_build: crate::build_info::get(),
            isolation: Arc::new(
                ryeos_engine::isolation::IsolationRuntime::disabled_for_authoring(),
            ),
            state_store,
            engine,
            engine_cache: crate::engine_cache::EngineCache::new(
                crate::engine_cache::EngineCacheConfig::default(),
            ),
            resolution_cache: Arc::new(crate::resolution_cache::ResolutionCache::new(128)),
            identity: Arc::new(identity),
            threads,
            live_input: Arc::new(crate::live_input_queue::LiveInputQueue::new()),
            events,
            event_streams,
            commands,
            callback_tokens: Arc::new(crate::callback_token::CallbackCapabilityStore::new()),
            thread_auth: Arc::new(crate::callback_token::ThreadAuthStore::new()),
            controller_lifetime,
            extensions: Arc::new(crate::extension_state::ExtensionState::new()),
            write_barrier: Arc::new(write_barrier),
            started_at: std::time::Instant::now(),
            started_at_iso: String::new(),
            catalog_health: CatalogHealth {
                status: "ok".into(),
                missing_services: Vec::new(),
            },
            services: Arc::new(crate::service_registry::ServiceRegistry::new()),
            service_descriptors: &[],
            node_config: Arc::new(crate::node_config::NodeConfigSnapshot {
                external_execution: Vec::new(),
                runtime_snapshot_production: Vec::new(),
                guest_runtime_materialization: Vec::new(),
                runtime_snapshot_qualification: Vec::new(),
                bundles: Vec::new(),
                routes: Vec::new(),
                commands: Vec::new(),
            }),
            external_placement_backends: Arc::new(Default::default()),
            external_candidate_connectors: Arc::new(Default::default()),
            external_provider_configurations: Arc::new(Default::default()),
            external_candidate_imports: Arc::new(Default::default()),
            node_policy: Arc::new(
                ryeos_node_runtime::node_policy::NodePolicySnapshot::from_test_records(Vec::new()),
            ),
            vault: Arc::new(crate::vault::EmptyVault),
            provider_env: Arc::new(crate::vault::ProviderEnvironment::none()),
            command_registry: Arc::new(ryeos_runtime::CommandRegistry::from_records(
                &[],
                &Default::default(),
            )?),
            authorizer: Arc::new(ryeos_runtime::authorizer::Authorizer::new()),
            ignore_matcher: Arc::new(crate::ignore::IgnoreMatcher::from_config(
                &crate::ignore::IgnoreConfig {
                    patterns: Vec::new(),
                },
            )?),
            vault_fingerprint: None,
            accounting: None,
            persistent_sessions: Arc::new(crate::persistent_session::PersistentSessionPool::new()),
            execution_resources: Arc::new(
                crate::execution_resources::ExecutionResourcePool::deny_all(),
            ),
            scoped_producer_authorities: Arc::new(Default::default()),
            scoped_producer_processes: Arc::new(Default::default()),
            qualification_workspace_custody: Arc::new(Default::default()),
        })
    }
}

pub struct PinnedCasRead {
    _guard: ryeos_state::CasMutationGuard,
    cas: lillux::CasStore,
}

impl PinnedCasRead {
    pub fn cas(&self) -> &lillux::CasStore {
        &self.cas
    }
}

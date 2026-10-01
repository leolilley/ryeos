//! Authenticated public endpoint for an explicitly prepared disposable native
//! host node. This module never provisions a host service or changes node
//! policy: both are administrator-owned prerequisites to the diagnostic.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, ensure};
use lillux::crypto::SigningKey;
use serde_json::{Value, json};

use crate::common;

pub(crate) trait QualificationEndpoint {
    fn state_path(&self) -> &Path;

    async fn post_json(
        &self,
        route_path: &str,
        body: Value,
    ) -> Result<(reqwest::StatusCode, Value)>;

    async fn post_execute(
        &self,
        item_ref: &str,
        parameters: Value,
    ) -> Result<(reqwest::StatusCode, Value)> {
        self.post_json(
            "/execute",
            json!({
                "item_ref": item_ref,
                "ref_bindings": {},
                "project_path": null,
                "parameters": parameters,
                "execution_policy": ryeos_app::execution_policy::ExecutionPolicy::projectless(
                    ryeos_app::execution_policy::ExecutionResponse::Wait,
                ),
            }),
        )
        .await
    }
}

impl QualificationEndpoint for common::DaemonHarness {
    fn state_path(&self) -> &Path {
        &self.state_path
    }

    async fn post_json(
        &self,
        route_path: &str,
        body: Value,
    ) -> Result<(reqwest::StatusCode, Value)> {
        common::DaemonHarness::post_json(self, route_path, body).await
    }

    async fn post_execute(
        &self,
        item_ref: &str,
        parameters: Value,
    ) -> Result<(reqwest::StatusCode, Value)> {
        common::DaemonHarness::post_execute(self, item_ref, ".", parameters).await
    }
}

/// The exact installed service and initialized fixture are supplied by the
/// operator. Merely setting an address cannot turn a direct daemon into a
/// host-associated process-scope owner.
pub(crate) struct HostBackedQualificationEndpoint {
    state_path: PathBuf,
    bind: SocketAddr,
    authority_digest: String,
    expected_revision: String,
    user_key: SigningKey,
    node_key: SigningKey,
    #[cfg(unix)]
    peer: lillux::local_ipc::AuthenticatedUnixPeer,
}

impl HostBackedQualificationEndpoint {
    pub(crate) fn from_explicit_disposable_node() -> Result<Self> {
        let state_path = PathBuf::from(
            std::env::var_os("RYEOS_TEST_DISPOSABLE_HOST_APP_ROOT")
                .context("RYEOS_TEST_DISPOSABLE_HOST_APP_ROOT is required")?,
        );
        ensure!(
            state_path.is_absolute(),
            "disposable host app root must be absolute"
        );
        let pinned_root = lillux::PinnedDirectory::open(&state_path)?
            .context("disposable host app root is absent")?;
        ensure!(
            pinned_root.path() == state_path,
            "disposable host app root changed its exact path"
        );
        let marker = state_path.join(".ryeos-disposable-codex-diagnostic");
        let marker_bytes = lillux::read_regular_file_bounded_no_follow(&marker, 512)
            .context("explicit disposable Codex diagnostic marker is absent")?;
        ensure!(
            marker_bytes == b"ryeos-disposable-codex-diagnostic-v1\n",
            "app root has no exact disposable diagnostic marker"
        );
        let service = ryeos_node::supervision::InstalledService::discover_app_root(&state_path)?
            .context("disposable node has no administrator-owned host association")?;
        service.binding.runtime.validate(&state_path)?;
        ensure!(
            service.binding.runtime.app_root == state_path,
            "host association selected a different app root"
        );
        let installed_path = PathBuf::from(
            std::env::var_os("RYEOS_TEST_FINAL_INSTALLED_RYEOSD")
                .context("RYEOS_TEST_FINAL_INSTALLED_RYEOSD is required")?,
        );
        let expected_sha256 = std::env::var("RYEOS_TEST_FINAL_INSTALLED_RYEOSD_SHA256")
            .context("RYEOS_TEST_FINAL_INSTALLED_RYEOSD_SHA256 is required")?;
        let expected_revision = std::env::var("RYEOS_TEST_FINAL_SOURCE_REVISION")
            .context("RYEOS_TEST_FINAL_SOURCE_REVISION is required")?;
        ensure!(
            installed_path.is_absolute()
                && service.binding.daemon_executable == installed_path
                && lillux::valid_hash(&expected_sha256)
                && !expected_revision.trim().is_empty()
                && expected_revision.len() <= 128,
            "host service does not pin the exact final installed daemon image"
        );
        let image = lillux::secure_fs::open_pinned_regular_file_no_follow(&installed_path)?;
        let observed = image.observation()?;
        ensure!(
            image.digest_stable_exact(&observed)? == expected_sha256,
            "installed host daemon differs from the pinned final candidate"
        );
        let config = ryeos_node::NodeConfig::load_local(Some(state_path.clone()))?;
        ensure!(
            config.app_root == state_path && config.bind.ip().is_loopback(),
            "disposable host node must have its exact loopback endpoint"
        );
        let user_key = lillux::crypto::load_signing_key(
            &state_path.join(".ai/config/keys/signing/private_key.pem"),
        )?;
        let node_key = lillux::crypto::load_signing_key(
            &state_path.join(".ai/node/identity/private_key.pem"),
        )?;
        ensure!(
            lillux::signature::compute_fingerprint(&node_key.verifying_key())
                == service.binding.runtime.node_fingerprint
                && lillux::signature::compute_fingerprint(&node_key.verifying_key())
                    == lillux::signature::compute_fingerprint(
                        &common::fast_fixture::node_signing_key().verifying_key(),
                    )
                && lillux::signature::compute_fingerprint(&user_key.verifying_key())
                    == lillux::signature::compute_fingerprint(
                        &common::fast_fixture::user_signing_key().verifying_key(),
                    ),
            "disposable host does not carry the signed diagnostic fixture identity"
        );
        service.binding.runtime.account.require_current_process()?;
        #[cfg(unix)]
        let peer = {
            use std::os::fd::AsFd as _;
            let stream = std::os::unix::net::UnixStream::connect(&config.uds_path)
                .context("connect exact supervised diagnostic control endpoint")?;
            let peer = lillux::local_ipc::AuthenticatedUnixPeer::capture(stream.as_fd())?;
            peer.require_executable_name(std::ffi::OsStr::new("ryeosd"))?;
            ensure!(
                peer.executable_digest_exact(observed.size())? == expected_sha256,
                "running supervised daemon differs from pinned final installed candidate"
            );
            peer
        };
        #[cfg(not(unix))]
        anyhow::bail!("native host-backed diagnostic requires Unix process identity");
        let authority_digest = service.binding.runtime.identity_digest()?;
        Ok(Self {
            state_path,
            bind: config.bind,
            authority_digest,
            expected_revision,
            user_key,
            node_key,
            #[cfg(unix)]
            peer,
        })
    }

    pub(crate) fn require_running_status(&self, status: &Value) -> Result<()> {
        ensure!(
            status["revision"] == self.expected_revision
                && status["bind"] == self.bind.to_string()
                && status["isolation"]["process_scopes"]["authority_digest"]
                    == self.authority_digest,
            "running diagnostic node differs from pinned candidate or protected host binding"
        );
        Ok(())
    }
}

impl QualificationEndpoint for HostBackedQualificationEndpoint {
    fn state_path(&self) -> &Path {
        &self.state_path
    }

    async fn post_json(
        &self,
        route_path: &str,
        body: Value,
    ) -> Result<(reqwest::StatusCode, Value)> {
        #[cfg(unix)]
        ensure!(
            !self.peer.has_exited()?,
            "pinned supervised daemon exited before public diagnostic request"
        );
        let bytes = serde_json::to_vec(&body)?;
        let mut request = reqwest::Client::new()
            .post(format!("http://{}{}", self.bind, route_path))
            .header("content-type", "application/json")
            .body(bytes.clone());
        for (name, value) in common::build_signed_headers_for_bytes(
            &self.user_key,
            &self.node_key,
            "POST",
            route_path,
            &bytes,
        ) {
            request = request.header(name, value);
        }
        let response = request.send().await?;
        let status = response.status();
        let body = response
            .json()
            .await
            .context("decode signed host response")?;
        #[cfg(unix)]
        ensure!(
            !self.peer.has_exited()?,
            "pinned supervised daemon exited during public diagnostic request"
        );
        Ok((status, body))
    }
}

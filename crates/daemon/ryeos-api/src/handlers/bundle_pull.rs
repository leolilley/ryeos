//! `bundle/pull` — install a published bundle from a link and two pins.
//!
//! The operator gives a `/published/<id>` URL, the node key fingerprint they
//! trust (`--node`) and the publication record hash they expect (`--expect`).
//! The local daemon fetches, verifies and installs, so staging, the install
//! journal and admission stay daemon-side, exactly as for
//! `remote/bundle-install`.
//!
//! The node verified that its active operator signed; the puller verifies the
//! node. Checks run in this order, each with a typed refusal, and no blob is
//! fetched before the last of them passes:
//!
//! 1. the node's `/public-key` answers with the pinned fingerprint, a key that
//!    hashes to it, and a build of the same major.minor as this daemon;
//! 2. the served record hashes to `--expect` (and that hash is the recomputed
//!    canonical body hash), names the pinned node, carries a countersignature
//!    that verifies under the node key, and has not passed `not_after`;
//! 3. the served manifest hashes to the record's `manifest_hash` and names the
//!    record's bundle;
//! 4. the manifest's publisher is a signer this node trusts.
//!
//! Then every blob is fetched with one GET, hashed against its manifest entry
//! and materialised, and the completed staging goes through the same admission
//! and activation `remote/bundle-install` uses. The pull never carries, offers
//! or adds trust: an unpinned publisher is refused with the command that pins
//! one.
//!
//! The fetch is public. It uses a plain HTTP client: https only (plain http
//! only to a loopback address), no redirects, connect and total timeouts, a
//! response size bound on every read, and no identity of any kind on the
//! request: no signed-request headers, cookies or key material.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context as _;
use ryeos_engine::trust::{
    MAX_PUBLICATION_MANIFEST_BYTES, PublicationManifest, ServedPublication, TrustStore,
    compute_fingerprint,
};
use ryeos_executor::executor::ServiceAvailability;
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};

use super::remote_bundle_install::{
    BlobMismatch, BlobSource, BundleExportEntry, BundleExportResponse, install_entries,
    precheck_install, validate_export_response,
};
use crate::handler_context::HandlerContext;
use crate::handler_error::{HandlerError, HandlerResult};
use crate::registry::ServiceDescriptor;
use ryeos_app::state::AppState;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(120);
/// Largest `/public-key` answer read.
const PUBLIC_KEY_MAX_BYTES: u64 = 64 * 1024;
/// Largest refusal body read from a failing answer.
const ERROR_BODY_MAX_BYTES: u64 = 16 * 1024;
/// Largest describe answer read: the manifest's own bound plus the record,
/// countersignature and framing around it.
const DESCRIBE_MAX_BYTES: u64 = MAX_PUBLICATION_MANIFEST_BYTES + 256 * 1024;

// ── Typed refusals ──────────────────────────────────────────────────

/// Why a pull was refused. Every refusal has a stable `publication_*` code.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PullRefusal {
    #[error("publication_malformed: {0}")]
    Malformed(String),
    #[error("publication_insecure_url: {0}")]
    InsecureUrl(String),
    #[error("publication_fetch_failed: {0}")]
    FetchFailed(String),
    #[error("publication_node_mismatch: {0}")]
    NodeMismatch(String),
    #[error(
        "publication_version_mismatch: the node runs {remote}, this daemon runs {local}; pulls \
         need the same major.minor on both"
    )]
    VersionMismatch { local: String, remote: String },
    #[error("publication_hash_mismatch: {0}")]
    HashMismatch(String),
    #[error(
        "publication_signature_invalid: the node countersignature does not verify under the \
         pinned node key"
    )]
    SignatureInvalid,
    #[error("publication_expired: the publication's not_after has passed")]
    Expired,
    #[error("publication_manifest_mismatch: {0}")]
    ManifestMismatch(String),
    #[error("publication_blob_mismatch: {0}")]
    BlobMismatch(String),
    #[error(
        "publication_publisher_unpinned: the bundle is signed by publisher fp:{fingerprint}, \
         which this node does not trust. The pull never adds trust. Obtain the publisher's \
         PUBLISHER_TRUST.toml from a source you trust, check it names fp:{fingerprint}, stop the \
         node and run `ryeos trust pin --from <PUBLISHER_TRUST.toml> --role publisher`, then pull \
         again"
    )]
    PublisherUnpinned { fingerprint: String },
    #[error(
        "publication_bundle_not_authorized: this node's sealed command-registration policy names \
         no authority for bundle `{0}`, and the policy cannot change after initialization. An \
         ordinary `ryeos bundle install` of this bundle refuses the same way. The node must be \
         initialized with a profile that lists `{0}` under `bundle_source_caps` (an empty list \
         grants no command authority) before this bundle can be installed"
    )]
    BundleNotAuthorized(String),
}

impl PullRefusal {
    pub fn code(&self) -> &'static str {
        match self {
            Self::Malformed(_) => "publication_malformed",
            Self::InsecureUrl(_) => "publication_insecure_url",
            Self::FetchFailed(_) => "publication_fetch_failed",
            Self::NodeMismatch(_) => "publication_node_mismatch",
            Self::VersionMismatch { .. } => "publication_version_mismatch",
            Self::HashMismatch(_) => "publication_hash_mismatch",
            Self::SignatureInvalid => "publication_signature_invalid",
            Self::Expired => "publication_expired",
            Self::ManifestMismatch(_) => "publication_manifest_mismatch",
            Self::BlobMismatch(_) => "publication_blob_mismatch",
            Self::PublisherUnpinned { .. } => "publication_publisher_unpinned",
            Self::BundleNotAuthorized(_) => "publication_bundle_not_authorized",
        }
    }

    fn status(&self) -> u16 {
        match self {
            Self::Malformed(_) | Self::InsecureUrl(_) => 400,
            Self::FetchFailed(_) => 502,
            _ => 422,
        }
    }

    fn into_handler_error(self) -> HandlerError {
        HandlerError::Structured {
            code: self.code().to_owned(),
            status: self.status(),
            body: json!({ "code": self.code(), "error": self.to_string() }),
        }
    }
}

/// Map an install-path error onto the pull's typed refusals. Anything else is
/// the install's own failure (admission, activation) and keeps its message.
fn classify(error: anyhow::Error) -> HandlerError {
    if let Some(refusal) = error.downcast_ref::<PullRefusal>() {
        return refusal.clone().into_handler_error();
    }
    if let Some(mismatch) = error.downcast_ref::<BlobMismatch>() {
        return PullRefusal::BlobMismatch(mismatch.0.clone()).into_handler_error();
    }
    HandlerError::Structured {
        code: "publication_install_failed".to_owned(),
        status: 422,
        body: json!({
            "code": "publication_install_failed",
            "error": format!("publication_install_failed: {error:#}"),
        }),
    }
}

/// The node's sealed command-registration policy must already name the bundle.
/// Installation refuses a prospective set it does not cover, so say so before
/// any blob moves and name the step.
fn check_bundle_authorized(state: &AppState, bundle_name: &str) -> Result<(), PullRefusal> {
    let policy_table = ryeos_node_runtime::node_policy::NodePolicyTable::new();
    let snapshot = ryeos_node_runtime::node_policy::load_snapshot(
        &state.config.app_root,
        &state.engine.node_trust_store,
        &policy_table,
    )
    .map_err(|error| PullRefusal::Malformed(format!("load node policy: {error:#}")))?;
    let authority = snapshot
        .require::<ryeos_node_runtime::node_policy::sections::command_registration::CommandRegistrationAuthority>()
        .map_err(|error| PullRefusal::Malformed(format!("node policy: {error:#}")))?;
    authority
        .require_bundle_sources([bundle_name])
        .map_err(|_| PullRefusal::BundleNotAuthorized(bundle_name.to_owned()))
}

// ── The pins and the URL ────────────────────────────────────────────

/// What the operator pinned on the command line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Pins {
    /// The node key fingerprint, without the `fp:` prefix.
    pub node: String,
    /// The publication record body hash.
    pub expect: String,
}

impl Pins {
    pub(crate) fn parse(node: &str, expect: &str) -> Result<Self, PullRefusal> {
        let node = node.strip_prefix("fp:").unwrap_or(node);
        if !lillux::valid_hash(node) {
            return Err(PullRefusal::Malformed(
                "--node must be fp:<64 lowercase hex digits>".into(),
            ));
        }
        if !lillux::valid_hash(expect) {
            return Err(PullRefusal::Malformed(
                "--expect must be a 64 lowercase hex digit record hash".into(),
            ));
        }
        Ok(Self {
            node: node.to_owned(),
            expect: expect.to_owned(),
        })
    }
}

/// Where to fetch from: the origin, and the publication id in the path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PullTarget {
    pub origin: String,
    pub id: String,
}

fn is_loopback(host: Option<url::Host<&str>>) -> bool {
    match host {
        Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
        Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
        Some(url::Host::Domain(name)) => name.eq_ignore_ascii_case("localhost"),
        None => false,
    }
}

/// Parse `https://<host>/published/<id>`. Plain http is accepted only for a
/// loopback address; credentials, a query and a fragment are refused.
pub(crate) fn parse_target(raw: &str) -> Result<PullTarget, PullRefusal> {
    let url = url::Url::parse(raw)
        .map_err(|error| PullRefusal::Malformed(format!("the URL does not parse: {error}")))?;
    match url.scheme() {
        "https" => {}
        "http" if is_loopback(url.host()) => {}
        _ => {
            return Err(PullRefusal::InsecureUrl(
                "a publication is pulled over https; plain http is accepted only for a loopback \
                 address"
                    .into(),
            ));
        }
    }
    if url.host().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(PullRefusal::Malformed(
            "the URL must be https://<host>/published/<id> with no credentials, query or fragment"
                .into(),
        ));
    }
    let id = url
        .path()
        .trim_end_matches('/')
        .strip_prefix("/published/")
        .filter(|id| ryeos_engine::trust::is_publication_id(id))
        .ok_or_else(|| {
            PullRefusal::Malformed(
                "the URL path must be /published/<32 lowercase hex digit id>".into(),
            )
        })?;
    Ok(PullTarget {
        origin: url.origin().ascii_serialization(),
        id: id.to_owned(),
    })
}

// ── Verification ────────────────────────────────────────────────────

/// The parts of the node's `/public-key` answer the pull reads.
#[derive(Debug, serde::Deserialize)]
pub(crate) struct PublicKeyDoc {
    pub fingerprint: String,
    pub signing_key: String,
    pub version: String,
}

/// The release series a build belongs to: its major and minor numbers.
fn release_series(version: &str) -> Option<(u64, u64)> {
    let mut parts = version.trim().trim_start_matches('v').split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    Some((major, minor))
}

/// Step 1: the node at the URL is the pinned node and a build this daemon
/// can pull from. Returns the key its countersignature must verify under.
pub(crate) fn check_public_key(
    doc: &PublicKeyDoc,
    pinned_node: &str,
    local_version: &str,
) -> Result<lillux::crypto::VerifyingKey, PullRefusal> {
    let reported = doc
        .fingerprint
        .strip_prefix("fp:")
        .unwrap_or(&doc.fingerprint);
    if reported != pinned_node {
        return Err(PullRefusal::NodeMismatch(format!(
            "the node at this URL has key fp:{reported}, not the pinned fp:{pinned_node}"
        )));
    }
    let key = crate::remote::config::decode_signing_key(&doc.signing_key)
        .map_err(|error| PullRefusal::Malformed(format!("the node's signing_key: {error:#}")))?;
    if compute_fingerprint(&key) != pinned_node {
        return Err(PullRefusal::NodeMismatch(
            "the node's signing_key does not hash to its fingerprint".into(),
        ));
    }
    match (release_series(&doc.version), release_series(local_version)) {
        (Some(remote), Some(local)) if remote == local => Ok(key),
        _ => Err(PullRefusal::VersionMismatch {
            local: local_version.to_owned(),
            remote: doc.version.clone(),
        }),
    }
}

/// A publication whose record, countersignature and manifest all verified.
#[derive(Debug)]
pub(crate) struct VerifiedPublication {
    pub served: ServedPublication,
    pub manifest: PublicationManifest,
}

/// Steps 2 and 3: the describe answer against the pins and the node key.
pub(crate) fn verify_publication(
    body: &Value,
    node_key: &lillux::crypto::VerifyingKey,
    pins: &Pins,
    now_secs: u64,
) -> Result<VerifiedPublication, PullRefusal> {
    let malformed = |what: &str, error: &dyn std::fmt::Display| {
        PullRefusal::Malformed(format!("{what}: {error}"))
    };
    let mut object = body
        .as_object()
        .cloned()
        .ok_or_else(|| PullRefusal::Malformed("the publication answer is not an object".into()))?;
    let manifest_value = object
        .remove("manifest")
        .ok_or_else(|| PullRefusal::Malformed("the publication answer has no manifest".into()))?;
    let served: ServedPublication = serde_json::from_value(Value::Object(object))
        .map_err(|error| malformed("the publication answer", &error))?;
    let manifest: PublicationManifest = serde_json::from_value(manifest_value)
        .map_err(|error| malformed("the manifest", &error))?;
    let record = &served.record;
    record
        .validate_shape()
        .map_err(|error| malformed("the record", &error))?;

    let recomputed = record
        .body_hash()
        .map_err(|error| malformed("the record", &error))?;
    if served.record_hash != pins.expect || recomputed != pins.expect {
        return Err(PullRefusal::HashMismatch(format!(
            "the record hashes to {recomputed} and the node calls it {}; --expect is {}",
            served.record_hash, pins.expect
        )));
    }
    if record.node_fingerprint != pins.node {
        return Err(PullRefusal::NodeMismatch(format!(
            "the record is for node fp:{}, not the pinned fp:{}",
            record.node_fingerprint, pins.node
        )));
    }
    served
        .verify(node_key)
        .map_err(|_| PullRefusal::SignatureInvalid)?;
    if record.not_after_secs() <= now_secs {
        return Err(PullRefusal::Expired);
    }

    let canonical = lillux::canonical_json(
        &serde_json::to_value(&manifest).map_err(|error| malformed("the manifest", &error))?,
    )
    .map_err(|_| PullRefusal::ManifestMismatch("the manifest is not canonicalizable".into()))?;
    if canonical.len() as u64 > MAX_PUBLICATION_MANIFEST_BYTES {
        return Err(PullRefusal::ManifestMismatch(
            "the manifest exceeds its size bound".into(),
        ));
    }
    if lillux::sha256_hex(canonical.as_bytes()) != record.root.manifest_hash {
        return Err(PullRefusal::ManifestMismatch(
            "the manifest does not hash to the record's manifest_hash".into(),
        ));
    }
    manifest
        .validate_shape()
        .map_err(|error| PullRefusal::ManifestMismatch(error.to_string()))?;
    if manifest.bundle_name != record.bundle_name {
        return Err(PullRefusal::ManifestMismatch(
            "the manifest is for another bundle than the record names".into(),
        ));
    }
    Ok(VerifiedPublication { served, manifest })
}

/// Step 4: the manifest's publisher is a signer this node already trusts.
/// Scope and the rest of admission are left to the install's own preflight.
pub(crate) fn check_publisher_pinned(
    trust: &TrustStore,
    manifest: &PublicationManifest,
) -> Result<(), PullRefusal> {
    let fingerprint = manifest
        .publisher_fingerprint
        .strip_prefix("fp:")
        .unwrap_or(&manifest.publisher_fingerprint);
    if trust
        .signers()
        .any(|signer| signer.fingerprint == fingerprint)
    {
        Ok(())
    } else {
        Err(PullRefusal::PublisherUnpinned {
            fingerprint: fingerprint.to_owned(),
        })
    }
}

/// The manifest as the export shape the shared limits and materialisation
/// helpers take.
pub(crate) fn export_response(manifest: &PublicationManifest) -> BundleExportResponse {
    BundleExportResponse {
        bundle_name: manifest.bundle_name.clone(),
        file_count: manifest.entries.len(),
        total_bytes: manifest.total_bytes,
        entries: manifest
            .entries
            .iter()
            .map(|entry| BundleExportEntry {
                kind: super::bundle_export::EXPORTED_BUNDLE_ENTRY_KIND_FILE.to_owned(),
                path: entry.path.clone(),
                hash: entry.hash.clone(),
                size: entry.size,
                mode: entry.mode,
            })
            .collect(),
    }
}

// ── The public fetch ────────────────────────────────────────────────

/// A plain HTTP client for public reads. It carries no identity: nothing here
/// signs a request, stores a cookie or attaches a key, and a redirect is an
/// error rather than a second request.
pub(crate) struct PublicHttp {
    client: reqwest::Client,
}

impl PublicHttp {
    pub(crate) fn new() -> Result<Self, PullRefusal> {
        let client = ryeos_http_egress::client_builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(REQUEST_TIMEOUT)
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|error| PullRefusal::FetchFailed(format!("build the HTTP client: {error}")))?;
        Ok(Self { client })
    }

    async fn send(&self, url: &str) -> Result<reqwest::Response, PullRefusal> {
        let response = self
            .client
            .get(url)
            .send()
            .await
            .map_err(|error| PullRefusal::FetchFailed(format!("GET {url}: {error}")))?;
        let status = response.status();
        if status.is_success() {
            return Ok(response);
        }
        // The node answers refusals as typed JSON; carry its code, not its text.
        let code = match read_bounded(response, ERROR_BODY_MAX_BYTES).await {
            Ok(Some(bytes)) => serde_json::from_slice::<Value>(&bytes)
                .ok()
                .and_then(|value| value["error"]["code"].as_str().map(str::to_owned)),
            _ => None,
        };
        Err(PullRefusal::FetchFailed(format!(
            "GET {url} answered {}{}",
            status.as_u16(),
            code.map(|code| format!(" ({code})")).unwrap_or_default()
        )))
    }

    /// GET `url` and return its body, or `None` when it is longer than
    /// `max_bytes`.
    pub(crate) async fn get_bounded(
        &self,
        url: &str,
        max_bytes: u64,
    ) -> Result<Option<Vec<u8>>, PullRefusal> {
        let response = self.send(url).await?;
        read_bounded(response, max_bytes)
            .await
            .map_err(|error| PullRefusal::FetchFailed(format!("GET {url}: {error}")))
    }

    async fn get_json(&self, url: &str, max_bytes: u64) -> Result<Value, PullRefusal> {
        let bytes = self.get_bounded(url, max_bytes).await?.ok_or_else(|| {
            PullRefusal::FetchFailed(format!("GET {url} answered more than {max_bytes} bytes"))
        })?;
        serde_json::from_slice(&bytes).map_err(|error| {
            PullRefusal::Malformed(format!("GET {url} answered non-JSON: {error}"))
        })
    }
}

/// Read a response body, stopping with `None` once it exceeds `max_bytes`.
async fn read_bounded(
    mut response: reqwest::Response,
    max_bytes: u64,
) -> Result<Option<Vec<u8>>, reqwest::Error> {
    if response.content_length().is_some_and(|len| len > max_bytes) {
        return Ok(None);
    }
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        if (body.len() as u64).saturating_add(chunk.len() as u64) > max_bytes {
            return Ok(None);
        }
        body.extend_from_slice(&chunk);
    }
    Ok(Some(body))
}

/// A published bundle's blobs, one GET each, every read bounded by the size
/// the verified manifest declares for it.
struct PublicBlobSource {
    http: PublicHttp,
    origin: String,
    id: String,
    sizes: HashMap<String, u64>,
}

impl PublicBlobSource {
    fn blob_url(&self, hash: &str) -> Result<(String, u64), PullRefusal> {
        let size = *self.sizes.get(hash).ok_or_else(|| {
            PullRefusal::BlobMismatch(format!("blob {hash} is not in the manifest"))
        })?;
        Ok((
            format!("{}/published/{}/blob/{hash}", self.origin, self.id),
            size,
        ))
    }
}

impl BlobSource for PublicBlobSource {
    async fn fetch_batch(&self, hashes: &[String]) -> anyhow::Result<HashMap<String, Vec<u8>>> {
        let mut blobs = HashMap::with_capacity(hashes.len());
        for hash in hashes {
            let (url, size) = self.blob_url(hash)?;
            let bytes = self.http.get_bounded(&url, size).await?.ok_or_else(|| {
                PullRefusal::BlobMismatch(format!(
                    "blob {hash} is longer than the {size} bytes its manifest entry declares"
                ))
            })?;
            blobs.insert(hash.clone(), bytes);
        }
        Ok(blobs)
    }

    async fn stream_blob(&self, hash: &str, sink: &mut std::fs::File) -> anyhow::Result<u64> {
        use std::io::Write as _;
        let (url, size) = self.blob_url(hash)?;
        let mut response = self.http.send(&url).await?;
        let mut hasher = Sha256::new();
        let mut written = 0_u64;
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|error| PullRefusal::FetchFailed(format!("GET {url}: {error}")))?
        {
            written = written.saturating_add(chunk.len() as u64);
            if written > size {
                return Err(PullRefusal::BlobMismatch(format!(
                    "blob {hash} is longer than the {size} bytes its manifest entry declares"
                ))
                .into());
            }
            hasher.update(&chunk);
            sink.write_all(&chunk).context("write the streamed blob")?;
        }
        let actual: String = hasher
            .finalize()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        if actual != hash {
            return Err(PullRefusal::BlobMismatch(format!(
                "blob content hash mismatch: expected {hash}, found {actual}"
            ))
            .into());
        }
        Ok(written)
    }
}

// ── The service ─────────────────────────────────────────────────────

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    /// `https://<host>/published/<id>`.
    pub url: String,
    /// The node key fingerprint to trust, `fp:<hex>`.
    pub node: String,
    /// The publication record hash to expect.
    pub expect: String,
}

pub async fn handle(
    req: Request,
    ctx: HandlerContext,
    state: Arc<AppState>,
) -> HandlerResult<Value> {
    super::publication::operator_caller(&ctx, &state)?;
    let refuse = PullRefusal::into_handler_error;

    let pins = Pins::parse(&req.node, &req.expect).map_err(refuse)?;
    let target = parse_target(&req.url).map_err(refuse)?;
    let http = PublicHttp::new().map_err(refuse)?;

    // 1. The node at the URL is the pinned node, on a compatible build.
    let key_doc: PublicKeyDoc = serde_json::from_value(
        http.get_json(
            &format!("{}/public-key", target.origin),
            PUBLIC_KEY_MAX_BYTES,
        )
        .await
        .map_err(refuse)?,
    )
    .map_err(|error| PullRefusal::Malformed(format!("the node's /public-key answer: {error}")))
    .map_err(refuse)?;
    let node_key =
        check_public_key(&key_doc, &pins.node, state.daemon_build.version).map_err(refuse)?;

    // 2 and 3. The record, its countersignature and its manifest.
    let described = http
        .get_json(
            &format!("{}/published/{}", target.origin, target.id),
            DESCRIBE_MAX_BYTES,
        )
        .await
        .map_err(refuse)?;
    let verified = verify_publication(&described, &node_key, &pins, lillux::time::unix_secs_now())
        .map_err(refuse)?;
    if verified.served.record.id != target.id {
        return Err(refuse(PullRefusal::Malformed(
            "the record's id is not the id in the URL".into(),
        )));
    }
    let bundle_name = verified.manifest.bundle_name.clone();
    super::bundle_install::validate_name(&bundle_name)
        .map_err(|error| refuse(PullRefusal::Malformed(format!("{error:#}"))))?;

    // 4. Trust is the node's own: refuse before any blob moves.
    check_publisher_pinned(&state.engine.node_trust_store, &verified.manifest).map_err(refuse)?;
    check_bundle_authorized(&state, &bundle_name).map_err(refuse)?;
    let response = export_response(&verified.manifest);
    validate_export_response(&response, &bundle_name)
        .map_err(|error| refuse(PullRefusal::BlobMismatch(format!("{error:#}"))))?;
    if response.entries.is_empty() {
        return Err(refuse(PullRefusal::ManifestMismatch(
            "the manifest has no files".into(),
        )));
    }
    if let Some(report) = precheck_install(&bundle_name, &state).map_err(classify)? {
        return Ok(report);
    }

    // Fetch every blob, hash it against its manifest entry, and install.
    let source = PublicBlobSource {
        http,
        origin: target.origin.clone(),
        id: target.id.clone(),
        sizes: response
            .entries
            .iter()
            .map(|entry| (entry.hash.clone(), entry.size))
            .collect(),
    };
    let mut installed = install_entries(
        &state,
        &ctx.fingerprint,
        &bundle_name,
        &source,
        &response.entries,
        &format!("publication:{}", target.id),
    )
    .await
    .map_err(classify)?;
    if let Some(object) = installed.as_object_mut() {
        object.insert("publication_id".to_owned(), json!(target.id));
        object.insert("record_hash".to_owned(), json!(pins.expect));
        object.insert("node_fingerprint".to_owned(), json!(pins.node));
    }
    Ok(installed)
}

pub const DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:bundle/pull",
    endpoint: "bundle.pull",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &["ryeos.execute.service.bundle/pull"],
    handler: |params, ctx, state| {
        Box::pin(async move {
            let req: Request = crate::handler_error::parse_request(params)?;
            handle(req, ctx, state).await.map_err(Into::into)
        })
    },
};

#[cfg(test)]
mod tests;

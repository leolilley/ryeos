//! Origin-side driver that pushes one exact published product to a hosted
//! node.
//!
//! The origin reads its own authority read-only: the witness must be this
//! operator's current product publication and the named admission must be this
//! node's accepted admission of that witness. The closure is offered to the
//! hosted node, which answers with exactly what it still lacks; the driver
//! then streams missing blobs and large objects in bounded chunks and finally
//! asks the hosted node to accept. Every call carries the staging capability
//! the hosted node issued, so a repeated push resumes rather than restarts.

use std::collections::BTreeMap;
use std::future::Future;
use std::io::SeekFrom;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context as _, Result, bail};
use ryeos_app::handler_context::HandlerContext;
use ryeos_app::operator_external_content::product_receipt::{
    ProductSourceVerification, load_product_source,
};
use ryeos_app::state::AppState;
use ryeos_node_runtime::node_policy::sections::object_closure::NodeObjectClosurePolicy;
use ryeos_state::external_content::products::transfer::ProductWitnessSource;
use ryeos_state::{AdmissionAttestationRecord, AdmissionAttestationState, LargeObjectStore};
use serde::Serialize;
use serde_json::Value;

use crate::handlers::external_content_product_offer::MAX_OFFERED_OBJECTS;
use crate::handlers::objects_get::MAX_BLOB_CHUNK_BYTES;
use crate::remote::client::{
    ProductAcceptRequest, ProductLargeObjectOffer, ProductOfferRequest, ProductOfferResponse,
    RemoteClient, StageChunkRequest, StageNamespace,
};

/// Wire chunk length for blobs and large objects.
const WIRE_CHUNK_BYTES: u64 = MAX_BLOB_CHUNK_BYTES as u64;
/// Retries of one step after a connection failure, all with the same staging
/// capability.
const STEP_RETRIES: u32 = 3;
const RETRY_BACKOFF: Duration = Duration::from_millis(250);
/// Canonical object bytes carried by one offer call, so a closure of large
/// canonical objects is spread across calls as well as bounded by count.
const OFFER_BATCH_BYTES: usize = 1024 * 1024;
/// Rounds in which the hosted node reports unchanged pending work after a
/// transfer round before the driver gives up.
const STALL_LIMIT: u32 = 3;
/// Restarts allowed after the hosted node refuses a chunk (for example after
/// its maintenance sweep removed a partial staging file).
const REFUSED_CHUNK_RESTARTS: u32 = 3;

#[derive(Debug, Clone)]
pub struct ProductPushRequest {
    pub remote: String,
    pub witness_hash: String,
    pub origin_admission_hash: String,
    pub maximum_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ProductPushOutcome {
    pub remote: String,
    pub staging_id: String,
    pub witness_hash: String,
    pub origin_admission_hash: String,
    pub acceptance_hash: String,
    pub reused_existing: bool,
    pub manifest_hash: String,
    pub large_objects_pushed: u64,
    /// Chunk payload bytes sent in this call, blobs and large objects.
    pub bytes_pushed: u64,
}

/// Shape a manifest declares for one large object.
#[derive(Debug, Clone, PartialEq, Eq)]
struct LargeDeclared {
    size: u64,
    chunk_size: u64,
    manifest_hash: String,
}

/// Everything read from local authority before any network call.
struct LocalProduct {
    admission_record: Value,
    /// Canonical objects of the closure keyed by hash.
    objects: BTreeMap<String, Value>,
    large: BTreeMap<String, LargeDeclared>,
}

pub async fn push_product(
    state: &Arc<AppState>,
    ctx: &HandlerContext,
    request: &ProductPushRequest,
) -> Result<ProductPushOutcome> {
    let remotes = crate::remote::config::load_remotes_layered(&state.config.app_root, None)?;
    let remote = crate::remote::config::get_remote(&remotes, &request.remote)?;
    let client = RemoteClient::from_remote_cfg_as_configured_operator(state, &remote, ctx)?;

    let local = {
        let state = Arc::clone(state);
        let owner = ctx.fingerprint.clone();
        let request = request.clone();
        tokio::task::spawn_blocking(move || gather_local_product(&state, &owner, &request))
            .await
            .context("product push local read task stopped")??
    };
    tracing::info!(
        remote = %request.remote,
        witness = %request.witness_hash,
        objects = local.objects.len(),
        large_objects = local.large.len(),
        "product push: closure collected"
    );

    let mut driver = Driver {
        state,
        client: &client,
        request,
        local: &local,
        staging_id: None,
        large_objects_pushed: 0,
        bytes_pushed: 0,
    };
    driver.run().await
}

struct Driver<'a> {
    state: &'a Arc<AppState>,
    client: &'a RemoteClient,
    request: &'a ProductPushRequest,
    local: &'a LocalProduct,
    staging_id: Option<String>,
    large_objects_pushed: u64,
    bytes_pushed: u64,
}

/// What the hosted node reported as pending, used to detect a transfer round
/// that changed nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Pending {
    objects: Vec<String>,
    blobs: Vec<String>,
    large: Vec<(String, u64)>,
}

impl Pending {
    fn of(response: &ProductOfferResponse) -> Self {
        Self {
            objects: response.missing_objects.clone(),
            blobs: response.missing_blobs.clone(),
            large: response
                .large_objects
                .iter()
                .filter(|large| !large.resident)
                .map(|large| (large.file_sha256.clone(), large.staged_bytes))
                .collect(),
        }
    }
}

impl Driver<'_> {
    async fn run(&mut self) -> Result<ProductPushOutcome> {
        let mut batch = self.first_batch();
        let mut previous: Option<Pending> = None;
        let mut stalls = 0_u32;
        let mut restarts = 0_u32;
        loop {
            let response = self.offer(&batch).await?;
            batch = Vec::new();
            let pending = Pending::of(&response);
            if !response.missing_objects.is_empty() {
                let next = self.select_objects(&response.missing_objects)?;
                if let Some(prior) = &previous
                    && prior.objects == pending.objects
                {
                    bail!(
                        "{}",
                        self.failure_text(
                            "offer",
                            "hosted node still lacks objects it was just sent"
                        )
                    );
                }
                previous = Some(pending);
                batch = next;
                continue;
            }
            if response.ready {
                break;
            }
            if previous.as_ref() == Some(&pending) {
                stalls += 1;
                if stalls >= STALL_LIMIT {
                    bail!(
                        "{}",
                        self.failure_text(
                            "transfer",
                            "hosted node reports unchanged pending blobs or large objects after complete transfers"
                        )
                    );
                }
            } else {
                stalls = 0;
            }
            previous = Some(pending);
            match self.transfer_pending(&response).await {
                Ok(()) => {}
                Err(StepFailure::Refused(error)) if restarts < REFUSED_CHUNK_RESTARTS => {
                    restarts += 1;
                    tracing::warn!(
                        remote = %self.request.remote,
                        staging_id = ?self.staging_id,
                        restart = restarts,
                        error = %format!("{error:#}"),
                        "product push: chunk refused; re-reading hosted progress"
                    );
                }
                Err(failure) => return Err(self.fail("chunk transfer", failure)),
            }
        }

        let staging_id = self.staging_id()?.to_owned();
        let accept = ProductAcceptRequest {
            staging_id: staging_id.clone(),
            witness_hash: self.request.witness_hash.clone(),
            origin_admission_hash: self.request.origin_admission_hash.clone(),
            maximum_bytes: self.request.maximum_bytes,
        };
        let accepted = retried("accept", || self.client.product_accept(&accept))
            .await
            .map_err(|failure| self.fail("accept", failure))?;
        tracing::info!(
            remote = %self.request.remote,
            staging_id = %staging_id,
            acceptance = %accepted.acceptance_hash,
            reused_existing = accepted.reused_existing,
            "product push: accepted"
        );
        Ok(ProductPushOutcome {
            remote: self.request.remote.clone(),
            staging_id,
            witness_hash: accepted.witness_hash,
            origin_admission_hash: accepted.origin_admission_hash,
            acceptance_hash: accepted.acceptance_hash,
            reused_existing: accepted.reused_existing,
            manifest_hash: accepted.manifest_hash,
            large_objects_pushed: self.large_objects_pushed,
            bytes_pushed: self.bytes_pushed,
        })
    }

    fn staging_id(&self) -> Result<&str> {
        self.staging_id
            .as_deref()
            .context("hosted node issued no staging capability")
    }

    fn failure_text(&self, step: &str, detail: &str) -> String {
        failure_message(
            self.staging_id.as_deref(),
            &self.request.remote,
            step,
            detail,
        )
    }

    fn fail(&self, step: &str, failure: StepFailure) -> anyhow::Error {
        let (kind, error) = match failure {
            StepFailure::Transport(error) => ("connection failed", error),
            StepFailure::Refused(error) => ("refused", error),
        };
        anyhow::anyhow!("{}: {error:#}", self.failure_text(step, kind))
    }

    /// The first offer carries the witness object (so the hosted node can walk
    /// the closure) followed by as many further objects as one call allows.
    fn first_batch(&self) -> Vec<String> {
        let mut hashes = vec![self.request.witness_hash.clone()];
        hashes.extend(
            self.local
                .objects
                .keys()
                .filter(|hash| **hash != self.request.witness_hash)
                .cloned(),
        );
        batch_hashes(&self.local.objects, &hashes)
    }

    /// Resolve hashes the hosted node asked for into a bounded next batch.
    fn select_objects(&self, wanted: &[String]) -> Result<Vec<String>> {
        for hash in wanted {
            if !self.local.objects.contains_key(hash) {
                bail!(
                    "{}",
                    self.failure_text(
                        "offer",
                        &format!(
                            "hosted node requires object {hash} that is not in the local closure"
                        )
                    )
                );
            }
        }
        Ok(batch_hashes(&self.local.objects, wanted))
    }

    async fn offer(&mut self, hashes: &[String]) -> Result<ProductOfferResponse> {
        let request = ProductOfferRequest {
            witness_hash: self.request.witness_hash.clone(),
            origin_admission_hash: self.request.origin_admission_hash.clone(),
            maximum_bytes: self.request.maximum_bytes,
            origin_admission: self.local.admission_record.clone(),
            objects: hashes
                .iter()
                .map(|hash| self.local.objects[hash].clone())
                .collect(),
            staging_id: self.staging_id.clone(),
        };
        let response = retried("offer", || self.client.product_offer(&request))
            .await
            .map_err(|failure| self.fail("offer", failure))?;
        self.staging_id = Some(response.staging_id.clone());
        tracing::info!(
            remote = %self.request.remote,
            staging_id = %response.staging_id,
            sent_objects = hashes.len(),
            missing_objects = response.missing_objects.len(),
            missing_blobs = response.missing_blobs.len(),
            pending_large_objects = response.large_objects.iter().filter(|l| !l.resident).count(),
            ready = response.ready,
            "product push: offer"
        );
        Ok(response)
    }

    async fn transfer_pending(
        &mut self,
        response: &ProductOfferResponse,
    ) -> Result<(), StepFailure> {
        for hash in &response.missing_blobs {
            self.stream_blob(hash).await?;
        }
        for large in response
            .large_objects
            .iter()
            .filter(|large| !large.resident)
        {
            self.stream_large_object(large).await?;
        }
        Ok(())
    }

    async fn stream_blob(&mut self, hash: &str) -> Result<(), StepFailure> {
        let staging_id = self.staging_id().map_err(StepFailure::Refused)?.to_owned();
        let total = {
            let state = Arc::clone(self.state);
            let hash = hash.to_owned();
            blocking(move || blob_size(&state, &hash))
                .await
                .map_err(StepFailure::Refused)?
        };
        tracing::info!(
            remote = %self.request.remote,
            staging_id = %staging_id,
            blob = %hash,
            bytes = total,
            "product push: blob"
        );
        // The hosted blob path reports only the end of each accepted chunk, so
        // a blob always walks from its start; replaying a completed chunk is
        // accepted there.
        let mut offset = 0_u64;
        while offset < total {
            let length = plan_next_chunk(total, offset)
                .map_err(StepFailure::Refused)?
                .1;
            let data = {
                let state = Arc::clone(self.state);
                let hash = hash.to_owned();
                blocking(move || read_blob_range(&state, &hash, offset, length))
                    .await
                    .map_err(StepFailure::Refused)?
            };
            let chunk = StageChunkRequest {
                staging_id: &staging_id,
                namespace: StageNamespace::Blob,
                hash,
                total_size: total,
                offset,
                data: &data,
                manifest_hash: None,
            };
            let answer = retried("blob chunk", || self.client.objects_stage_chunk(&chunk)).await?;
            self.bytes_pushed +=
                u64::try_from(data.len()).map_err(|e| StepFailure::Refused(e.into()))?;
            offset = answer.staged_bytes;
            if answer.complete {
                break;
            }
        }
        Ok(())
    }

    async fn stream_large_object(
        &mut self,
        large: &ProductLargeObjectOffer,
    ) -> Result<(), StepFailure> {
        let staging_id = self.staging_id().map_err(StepFailure::Refused)?.to_owned();
        let local = self.local;
        let declared = local
            .large
            .get(&large.file_sha256)
            .with_context(|| {
                format!(
                    "hosted node asked for large object {} that no local manifest declares",
                    large.file_sha256
                )
            })
            .map_err(StepFailure::Refused)?;
        if declared.size != large.size || declared.chunk_size != large.chunk_size {
            return Err(StepFailure::Refused(anyhow::anyhow!(
                "hosted node describes large object {} as {} bytes in {} byte chunks; the local manifest {} seals {} bytes in {} byte chunks",
                large.file_sha256,
                large.size,
                large.chunk_size,
                declared.manifest_hash,
                declared.size,
                declared.chunk_size
            )));
        }
        tracing::info!(
            remote = %self.request.remote,
            staging_id = %staging_id,
            large_object = %large.file_sha256,
            bytes = large.size,
            resume_at = large.staged_bytes,
            "product push: large object"
        );
        let mut offset = large_object_start_offset(large.size, large.staged_bytes, large.resident);
        let mut sent = 0_u64;
        while offset < large.size {
            let length = plan_next_chunk(large.size, offset)
                .map_err(StepFailure::Refused)?
                .1;
            let data = {
                let state = Arc::clone(self.state);
                let hash = large.file_sha256.clone();
                let size = large.size;
                blocking(move || read_large_range(&state, &hash, size, offset, length))
                    .await
                    .map_err(StepFailure::Refused)?
            };
            let chunk = StageChunkRequest {
                staging_id: &staging_id,
                namespace: StageNamespace::LargeObject,
                hash: &large.file_sha256,
                total_size: large.size,
                offset,
                data: &data,
                manifest_hash: Some(&declared.manifest_hash),
            };
            let answer = retried("large-object chunk", || {
                self.client.objects_stage_chunk(&chunk)
            })
            .await?;
            let length = u64::try_from(data.len()).map_err(|e| StepFailure::Refused(e.into()))?;
            sent += length;
            self.bytes_pushed += length;
            offset = answer.staged_bytes;
            if answer.complete {
                break;
            }
        }
        if sent > 0 {
            self.large_objects_pushed += 1;
        }
        Ok(())
    }
}

/// Where a large-object upload starts. A hosted node that holds every byte in
/// staging but has not published the object (it stopped between the last append
/// and publication) reports `staged_bytes == size` with `resident == false`;
/// nothing would be sent and the push would stall, so the last chunk is
/// replayed, which the store treats as a completion retry.
fn large_object_start_offset(size: u64, staged_bytes: u64, resident: bool) -> u64 {
    if !resident && staged_bytes == size && size > 0 {
        size - size.min(WIRE_CHUNK_BYTES)
    } else {
        staged_bytes
    }
}

fn failure_message(staging_id: Option<&str>, remote: &str, step: &str, detail: &str) -> String {
    match staging_id {
        Some(staging_id) => format!(
            "product push to {remote}: {step}: {detail} (staging capability {staging_id}; push again to resume it)"
        ),
        None => {
            format!("product push to {remote}: {step}: {detail} (no staging capability issued yet)")
        }
    }
}

/// Failure of one remote step. Only a connection failure that outlasted its
/// retries is `Transport`; any answer from the hosted node that is an error is
/// `Refused` and is never retried blindly.
enum StepFailure {
    Transport(anyhow::Error),
    Refused(anyhow::Error),
}

async fn retried<T, F, Fut>(step: &str, mut call: F) -> Result<T, StepFailure>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T>>,
{
    let mut attempt = 0_u32;
    loop {
        match call().await {
            Ok(value) => return Ok(value),
            Err(error) if is_connection_failure(&error) => {
                if attempt >= STEP_RETRIES {
                    return Err(StepFailure::Transport(error.context(format!(
                        "{step} still failing after {STEP_RETRIES} retries"
                    ))));
                }
                attempt += 1;
                tracing::warn!(
                    step,
                    attempt,
                    error = %format!("{error:#}"),
                    "product push: connection failure; retrying the step"
                );
                tokio::time::sleep(RETRY_BACKOFF * attempt).await;
            }
            Err(error) => return Err(StepFailure::Refused(error)),
        }
    }
}

fn is_connection_failure(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause
            .downcast_ref::<reqwest::Error>()
            .is_some_and(|inner| inner.is_connect() || inner.is_timeout() || inner.is_request())
    })
}

async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> Result<T> + Send + 'static,
) -> Result<T> {
    tokio::task::spawn_blocking(work)
        .await
        .context("product push local read task stopped")?
}

/// Pick hashes, in order, until the per-call object count or canonical byte
/// budget is reached. A single object over the byte budget travels alone.
fn batch_hashes(objects: &BTreeMap<String, Value>, hashes: &[String]) -> Vec<String> {
    let mut chosen = Vec::new();
    let mut bytes = 0_usize;
    for hash in hashes {
        if chosen.len() >= MAX_OFFERED_OBJECTS {
            break;
        }
        let size = objects
            .get(hash)
            .map(|value| value.to_string().len())
            .unwrap_or(0);
        if !chosen.is_empty() && bytes.saturating_add(size) > OFFER_BATCH_BYTES {
            break;
        }
        bytes = bytes.saturating_add(size);
        chosen.push(hash.clone());
    }
    chosen
}

/// Next chunk to send for an object of `total` bytes when `offset` bytes are
/// already staged: its offset and length, bounded by the wire chunk size.
fn plan_next_chunk(total: u64, offset: u64) -> Result<(u64, u64)> {
    if offset >= total {
        bail!("no chunk remains: offset {offset} is not before the {total} byte end");
    }
    Ok((offset, WIRE_CHUNK_BYTES.min(total - offset)))
}

/// Every chunk `(offset, length)` from `start` to the end of an object.
#[cfg(test)]
fn plan_chunks(total: u64, start: u64) -> Result<Vec<(u64, u64)>> {
    if start > total {
        bail!("resume offset {start} is past the {total} byte end");
    }
    let mut chunks = Vec::new();
    let mut offset = start;
    while offset < total {
        let (at, length) = plan_next_chunk(total, offset)?;
        chunks.push((at, length));
        offset = at + length;
    }
    Ok(chunks)
}

fn blob_size(state: &AppState, hash: &str) -> Result<u64> {
    let read = state.acquire_cas_read()?;
    let (_file, size) = read
        .cas()
        .open_blob(hash)?
        .with_context(|| format!("local CAS blob {hash} is absent; it cannot be pushed"))?;
    Ok(size)
}

fn read_blob_range(state: &AppState, hash: &str, offset: u64, length: u64) -> Result<Vec<u8>> {
    let read = state.acquire_cas_read()?;
    let (mut file, size) = read
        .cas()
        .open_blob(hash)?
        .with_context(|| format!("local CAS blob {hash} disappeared during the push"))?;
    read_range(&mut file, size, offset, length, hash)
}

fn read_large_range(
    state: &AppState,
    hash: &str,
    size: u64,
    offset: u64,
    length: u64,
) -> Result<Vec<u8>> {
    let authority = state.state_store.pinned_state_authority()?;
    let store = LargeObjectStore::open_under(authority.runtime_directory())?
        .context("local large-object store does not exist")?;
    let leased = store
        .lease_object(hash, size)
        .with_context(|| format!("local large object {hash} cannot be leased for pushing"))?;
    let mut file = leased.file();
    read_range(&mut file, size, offset, length, hash)
}

fn read_range<F: std::io::Read + std::io::Seek>(
    file: &mut F,
    total: u64,
    offset: u64,
    length: u64,
    name: &str,
) -> Result<Vec<u8>> {
    let end = offset
        .checked_add(length)
        .context("chunk range overflows")?;
    if end > total {
        bail!("chunk {offset}..{end} is past the {total} byte end of {name}");
    }
    file.seek(SeekFrom::Start(offset))?;
    let mut bytes = vec![0_u8; usize::try_from(length)?];
    file.read_exact(&mut bytes)
        .with_context(|| format!("read {name} range {offset}..{end}"))?;
    Ok(bytes)
}

fn gather_local_product(
    state: &AppState,
    owner: &str,
    request: &ProductPushRequest,
) -> Result<LocalProduct> {
    let limits = state
        .node_policy
        .require::<NodeObjectClosurePolicy>()?
        .closure_limits()?;
    let authority = state.state_store.pinned_state_authority()?;
    let guard = authority.acquire_shared_guard()?;
    let cas = authority.cas_store()?;

    // The witness must be this operator's current published product; the
    // source loader refuses an absent, foreign, or superseded witness.
    load_product_source(
        state,
        &authority,
        &guard,
        limits,
        owner,
        &request.witness_hash,
        &ProductWitnessSource::LocalCapture {},
        ProductSourceVerification::Fresh,
    )
    .with_context(|| {
        format!(
            "witness {} is not this operator's current product publication",
            request.witness_hash
        )
    })?;

    let local_issuer = format!("fp:{}", state.identity.fingerprint());
    let records = state.state_store.with_state_db(|db| {
        db.list_admission_attestations_for_subject(
            &request.witness_hash,
            Some(ryeos_state::admission::LOCAL_ADMISSION_POLICY),
        )
    })?;
    let stored_subject = match cas.get_object(&request.origin_admission_hash)? {
        Some(value) => ryeos_state::Attestation::from_value(&value)
            .ok()
            .map(|attestation| attestation.subject_hash),
        None => None,
    };
    let record = require_admission_record(
        &records,
        &request.origin_admission_hash,
        &request.witness_hash,
        &local_issuer,
        stored_subject.as_deref(),
    )?;
    let attestation_value = cas.get_object(&record.attestation_hash)?.with_context(|| {
        format!(
            "origin admission {} is indexed but its object is absent",
            record.attestation_hash
        )
    })?;
    let attestation = ryeos_state::Attestation::from_value(&attestation_value)?;
    attestation
        .verify_with_key(state.identity.verifying_key())
        .context("origin admission is not signed by this node")?;
    let admission_record = serde_json::json!({
        "attestation_hash": record.attestation_hash,
        "subject_hash": record.subject_hash,
        "policy": record.policy,
        "claim": record.claim,
        "issuer": record.issuer,
        "issued_at": record.issued_at,
        "expires_at": record.expires_at,
        "head_ref_path": record.head_ref_path,
        "indexed_at": record.indexed_at,
        "state": record.state.as_str(),
        "attestation": attestation_value,
    });

    let report = ryeos_state::object_closure::collect_object_closure_with_cas_and_limits(
        &cas,
        [request.witness_hash.clone()],
        limits,
    )?;
    if !report.is_complete() {
        bail!(
            "local closure of witness {} is incomplete: {} missing objects, {} missing blobs, {} malformed, {} unsupported",
            request.witness_hash,
            report.missing_objects.len(),
            report.missing_blobs.len(),
            report.malformed_objects.len(),
            report.unsupported_objects.len()
        );
    }
    let mut objects = BTreeMap::new();
    for hash in &report.object_hashes {
        let value = cas.get_object(hash)?.with_context(|| {
            format!("local CAS object {hash} disappeared while reading the closure")
        })?;
        objects.insert(hash.clone(), value);
    }
    let large = declared_large_objects(&objects)?;
    for hash in &report.large_object_hashes {
        if !large.contains_key(hash) {
            bail!("closure names large object {hash} that no present manifest declares");
        }
    }
    let store = LargeObjectStore::open_under(authority.runtime_directory())?;
    require_large_objects_present(store.as_ref(), &large)?;
    Ok(LocalProduct {
        admission_record,
        objects,
        large,
    })
}

/// Select the accepted local admission that names `witness_hash`. `stored_subject`
/// is the subject of whatever attestation object sits at the admission hash,
/// when one exists, so a wrong-witness admission is told apart from an absent one.
fn require_admission_record<'a>(
    records: &'a [AdmissionAttestationRecord],
    admission_hash: &str,
    witness_hash: &str,
    local_issuer: &str,
    stored_subject: Option<&str>,
) -> Result<&'a AdmissionAttestationRecord> {
    let Some(record) = records
        .iter()
        .find(|record| record.attestation_hash == admission_hash)
    else {
        return match stored_subject {
            Some(subject) if subject != witness_hash => bail!(
                "origin admission {admission_hash} names witness {subject}, not {witness_hash}"
            ),
            _ => bail!(
                "origin admission {admission_hash} is absent from this node's admissions of witness {witness_hash}"
            ),
        };
    };
    if record.subject_hash != witness_hash {
        bail!(
            "origin admission {admission_hash} names witness {}, not {witness_hash}",
            record.subject_hash
        );
    }
    if !matches!(record.state, AdmissionAttestationState::Accepted) || record.claim != "accepted" {
        bail!("origin admission {admission_hash} is not an accepted admission");
    }
    if record.issuer != local_issuer {
        bail!(
            "origin admission {admission_hash} was issued by {}, not by this node ({local_issuer})",
            record.issuer
        );
    }
    Ok(record)
}

/// Large objects declared by the closure's large-content manifests, with the
/// manifest that carries each.
fn declared_large_objects(
    objects: &BTreeMap<String, Value>,
) -> Result<BTreeMap<String, LargeDeclared>> {
    let mut declared = BTreeMap::<String, LargeDeclared>::new();
    for (hash, value) in objects {
        if value.get("kind").and_then(Value::as_str)
            != Some(ryeos_state::objects::EXTERNAL_LARGE_CONTENT_MANIFEST_KIND)
        {
            continue;
        }
        let manifest = ryeos_state::objects::ExternalLargeContentManifestObject::from_value(value)
            .with_context(|| format!("large-content manifest {hash} is invalid"))?;
        for entry in &manifest.entries {
            let (Some(file_sha256), Some(size), Some(chunk_size)) =
                (&entry.file_sha256, entry.size, entry.chunk_size)
            else {
                continue;
            };
            let shape = LargeDeclared {
                size,
                chunk_size,
                manifest_hash: hash.clone(),
            };
            if let Some(prior) = declared.get(file_sha256) {
                if (prior.size, prior.chunk_size) != (size, chunk_size) {
                    bail!(
                        "manifests {} and {hash} declare conflicting shapes for large object {file_sha256}",
                        prior.manifest_hash
                    );
                }
                continue;
            }
            declared.insert(file_sha256.clone(), shape);
        }
    }
    Ok(declared)
}

/// Refuse a push whose declared large objects are not all present locally at
/// their sealed size.
fn require_large_objects_present(
    store: Option<&LargeObjectStore>,
    declared: &BTreeMap<String, LargeDeclared>,
) -> Result<()> {
    if declared.is_empty() {
        return Ok(());
    }
    let store =
        store.context("closure declares large objects but this node has no large-object store")?;
    for (hash, shape) in declared {
        match store.object_size(hash)? {
            Some(size) if size == shape.size => {}
            Some(size) => bail!(
                "local large object {hash} is {size} bytes; manifest {} seals {}",
                shape.manifest_hash,
                shape.size
            ),
            None => bail!(
                "local large-object store lacks {hash} ({} bytes) declared by manifest {}",
                shape.size,
                shape.manifest_hash
            ),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunk_plan_covers_the_object_in_bounded_pieces() {
        let plan = plan_chunks(WIRE_CHUNK_BYTES * 2 + 10, 0).unwrap();
        assert_eq!(
            plan,
            vec![
                (0, WIRE_CHUNK_BYTES),
                (WIRE_CHUNK_BYTES, WIRE_CHUNK_BYTES),
                (WIRE_CHUNK_BYTES * 2, 10),
            ]
        );
        assert!(plan.iter().all(|(_, length)| *length <= WIRE_CHUNK_BYTES));
        assert_eq!(WIRE_CHUNK_BYTES, 512 * 1024);
    }

    #[test]
    fn fully_staged_unpublished_object_replays_its_last_chunk() {
        let size = WIRE_CHUNK_BYTES * 2 + 10;
        assert_eq!(
            large_object_start_offset(size, size, false),
            size - WIRE_CHUNK_BYTES
        );
        assert_eq!(large_object_start_offset(10, 10, false), 0);
        assert_eq!(large_object_start_offset(size, 100, false), 100);
        assert_eq!(large_object_start_offset(size, 0, false), 0);
    }

    #[test]
    fn chunk_plan_resumes_at_a_nonzero_offset() {
        let plan = plan_chunks(WIRE_CHUNK_BYTES * 2 + 10, 100).unwrap();
        assert_eq!(plan[0], (100, WIRE_CHUNK_BYTES));
        assert_eq!(plan[1], (100 + WIRE_CHUNK_BYTES, WIRE_CHUNK_BYTES));
        let (last_offset, last_length) = *plan.last().unwrap();
        assert_eq!(last_offset + last_length, WIRE_CHUNK_BYTES * 2 + 10);
    }

    #[test]
    fn chunk_plan_ends_exactly_and_refuses_past_the_end() {
        assert_eq!(plan_chunks(WIRE_CHUNK_BYTES, 0).unwrap().len(), 1);
        assert!(
            plan_chunks(WIRE_CHUNK_BYTES, WIRE_CHUNK_BYTES)
                .unwrap()
                .is_empty()
        );
        assert!(plan_chunks(10, 11).is_err());
        assert!(plan_next_chunk(10, 10).is_err());
        assert_eq!(plan_next_chunk(10, 4).unwrap(), (4, 6));
    }

    #[test]
    fn range_reads_stay_inside_the_object() {
        let mut file = std::io::Cursor::new((0_u8..100).collect::<Vec<_>>());
        assert_eq!(
            read_range(&mut file, 100, 90, 10, "x").unwrap(),
            (90_u8..100).collect::<Vec<_>>()
        );
        assert!(read_range(&mut file, 100, 95, 10, "x").is_err());
    }

    fn record(
        hash: &str,
        subject: &str,
        issuer: &str,
        state: AdmissionAttestationState,
    ) -> AdmissionAttestationRecord {
        AdmissionAttestationRecord {
            attestation_hash: hash.to_owned(),
            subject_hash: subject.to_owned(),
            policy: ryeos_state::admission::LOCAL_ADMISSION_POLICY.to_owned(),
            claim: "accepted".to_owned(),
            issuer: issuer.to_owned(),
            issued_at: "2026-10-22T00:00:00Z".to_owned(),
            expires_at: None,
            head_ref_path: None,
            indexed_at: "2026-10-22T00:00:01Z".to_owned(),
            state,
        }
    }

    #[test]
    fn admission_must_exist_name_the_witness_and_be_local_and_accepted() {
        let witness = "aa".repeat(32);
        let admission = "bb".repeat(32);
        let other = "cc".repeat(32);
        let issuer = "fp:local";
        let good = record(
            &admission,
            &witness,
            issuer,
            AdmissionAttestationState::Accepted,
        );
        assert!(
            require_admission_record(
                std::slice::from_ref(&good),
                &admission,
                &witness,
                issuer,
                None
            )
            .is_ok()
        );

        let absent = require_admission_record(&[], &admission, &witness, issuer, None)
            .unwrap_err()
            .to_string();
        assert!(absent.contains("is absent"), "{absent}");

        let wrong_witness =
            require_admission_record(&[], &admission, &witness, issuer, Some(other.as_str()))
                .unwrap_err()
                .to_string();
        assert!(wrong_witness.contains("names witness"), "{wrong_witness}");

        let rejected = record(
            &admission,
            &witness,
            issuer,
            AdmissionAttestationState::Rejected,
        );
        assert!(
            require_admission_record(&[rejected], &admission, &witness, issuer, None)
                .unwrap_err()
                .to_string()
                .contains("not an accepted")
        );

        let foreign = record(
            &admission,
            &witness,
            "fp:other",
            AdmissionAttestationState::Accepted,
        );
        assert!(
            require_admission_record(&[foreign], &admission, &witness, issuer, None)
                .unwrap_err()
                .to_string()
                .contains("not by this node")
        );
    }

    #[test]
    fn declared_large_objects_ignore_other_kinds() {
        let mut objects = BTreeMap::new();
        objects.insert(
            "dd".repeat(32),
            serde_json::json!({"kind": "something_else"}),
        );
        assert!(declared_large_objects(&objects).unwrap().is_empty());
    }

    #[test]
    fn absent_large_object_is_refused_precisely() {
        let dir = tempfile::tempdir().unwrap();
        let runtime = lillux::PinnedDirectory::open_or_create(dir.path()).unwrap();
        let store = LargeObjectStore::open_with_chunk_bytes(&runtime, 4).unwrap();
        let hash = "ee".repeat(32);
        let mut declared = BTreeMap::new();
        declared.insert(
            hash.clone(),
            LargeDeclared {
                size: 8,
                chunk_size: 4,
                manifest_hash: "dd".repeat(32),
            },
        );
        let error = require_large_objects_present(Some(&store), &declared)
            .unwrap_err()
            .to_string();
        assert!(error.contains("lacks"), "{error}");
        assert!(error.contains(&hash), "{error}");
        let no_store = require_large_objects_present(None, &declared)
            .unwrap_err()
            .to_string();
        assert!(no_store.contains("no large-object store"), "{no_store}");
        assert!(require_large_objects_present(None, &BTreeMap::new()).is_ok());
    }

    #[test]
    fn offer_batches_respect_count_and_byte_budgets() {
        let mut objects = BTreeMap::new();
        let mut hashes = Vec::new();
        for index in 0..(MAX_OFFERED_OBJECTS + 5) {
            let hash = format!("{index:064x}");
            objects.insert(hash.clone(), serde_json::json!({"n": index}));
            hashes.push(hash);
        }
        let batch = batch_hashes(&objects, &hashes);
        assert_eq!(batch.len(), MAX_OFFERED_OBJECTS);

        let big = "x".repeat(OFFER_BATCH_BYTES);
        let mut heavy = BTreeMap::new();
        let first = "01".repeat(32);
        let second = "02".repeat(32);
        heavy.insert(first.clone(), serde_json::json!({"p": big.clone()}));
        heavy.insert(second.clone(), serde_json::json!({"p": big}));
        let batch = batch_hashes(&heavy, &[first.clone(), second]);
        assert_eq!(batch, vec![first]);
    }

    #[test]
    fn failure_text_names_the_staging_capability() {
        let with = failure_message(Some("stage-1"), "origin", "offer", "connection failed");
        assert!(
            with.contains("stage-1") && with.contains("resume"),
            "{with}"
        );
        let without = failure_message(None, "origin", "offer", "refused");
        assert!(without.contains("no staging capability"), "{without}");
    }

    #[test]
    fn absent_witness_is_refused_before_anything_is_offered() {
        let root = tempfile::tempdir().unwrap();
        let (_owner, state) = ryeos_app::state::test_support::build(root.path()).unwrap();
        let request = ProductPushRequest {
            remote: "hosted".to_owned(),
            witness_hash: "ab".repeat(32),
            origin_admission_hash: "cd".repeat(32),
            maximum_bytes: 1024,
        };
        let owner = format!("fp:{}", "11".repeat(32));
        assert!(gather_local_product(&state, &owner, &request).is_err());
    }
}

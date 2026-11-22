//! The `art` response mode: `GET /ui/art/{sha256}` serves one signed picture.
//!
//! A picture is served only when all of these hold, in this order:
//!
//! 1. the caller has a live browser session (the route's `browser_session`
//!    auth) and the request is a GET;
//! 2. the path value is a lowercase hex SHA-256. It is only ever a map key:
//!    no request value is joined into a filesystem path;
//! 3. the hash is named by a compiled binding attached to the calling
//!    session, or is in that session's grant set. A hash only another
//!    session's binding names, or one that is merely installed, is not
//!    served;
//! 4. the hash is in the [`ArtIndex`] built from installed bundles whose
//!    signed picture manifest and signed picture records verify under the
//!    node trust store, and (for a binding-named hash) the signed record's
//!    media type equals the type the signed Surface or View declared;
//! 5. the blob read from the installed bundle has the recorded length and
//!    hashes to the requested value, its magic bytes match the declared type,
//!    it is not animated, and its dimensions and size are inside the caps.
//!
//! Every failure before step 5 answers the same 404, so the route is no
//! oracle for which hashes exist or which a session may not have. Content
//! type comes from the signed record, never from the bytes.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use axum::http::{HeaderName, HeaderValue, Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use ryeos_api::route_error::{RouteConfigError, RouteDispatchError};
use ryeos_api::routes::compile::{
    CompiledResponseMode, CompiledRoute, ResponseMode, RouteDispatchContext,
};
use ryeos_app::route_raw::RawRouteSpec;
use ryeos_bundle::pictures::{
    MAX_PICTURE_BYTES, PictureEntry, inspect_image, picture_manifest_ref_path,
    verify_bundle_pictures,
};
use ryeos_client_base::art::{
    ART_MEDIA_TYPES, ArtRef, PictureBinding, PictureBindingError, is_art_hash,
};
use ryeos_engine::engine::Engine;
use ryeos_engine::trust::TrustStore;

use crate::browser_session::BrowserSession;
use crate::state::UiState;

/// Key of the response mode in a route file.
pub const ART_MODE_KEY: &str = "art";
const REQUIRED_AUTH: &str = "browser_session";
const HASH_CAPTURE: &str = "sha256";

/// The installed pictures a session may be served, by content hash. Built
/// only from bundles whose picture manifest and records verify.
#[derive(Debug, Clone, Default)]
pub struct ArtIndex {
    /// Every bundle's copy of each hash, in bundle order.
    entries: BTreeMap<String, Vec<PictureEntry>>,
    /// Renditions of each signed record (`pictures/<id>.yaml`), in record
    /// order, taken from the first verifying bundle that ships the record.
    records: BTreeMap<String, Vec<PictureEntry>>,
}

impl ArtIndex {
    /// Index every verifying bundle. A bundle whose pictures fail
    /// verification contributes none of them; the failure is logged.
    pub fn build(bundle_roots: &[PathBuf], trust_store: &TrustStore) -> Self {
        let mut entries = BTreeMap::new();
        let mut records: BTreeMap<String, Vec<PictureEntry>> = BTreeMap::new();
        for root in bundle_roots {
            match verify_bundle_pictures(root, trust_store, false) {
                Ok(pictures) => {
                    let mut by_record: BTreeMap<String, Vec<PictureEntry>> = BTreeMap::new();
                    for entry in pictures.entries.values() {
                        by_record
                            .entry(entry.record.clone())
                            .or_default()
                            .push(entry.clone());
                    }
                    for (record, mut renditions) in by_record {
                        renditions.sort_by_key(|entry| entry.rendition_index);
                        records.entry(record).or_insert(renditions);
                    }
                    for (sha256, entry) in pictures.entries {
                        // Equal hashes are equal bytes; every bundle's copy is
                        // kept so a corrupt one can fall through to a good one.
                        entries.entry(sha256).or_insert_with(Vec::new).push(entry);
                    }
                }
                Err(error) => tracing::warn!(
                    bundle = %root.display(),
                    %error,
                    "bundle pictures failed verification; none of them are served"
                ),
            }
        }
        Self { entries, records }
    }

    /// Add one verified rendition, for tests that build an index without
    /// installed bundles.
    #[cfg(test)]
    pub(crate) fn insert_for_test(&mut self, entry: PictureEntry) {
        let records = self.records.entry(entry.record.clone()).or_default();
        records.push(entry.clone());
        records.sort_by_key(|entry| entry.rendition_index);
        self.entries
            .entry(entry.sha256.clone())
            .or_default()
            .push(entry);
    }

    /// Every installed copy of `sha256`, empty when none.
    pub fn candidates(&self, sha256: &str) -> &[PictureEntry] {
        self.entries.get(sha256).map(Vec::as_slice).unwrap_or(&[])
    }

    /// Resolve a picture binding to one verified rendition of the signed
    /// record it names. The ref is looked up only among the verified bundles'
    /// records, in bundle order; a project can never supply one. A failure is
    /// a refusal, never a substitute: the caller shows the picture as
    /// unavailable and consults no other tier.
    pub fn resolve_binding(
        &self,
        binding: &PictureBinding,
    ) -> Result<ResolvedPicture, PictureRefusal> {
        binding.validate().map_err(PictureRefusal::Malformed)?;
        let record = binding
            .record_path()
            .ok_or(PictureRefusal::Malformed(PictureBindingError::MalformedRef))?;
        let renditions = self
            .records
            .get(&record)
            .filter(|renditions| !renditions.is_empty())
            .ok_or(PictureRefusal::NotIndexed)?;
        let entry = match &binding.role {
            None => &renditions[0],
            Some(role) => renditions
                .iter()
                .find(|entry| entry.role == *role)
                .ok_or_else(|| PictureRefusal::UnknownRole(role.clone()))?,
        };
        let media_type = entry.media_type.mime();
        if !ART_MEDIA_TYPES.contains(&media_type) {
            return Err(PictureRefusal::MediaTypeNotAllowed);
        }
        Ok(ResolvedPicture {
            art: ArtRef {
                sha256: entry.sha256.clone(),
                media_type: media_type.to_owned(),
            },
            role: entry.role.clone(),
        })
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// One rendition a picture binding resolved to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedPicture {
    pub art: ArtRef,
    /// The rendition's role word.
    pub role: String,
}

/// Why a picture binding did not resolve. Every refusal leaves the subject
/// with `picture_state: unavailable`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PictureRefusal {
    /// The binding is malformed, or names an item that is not a picture.
    Malformed(PictureBindingError),
    /// No verified bundle ships the record, including a record whose bundle
    /// failed picture verification.
    NotIndexed,
    /// The record has no rendition with this role.
    UnknownRole(String),
    /// The rendition's media type is not one a picture may be served as.
    MediaTypeNotAllowed,
}

struct CachedIndex {
    engine: Arc<Engine>,
    /// The signed picture-manifest ref of every bundle root when the index
    /// was built; a changed ref rebuilds the index.
    refs: Vec<Option<Vec<u8>>>,
    index: Arc<ArtIndex>,
}

/// The index for the engine generation in force, rebuilt when the generation
/// or any bundle's picture-manifest ref changes.
#[derive(Default)]
pub struct ArtIndexCache {
    cached: Mutex<Option<CachedIndex>>,
}

impl ArtIndexCache {
    pub fn index_for(&self, engine: &Arc<Engine>) -> Arc<ArtIndex> {
        let refs: Vec<Option<Vec<u8>>> = engine
            .bundle_roots
            .iter()
            .map(|root| {
                lillux::read_regular_file_bounded_no_follow(
                    &picture_manifest_ref_path(&root.join(ryeos_engine::AI_DIR)),
                    ryeos_engine::executor_resolution::MAX_EXECUTOR_MANIFEST_REF_BYTES,
                )
                .ok()
            })
            .collect();
        let mut cached = self
            .cached
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(current) = cached.as_ref()
            && Arc::ptr_eq(&current.engine, engine)
            && current.refs == refs
        {
            return current.index.clone();
        }
        let index = Arc::new(ArtIndex::build(
            &engine.bundle_roots,
            &engine.node_trust_store,
        ));
        *cached = Some(CachedIndex {
            engine: engine.clone(),
            refs,
            index: index.clone(),
        });
        index
    }
}

/// Why a hash is allowed for a session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ArtAllowance {
    /// A binding attached to the session names the hash with this media type.
    Binding { media_type: String },
    /// The session's grant set holds the hash.
    Grant,
}

/// Whether `session` may be served `sha256`: a compiled binding attached to
/// it names the hash, or its grant set holds it. Bindings of other sessions
/// are never consulted.
pub fn art_allowance(session: &BrowserSession, sha256: &str) -> Option<ArtAllowance> {
    for attachment in session.attachments.values() {
        if let Some(art) = attachment.compiled_binding.binding.art.get(sha256) {
            return Some(ArtAllowance::Binding {
                media_type: art.media_type.clone(),
            });
        }
    }
    session
        .art_grants
        .contains(sha256)
        .then_some(ArtAllowance::Grant)
}

/// Why a read of an allowed picture failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ArtFailure {
    /// Not installed or not verified, or the declared type differs from the
    /// signed record's. Answered as a plain 404.
    NotFound,
    /// The installed bytes no longer match what was signed.
    Integrity(String),
    /// The bytes are not an image the route serves.
    Refused(&'static str),
}

/// A verified picture ready to send.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtBody {
    pub bytes: Vec<u8>,
    pub content_type: &'static str,
}

/// Read one indexed picture and re-verify it. `declared` is the media type a
/// binding named, when the allowance came from a binding.
pub fn read_art(
    index: &ArtIndex,
    sha256: &str,
    declared: Option<&str>,
) -> Result<ArtBody, ArtFailure> {
    let mut candidates = index
        .candidates(sha256)
        .iter()
        .filter(|entry| declared.is_none_or(|declared| declared == entry.media_type.mime()))
        .peekable();
    if candidates.peek().is_none() {
        return Err(ArtFailure::NotFound);
    }
    // A copy that fails verification falls through to the next bundle's copy;
    // the failure is returned only when no copy verifies.
    let mut failure = ArtFailure::NotFound;
    for entry in candidates {
        match read_entry(entry, sha256) {
            Ok(body) => return Ok(body),
            Err(error) => failure = error,
        }
    }
    Err(failure)
}

fn read_entry(entry: &PictureEntry, sha256: &str) -> Result<ArtBody, ArtFailure> {
    let bytes = lillux::read_regular_file_bounded_no_follow(&entry.blob_path, MAX_PICTURE_BYTES)
        .map_err(|error| ArtFailure::Integrity(format!("read picture blob: {error:#}")))?;
    if bytes.len() as u64 != entry.bytes {
        return Err(ArtFailure::Integrity(
            "blob length differs from the signed record".to_string(),
        ));
    }
    let actual = lillux::sha256_hex(&bytes);
    if actual != sha256 {
        return Err(ArtFailure::Integrity(format!(
            "blob hashes to {actual}, not the requested {sha256}"
        )));
    }
    let facts = inspect_image(&bytes, entry.media_type).map_err(|refusal| {
        tracing::warn!(%sha256, %refusal, "installed picture refused");
        ArtFailure::Refused(refusal.code())
    })?;
    if facts.width != entry.width || facts.height != entry.height {
        return Err(ArtFailure::Integrity(
            "blob dimensions differ from the signed record".to_string(),
        ));
    }
    Ok(ArtBody {
        bytes,
        content_type: entry.media_type.mime(),
    })
}

/// The outcome of one request, before it becomes a response.
enum ArtOutcome {
    Body(ArtBody),
    NotModified,
}

/// One request to the route, reduced to what the mode reads.
pub struct ArtRequest {
    pub method: Method,
    pub headers: axum::http::HeaderMap,
    /// The raw `{sha256}` path value.
    pub key: String,
    /// The route principal's id (`session:<id>`).
    pub principal_id: String,
    pub engine: Arc<Engine>,
}

pub struct ArtMode {
    pub ui: Arc<UiState>,
}

impl ResponseMode for ArtMode {
    fn key(&self) -> &'static str {
        ART_MODE_KEY
    }

    fn compile(
        &self,
        raw: &RawRouteSpec,
    ) -> Result<Arc<dyn CompiledResponseMode>, RouteConfigError> {
        if raw.auth != REQUIRED_AUTH {
            return Err(RouteConfigError::SourceAuthRequirement {
                id: raw.id.clone(),
                src: ART_MODE_KEY.into(),
                required: REQUIRED_AUTH.into(),
                got: raw.auth.clone(),
            });
        }
        let invalid = |reason: &str| RouteConfigError::InvalidResponseSpec {
            id: raw.id.clone(),
            mode: ART_MODE_KEY.into(),
            reason: reason.into(),
        };
        if raw.methods.len() != 1 || !raw.methods.contains("GET") {
            return Err(invalid("the art mode answers GET only"));
        }
        if !raw.path.ends_with(&format!("/{{{HASH_CAPTURE}}}")) {
            return Err(invalid("the art path must end in the {sha256} capture"));
        }
        Ok(Arc::new(CompiledArtMode {
            ui: self.ui.clone(),
            cache: self.ui.art_index.clone(),
        }))
    }
}

pub struct CompiledArtMode {
    ui: Arc<UiState>,
    cache: Arc<ArtIndexCache>,
}

fn art_error(status: StatusCode, code: &str, message: &str) -> RouteDispatchError {
    RouteDispatchError::Structured {
        code: code.to_string(),
        status: status.as_u16(),
        body: serde_json::json!({ "error": message, "error_code": code }),
        thread_id: None,
    }
}

impl CompiledArtMode {
    /// Answer one request. Public so tests drive the exact decision path
    /// without building a full route context.
    pub async fn serve(&self, request: ArtRequest) -> Result<Response, RouteDispatchError> {
        if request.method != Method::GET {
            return Err(art_error(
                StatusCode::METHOD_NOT_ALLOWED,
                "method_not_allowed",
                "this route answers GET only",
            ));
        }
        if !is_art_hash(&request.key) {
            return Err(RouteDispatchError::NotFound);
        }
        let session_id = request
            .principal_id
            .strip_prefix("session:")
            .ok_or(RouteDispatchError::Unauthorized)?;
        let session = self
            .ui
            .browser_sessions
            .get_session(session_id)
            .ok_or(RouteDispatchError::Unauthorized)?;
        let allowance =
            art_allowance(&session, &request.key).ok_or(RouteDispatchError::NotFound)?;
        let declared = match allowance {
            ArtAllowance::Binding { media_type } => Some(media_type),
            ArtAllowance::Grant => None,
        };
        let etag = format!("\"{}\"", request.key);
        let revalidating = request
            .headers
            .get(header::IF_NONE_MATCH)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| value == etag);
        let cache = self.cache.clone();
        let key = request.key.clone();
        let engine = request.engine;
        let outcome = tokio::task::spawn_blocking(move || -> Result<ArtOutcome, ArtFailure> {
            let index = cache.index_for(&engine);
            let usable = index.candidates(&key).iter().any(|entry| {
                declared
                    .as_deref()
                    .is_none_or(|declared| declared == entry.media_type.mime())
            });
            if !usable {
                return Err(ArtFailure::NotFound);
            }
            if revalidating {
                // The caller already holds exactly these content-addressed
                // bytes; none are sent, so none need re-reading.
                return Ok(ArtOutcome::NotModified);
            }
            read_art(&index, &key, declared.as_deref()).map(ArtOutcome::Body)
        })
        .await
        .map_err(|error| {
            tracing::error!(%error, "picture read task failed");
            art_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "art_failed",
                "the picture could not be read",
            )
        })?;
        match outcome {
            Ok(ArtOutcome::Body(body)) => Ok(art_response(&request.key, body)),
            Ok(ArtOutcome::NotModified) => Ok(not_modified_response(&request.key)),
            Err(ArtFailure::NotFound) => Err(RouteDispatchError::NotFound),
            Err(ArtFailure::Integrity(detail)) => {
                tracing::error!(sha256 = %request.key, %detail, "installed picture failed integrity");
                Err(art_error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "art_integrity",
                    "the installed picture does not match its signed record",
                ))
            }
            Err(ArtFailure::Refused(code)) => {
                tracing::error!(sha256 = %request.key, code, "installed picture refused");
                Err(art_error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "art_refused",
                    "the installed picture is not an image this route serves",
                ))
            }
        }
    }
}

fn etag_value(sha256: &str) -> HeaderValue {
    // The key is validated lowercase hex before any response is built.
    HeaderValue::from_str(&format!("\"{sha256}\"")).expect("a quoted hex digest is a header value")
}

fn common_headers(response: &mut Response, sha256: &str) {
    let headers = response.headers_mut();
    headers.insert(header::ETAG, etag_value(sha256));
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("private, max-age=31536000, immutable"),
    );
    headers.insert(header::VARY, HeaderValue::from_static("Cookie"));
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static("default-src 'none'; sandbox"),
    );
    headers.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("same-origin"),
    );
    headers.insert(
        HeaderName::from_static("cross-origin-resource-policy"),
        HeaderValue::from_static("same-origin"),
    );
}

fn art_response(sha256: &str, body: ArtBody) -> Response {
    let length = body.bytes.len();
    let mut response = (StatusCode::OK, body.bytes).into_response();
    common_headers(&mut response, sha256);
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static(body.content_type),
    );
    headers.insert(header::CONTENT_LENGTH, HeaderValue::from(length));
    headers.insert(header::ACCEPT_RANGES, HeaderValue::from_static("none"));
    response
}

fn not_modified_response(sha256: &str) -> Response {
    let mut response = StatusCode::NOT_MODIFIED.into_response();
    common_headers(&mut response, sha256);
    response
}

#[axum::async_trait]
impl CompiledResponseMode for CompiledArtMode {
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    async fn handle(
        &self,
        _compiled: &CompiledRoute,
        ctx: RouteDispatchContext,
    ) -> Result<Response, RouteDispatchError> {
        let key = ctx.captures.get(HASH_CAPTURE).cloned().unwrap_or_default();
        self.serve(ArtRequest {
            method: ctx.request_parts.method.clone(),
            headers: ctx.request_parts.headers.clone(),
            key,
            principal_id: ctx.principal.id.clone(),
            engine: ctx.state.engine.clone(),
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ryeos_bundle::pictures::PictureMediaType;

    fn entry(record: &str, role: &str, index: usize, hash_seed: char) -> PictureEntry {
        PictureEntry {
            sha256: hash_seed.to_string().repeat(64),
            media_type: PictureMediaType::Webp,
            role: role.to_owned(),
            rendition_index: index,
            width: 16,
            height: 10,
            bytes: 52,
            blob_path: PathBuf::from("/nonexistent"),
            record: record.to_owned(),
        }
    }

    fn index() -> ArtIndex {
        let mut records = BTreeMap::new();
        records.insert(
            "pictures/ryeos/atlas.yaml".to_owned(),
            vec![
                entry("pictures/ryeos/atlas.yaml", "hero", 0, 'a'),
                entry("pictures/ryeos/atlas.yaml", "card", 1, 'b'),
            ],
        );
        ArtIndex {
            entries: BTreeMap::new(),
            records,
        }
    }

    fn binding(picture: &str, role: Option<&str>) -> PictureBinding {
        PictureBinding {
            picture: picture.to_owned(),
            role: role.map(str::to_owned),
        }
    }

    #[test]
    fn a_binding_resolves_to_the_first_rendition_or_the_named_role() {
        let index = index();
        let first = index
            .resolve_binding(&binding("picture:ryeos/atlas", None))
            .unwrap();
        assert_eq!(first.art.sha256, "a".repeat(64));
        assert_eq!(first.art.media_type, "image/webp");
        assert_eq!(first.role, "hero");
        let card = index
            .resolve_binding(&binding("picture:ryeos/atlas", Some("card")))
            .unwrap();
        assert_eq!(card.art.sha256, "b".repeat(64));
    }

    #[test]
    fn every_failed_binding_is_a_refusal_and_never_a_substitute() {
        let index = index();
        assert_eq!(
            index.resolve_binding(&binding("picture:ryeos/missing", None)),
            Err(PictureRefusal::NotIndexed)
        );
        assert_eq!(
            index.resolve_binding(&binding("picture:ryeos/atlas", Some("wide"))),
            Err(PictureRefusal::UnknownRole("wide".to_owned()))
        );
        assert_eq!(
            index.resolve_binding(&binding("view:ryeos/atlas", None)),
            Err(PictureRefusal::Malformed(
                PictureBindingError::NotAPictureRef
            ))
        );
        assert_eq!(
            index.resolve_binding(&binding("picture:../atlas", None)),
            Err(PictureRefusal::Malformed(PictureBindingError::MalformedRef))
        );
        // A record whose bundle failed verification contributes nothing.
        assert_eq!(
            ArtIndex::default().resolve_binding(&binding("picture:ryeos/atlas", None)),
            Err(PictureRefusal::NotIndexed)
        );
    }
}

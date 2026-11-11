//! The native platform: signed HTTP over the shared client transport, tokio
//! timers and local tasks, and owner-private files for the small durable
//! store.
//!
//! Requests are signed over the exact body bytes the runtime encoded, so a
//! retry is byte-identical. Paths under `/ui/` also carry the session
//! credential as a `ryeos_session=<id>` cookie header (there is no cookie jar:
//! the credential is held here and nowhere else). Tasks run on the calling
//! thread's [`tokio::task::LocalSet`]; spawning outside one panics.

use std::cell::RefCell;
use std::future::Future;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rand::RngCore;
use ryeos_client_transport::TransportError;
use ryeos_client_transport::discovery::discover_audience;
use ryeos_client_transport::http::{resolve_daemon_url, signed_client};
use ryeos_client_transport::signing::{SignHeaders, Signer};
use sha2::{Digest, Sha256};
use tokio::time::Instant;

use crate::admission::decode_session;
use crate::api::{LAUNCH_MINT_PATH, SESSION_CURRENT_PATH};
use crate::codec::{decode_strict, encode_exact};
use crate::error::RuntimeError;
use crate::platform::{
    Auth, ByteSource, Clock, Elapsed, EstablishMode, EstablishedSession, Platform, ReplacementMode,
    Spawner, Store, TaskHandle, Transport,
};

/// The most a single activation of a launch URL is retried for. Activation is
/// replayable, so a lost response is recovered by asking again.
const ACTIVATION_DEADLINE: Duration = Duration::from_secs(55);
const ACTIVATION_ATTEMPT: Duration = Duration::from_secs(5);
const ACTIVATION_RETRY_PAUSE: Duration = Duration::from_millis(250);

/// Why the platform could not be built.
#[derive(Debug, thiserror::Error)]
pub enum ConnectError {
    #[error("no system directory found")]
    NoSystemDir,

    #[error("transport: {0}")]
    Transport(#[from] TransportError),
}

/// What a fresh session is minted for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MintRequest {
    /// The authored shared presentation Config.
    pub config_ref: String,
    /// Explicit project context; absent means node level.
    pub project_path: Option<String>,
}

fn app_root() -> Option<PathBuf> {
    std::env::var_os("RYEOS_APP_ROOT")
        .map(PathBuf::from)
        .or_else(|| dirs::data_dir().map(|dir| dir.join("ryeos")))
}

fn network(path: &str, what: &str, error: impl std::fmt::Display) -> RuntimeError {
    RuntimeError::Network {
        path: path.to_owned(),
        message: format!("{what}: {error}"),
    }
}

fn bounded_token(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
}

fn seat_history_stream_response(
    status: reqwest::StatusCode,
    content_type: Option<&reqwest::header::HeaderValue>,
    content_length: Option<u64>,
) -> bool {
    status == reqwest::StatusCode::OK
        && content_length != Some(0)
        && content_type
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.split(';').next())
            .is_some_and(|media_type| media_type.trim().eq_ignore_ascii_case("text/event-stream"))
}

/// The session credential a launch response set, if it named one.
fn cookie_session(response: &reqwest::Response) -> Option<String> {
    response
        .headers()
        .get_all(reqwest::header::SET_COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .filter_map(|value| value.split(';').next())
        .filter_map(|pair| pair.trim().strip_prefix("ryeos_session="))
        .map(str::to_owned)
        .find(|value| bounded_token(value))
}

// ---------------------------------------------------------------------------
// The signed connection
// ---------------------------------------------------------------------------

/// One pooled connection and the identity that signs it. Shared by the
/// transport and the authentication seam.
struct Link {
    base_url: String,
    audience: String,
    signer: Option<Signer>,
    http: reqwest::Client,
    /// The session credential, once established.
    session: RefCell<Option<String>>,
}

impl Link {
    fn url(&self, path: &str) -> String {
        format!("{}{}", self.base_url.trim_end_matches('/'), path)
    }

    fn sign(&self, method: &str, path: &str, body: &[u8]) -> Result<SignHeaders, RuntimeError> {
        let signer = self
            .signer
            .as_ref()
            .ok_or_else(|| RuntimeError::Sign("no identity found - run ryeos init".to_owned()))?;
        signer
            .sign(method, path, body, &self.audience)
            .map_err(|error| RuntimeError::Sign(error.to_string()))
    }

    fn cookie(&self, path: &str) -> Option<String> {
        path.starts_with("/ui/")
            .then(|| self.session.borrow().clone())
            .flatten()
            .map(|session_id| format!("ryeos_session={session_id}"))
    }

    fn signed(
        &self,
        builder: reqwest::RequestBuilder,
        method: &str,
        path: &str,
        body: &[u8],
    ) -> Result<reqwest::RequestBuilder, RuntimeError> {
        let headers = self.sign(method, path, body)?;
        let mut builder = builder
            .header("x-ryeos-key-id", &headers.key_id)
            .header("x-ryeos-timestamp", &headers.timestamp)
            .header("x-ryeos-nonce", &headers.nonce)
            .header("x-ryeos-signature", &headers.signature);
        if let Some(cookie) = self.cookie(path) {
            builder = builder.header("cookie", cookie);
        }
        Ok(builder)
    }

    fn get_request(
        &self,
        path: &str,
        accept: Option<&str>,
    ) -> Result<reqwest::RequestBuilder, RuntimeError> {
        let mut builder = self.http.get(self.url(path));
        if let Some(accept) = accept {
            builder = builder.header("accept", accept);
        }
        self.signed(builder, "GET", path, b"")
    }

    fn post_request(
        &self,
        path: &str,
        body: String,
        accept: &str,
    ) -> Result<reqwest::RequestBuilder, RuntimeError> {
        let bytes = body.into_bytes();
        let builder = self
            .http
            .post(self.url(path))
            .header("content-type", "application/json")
            .header("accept", accept);
        Ok(self.signed(builder, "POST", path, &bytes)?.body(bytes))
    }

    async fn send(
        &self,
        path: &str,
        request: reqwest::RequestBuilder,
    ) -> Result<reqwest::Response, RuntimeError> {
        request
            .send()
            .await
            .map_err(|error| network(path, "request send", error))
    }

    /// The daemon's own message for a non-2xx answer.
    async fn refusal(path: &str, response: reqwest::Response) -> RuntimeError {
        let status = response.status().as_u16();
        let message = match response.bytes().await {
            Ok(body) => super::daemon_error_message(&body),
            Err(_) => String::new(),
        };
        RuntimeError::Daemon {
            path: path.to_owned(),
            status,
            message,
        }
    }

    /// The body of a 2xx answer as text.
    async fn text(path: &str, response: reqwest::Response) -> Result<String, RuntimeError> {
        if !response.status().is_success() {
            return Err(Self::refusal(path, response).await);
        }
        let body = response
            .bytes()
            .await
            .map_err(|_| RuntimeError::UnreadableResponse {
                path: path.to_owned(),
            })?;
        String::from_utf8(body.to_vec()).map_err(|_| RuntimeError::UnreadableResponse {
            path: path.to_owned(),
        })
    }

    fn adopt(&self, session_id: &str) -> Result<(), RuntimeError> {
        if !bounded_token(session_id) {
            return Err(RuntimeError::BindingRequest(
                "replacement UI session id is not a bounded transport token".to_owned(),
            ));
        }
        *self.session.borrow_mut() = Some(session_id.to_owned());
        Ok(())
    }

    /// The authenticated session record, unwrapped, and the typed session.
    async fn current(
        &self,
    ) -> Result<(serde_json::Value, ryeos_client_base::ui::BrowserSession), RuntimeError> {
        let request = self.get_request(SESSION_CURRENT_PATH, Some("application/json"))?;
        let response = self.send(SESSION_CURRENT_PATH, request).await?;
        decode_session(&Self::text(SESSION_CURRENT_PATH, response).await?)
    }
}

// ---------------------------------------------------------------------------
// Clock and tasks
// ---------------------------------------------------------------------------

/// Wall clock and timers on tokio time.
#[derive(Debug, Default, Clone, Copy)]
pub struct NativeClock;

impl Clock for NativeClock {
    fn now_ms(&self) -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|elapsed| u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX))
            .unwrap_or(0)
    }

    fn sleep(&self, duration: Duration) -> impl Future<Output = ()> {
        tokio::time::sleep(duration)
    }

    async fn timeout<F: Future>(
        &self,
        duration: Duration,
        future: F,
    ) -> Result<F::Output, Elapsed> {
        tokio::time::timeout(duration, future)
            .await
            .map_err(|_| Elapsed)
    }
}

/// A task on the local set. Dropping the handle aborts the task.
pub struct NativeTask(tokio::task::JoinHandle<()>);

impl TaskHandle for NativeTask {
    fn abort(&self) {
        self.0.abort();
    }

    fn is_finished(&self) -> bool {
        self.0.is_finished()
    }
}

impl Drop for NativeTask {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Spawns onto the calling thread's local set.
#[derive(Debug, Default, Clone, Copy)]
pub struct NativeSpawner;

impl Spawner for NativeSpawner {
    type Handle = NativeTask;

    fn spawn(&self, future: impl Future<Output = ()> + 'static) -> NativeTask {
        NativeTask(tokio::task::spawn_local(future))
    }
}

// ---------------------------------------------------------------------------
// Transport
// ---------------------------------------------------------------------------

/// A response body read chunk by chunk.
pub struct ResponseSource {
    path: String,
    response: reqwest::Response,
}

impl ByteSource for ResponseSource {
    async fn next_chunk(&mut self) -> Result<Option<Vec<u8>>, RuntimeError> {
        match self.response.chunk().await {
            Ok(chunk) => Ok(chunk.map(|bytes| bytes.to_vec())),
            Err(error) => Err(network(&self.path, "read stream", error)),
        }
    }
}

/// Signed requests to the daemon.
pub struct NativeTransport {
    link: Rc<Link>,
}

impl Transport for NativeTransport {
    type Sse = ResponseSource;
    type Frames = ResponseSource;

    async fn get(&self, path: &str) -> Result<String, RuntimeError> {
        let request = self.link.get_request(path, Some("application/json"))?;
        let response = self.link.send(path, request).await?;
        Link::text(path, response).await
    }

    async fn post_encoded(&self, path: &str, body: String) -> Result<String, RuntimeError> {
        let request = self.link.post_request(path, body, "application/json")?;
        let response = self.link.send(path, request).await?;
        Link::text(path, response).await
    }

    async fn open_sse(&self, path: &str) -> Result<ResponseSource, RuntimeError> {
        let request = self.link.get_request(path, Some("text/event-stream"))?;
        let response = self.link.send(path, request).await?;
        if !response.status().is_success() {
            return Err(Link::refusal(path, response).await);
        }
        Ok(ResponseSource {
            path: path.to_owned(),
            response,
        })
    }

    async fn open_history(&self, path: &str, body: String) -> Result<ResponseSource, RuntimeError> {
        let request = self.link.post_request(path, body, "text/event-stream")?;
        let response = self.link.send(path, request).await?;
        if !response.status().is_success() {
            return Err(Link::refusal(path, response).await);
        }
        if !seat_history_stream_response(
            response.status(),
            response.headers().get(reqwest::header::CONTENT_TYPE),
            response.content_length(),
        ) {
            return Err(RuntimeError::BindingRequest(
                "seat history did not return a nonempty event stream response".to_owned(),
            ));
        }
        Ok(ResponseSource {
            path: path.to_owned(),
            response,
        })
    }
}

// ---------------------------------------------------------------------------
// Authentication
// ---------------------------------------------------------------------------

/// Session establishment: mint and redeem, adopt, and in-place replacement.
pub struct NativeAuth {
    link: Rc<Link>,
    mint: Option<MintRequest>,
}

impl NativeAuth {
    async fn mint_fresh(&self) -> Result<EstablishedSession, RuntimeError> {
        if self.link.session.borrow().is_some() {
            let (current, session) = self.link.current().await?;
            return Ok(EstablishedSession {
                session_id: session.session_id,
                current,
            });
        }
        let mint = self.mint.as_ref().ok_or_else(|| {
            RuntimeError::BindingRequest(
                "this platform was built without a presentation to mint".to_owned(),
            )
        })?;
        let user_principal_id = self
            .link
            .signer
            .as_ref()
            .map(|signer| format!("fp:{}", signer.fingerprint));
        let body = encode_exact(&ryeos_client_base::presentation::launch_mint_body(
            &mint.config_ref,
            mint.project_path.as_deref(),
            user_principal_id.as_deref(),
        ))?;
        let request = self
            .link
            .post_request(LAUNCH_MINT_PATH, body, "application/json")?;
        let response = self.link.send(LAUNCH_MINT_PATH, request).await?;
        let minted = decode_strict(&Link::text(LAUNCH_MINT_PATH, response).await?)?;
        if minted
            .get("ui_binding_contract_revision")
            .and_then(serde_json::Value::as_str)
            != Some(ryeos_client_base::UI_BINDING_CONTRACT_REVISION)
        {
            return Err(RuntimeError::Decode(format!(
                "UI binding contract mismatch: daemon must advertise '{}'",
                ryeos_client_base::UI_BINDING_CONTRACT_REVISION
            )));
        }
        let (Some(session_id), Some(launch_url)) = (
            minted.get("session_id").and_then(serde_json::Value::as_str),
            minted.get("launch_url").and_then(serde_json::Value::as_str),
        ) else {
            return Err(RuntimeError::UnreadableResponse {
                path: LAUNCH_MINT_PATH.to_owned(),
            });
        };
        let launch_path = self.launch_path(launch_url)?;
        self.redeem_path(&launch_path, Some(session_id)).await
    }

    /// The origin-relative launch route of `launch_url`, which may be the
    /// route itself or an absolute URL on the configured daemon origin.
    fn launch_path(&self, launch_url: &str) -> Result<String, RuntimeError> {
        if launch_url.starts_with("/ui/launch/") {
            return Ok(launch_url.to_owned());
        }
        let origin = self.link.base_url.trim_end_matches('/');
        launch_url
            .strip_prefix(origin)
            .map(str::to_owned)
            .ok_or_else(|| {
                RuntimeError::BindingRequest(
                    "minted UI launch URL does not retain the configured daemon origin".to_owned(),
                )
            })
    }

    /// Activate a daemon-minted immutable session and adopt only the exact
    /// authenticated result. Activation is replayable for response-loss
    /// recovery; a replacement keeps the predecessor credential until the
    /// successor is observed. When `expected` is absent the session is the one
    /// the launch response names.
    async fn redeem_path(
        &self,
        launch_path: &str,
        expected: Option<&str>,
    ) -> Result<EstablishedSession, RuntimeError> {
        let valid_launch_path = launch_path
            .strip_prefix("/ui/launch/")
            .is_some_and(|token| {
                !token.is_empty()
                    && token.len() <= 128
                    && token
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
            });
        if !valid_launch_path {
            return Err(RuntimeError::BindingRequest(
                "UI activation path is not a local token route".to_owned(),
            ));
        }
        let link = &self.link;
        let prior = link.session.borrow().clone();
        let restore = |prior: &Option<String>| {
            *link.session.borrow_mut() = prior.clone();
        };
        let deadline = Instant::now() + ACTIVATION_DEADLINE;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                restore(&prior);
                return Err(RuntimeError::Network {
                    path: launch_path.to_owned(),
                    message: "UI activation could not be observed before its recovery deadline"
                        .to_owned(),
                });
            }
            let attempt_timeout = remaining.min(ACTIVATION_ATTEMPT);
            restore(&prior);
            let request = link.get_request(launch_path, None)?;
            let unknown = match tokio::time::timeout(attempt_timeout, request.send()).await {
                Err(_) => "activation request timed out before an outcome was observed".to_owned(),
                Ok(Ok(response))
                    if response.status().is_success() || response.status().is_redirection() =>
                {
                    let session_id = match expected {
                        Some(session_id) => session_id.to_owned(),
                        None => match cookie_session(&response) {
                            Some(session_id) => session_id,
                            None => {
                                restore(&prior);
                                return Err(RuntimeError::BindingRequest(
                                    "UI activation did not name the session it created".to_owned(),
                                ));
                            }
                        },
                    };
                    if let Err(error) = link.adopt(&session_id) {
                        restore(&prior);
                        return Err(error);
                    }
                    let observation_timeout = deadline
                        .saturating_duration_since(Instant::now())
                        .min(ACTIVATION_ATTEMPT);
                    match tokio::time::timeout(observation_timeout, link.current()).await {
                        Ok(Ok((current, session))) if session.session_id == session_id => {
                            return Ok(EstablishedSession {
                                session_id,
                                current,
                            });
                        }
                        Ok(Ok(_)) => {
                            restore(&prior);
                            return Err(RuntimeError::BindingRequest(
                                "UI activation authenticated a different session".to_owned(),
                            ));
                        }
                        Ok(Err(error)) => {
                            format!("activation committed; observation failed: {error}")
                        }
                        Err(_) => "activation committed; session observation timed out".to_owned(),
                    }
                }
                Ok(Ok(response)) if response.status().is_client_error() => {
                    restore(&prior);
                    return Err(RuntimeError::Daemon {
                        path: launch_path.to_owned(),
                        status: response.status().as_u16(),
                        message: "UI session activation refused".to_owned(),
                    });
                }
                Ok(Ok(response)) => format!(
                    "activation server outcome was uncertain (HTTP {})",
                    response.status().as_u16()
                ),
                Ok(Err(error)) => format!("activation contact failed: {error}"),
            };
            if Instant::now() >= deadline {
                restore(&prior);
                return Err(RuntimeError::Network {
                    path: launch_path.to_owned(),
                    message: unknown,
                });
            }
            tokio::time::sleep(ACTIVATION_RETRY_PAUSE).await;
        }
    }

    async fn adopt_credential(&self, credential: &str) -> Result<EstablishedSession, RuntimeError> {
        let prior = self.link.session.borrow().clone();
        self.link.adopt(credential)?;
        match self.link.current().await {
            Ok((current, session)) if session.session_id == credential => Ok(EstablishedSession {
                session_id: session.session_id,
                current,
            }),
            Ok(_) => {
                *self.link.session.borrow_mut() = prior;
                Err(RuntimeError::BindingRequest(
                    "authenticated UI session did not match the supplied credential".to_owned(),
                ))
            }
            Err(error) => {
                *self.link.session.borrow_mut() = prior;
                Err(error)
            }
        }
    }
}

impl Auth for NativeAuth {
    async fn establish(&self, mode: EstablishMode) -> Result<EstablishedSession, RuntimeError> {
        match mode {
            EstablishMode::Fresh => self.mint_fresh().await,
            EstablishMode::Adopt(credential) => self.adopt_credential(&credential).await,
            EstablishMode::CookiePresent => {
                if self.link.session.borrow().is_none() {
                    return Err(RuntimeError::BindingRequest(
                        "no UI session credential is held".to_owned(),
                    ));
                }
                let (current, session) = self.link.current().await?;
                Ok(EstablishedSession {
                    session_id: session.session_id,
                    current,
                })
            }
        }
    }

    async fn redeem(&self, launch_url: &str) -> Result<EstablishedSession, RuntimeError> {
        let launch_path = self.launch_path(launch_url)?;
        self.redeem_path(&launch_path, None).await
    }

    fn replacement(&self) -> ReplacementMode {
        ReplacementMode::InPlace
    }
}

// ---------------------------------------------------------------------------
// Store
// ---------------------------------------------------------------------------

/// Owner-private files, one per key, named by the SHA-256 of the key. The
/// runtime already keeps the session id out of its keys for a store that says
/// [`Store::hashes_session_keys`].
pub struct FileStore {
    root: PathBuf,
}

impl FileStore {
    fn path_for(&self, key: &str) -> PathBuf {
        let digest = Sha256::digest(key.as_bytes());
        self.root.join(format!("{digest:x}"))
    }
}

impl Store for FileStore {
    fn get(&self, key: &str) -> Option<String> {
        std::fs::read_to_string(self.path_for(key)).ok()
    }

    fn set(&self, key: &str, value: &str) -> Result<(), RuntimeError> {
        lillux::atomic_write_private(&self.path_for(key), value.as_bytes())
            .map_err(|_| RuntimeError::StoreWrite)
    }

    fn remove(&self, key: &str) {
        let _ = lillux::remove_file_durable(&self.path_for(key));
    }

    fn hashes_session_keys(&self) -> bool {
        true
    }
}

// ---------------------------------------------------------------------------
// Platform
// ---------------------------------------------------------------------------

/// The native host: one daemon connection, tokio timers and local tasks, and
/// private files.
pub struct NativePlatform {
    clock: NativeClock,
    spawner: NativeSpawner,
    transport: NativeTransport,
    auth: NativeAuth,
    store: FileStore,
}

impl NativePlatform {
    /// Discover the daemon under the app root (`RYEOS_APP_ROOT`, else the
    /// user data directory), resolve the signing identity and bind the
    /// audience. `mint` names the presentation a [`EstablishMode::Fresh`]
    /// session is minted for; a platform that only adopts or resumes passes
    /// none.
    pub async fn connect(mint: Option<MintRequest>) -> Result<Self, ConnectError> {
        let app_root = app_root().ok_or(ConnectError::NoSystemDir)?;
        let mut base_url = resolve_daemon_url(&app_root).await?;
        let signer = Signer::resolve(&app_root).ok();
        // Discovery and signed requests share one normalized base and both
        // refuse redirects; the configured origin cannot move between them.
        let audience = if signer.is_some() {
            let discovered = discover_audience(&base_url).await?;
            base_url = discovered.base_url;
            discovered.principal_id
        } else {
            String::new()
        };
        let link = Rc::new(Link {
            base_url,
            audience,
            signer,
            http: signed_client()?,
            session: RefCell::new(None),
        });
        Ok(Self {
            clock: NativeClock,
            spawner: NativeSpawner,
            transport: NativeTransport { link: link.clone() },
            auth: NativeAuth { link, mint },
            store: FileStore {
                root: app_root.join("ui-retention"),
            },
        })
    }
}

impl Platform for NativePlatform {
    type Clock = NativeClock;
    type Spawner = NativeSpawner;
    type Transport = NativeTransport;
    type Auth = NativeAuth;
    type Store = FileStore;

    fn clock(&self) -> &NativeClock {
        &self.clock
    }

    fn spawner(&self) -> &NativeSpawner {
        &self.spawner
    }

    fn transport(&self) -> &NativeTransport {
        &self.transport
    }

    fn auth(&self) -> &NativeAuth {
        &self.auth
    }

    fn store(&self) -> &FileStore {
        &self.store
    }

    fn random_bytes(&self, out: &mut [u8]) {
        rand::rngs::OsRng.fill_bytes(out);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seat_history_requires_an_actual_sse_response() {
        use reqwest::StatusCode;
        use reqwest::header::HeaderValue;

        let sse = HeaderValue::from_static("text/event-stream; charset=utf-8");
        let json = HeaderValue::from_static("application/json");
        let lookalike = HeaderValue::from_static("text/event-streaming");
        assert!(seat_history_stream_response(
            StatusCode::OK,
            Some(&sse),
            None
        ));
        assert!(seat_history_stream_response(
            StatusCode::OK,
            Some(&sse),
            Some(42)
        ));
        assert!(!seat_history_stream_response(
            StatusCode::NO_CONTENT,
            Some(&sse),
            None
        ));
        assert!(!seat_history_stream_response(
            StatusCode::OK,
            Some(&sse),
            Some(0)
        ));
        assert!(!seat_history_stream_response(
            StatusCode::OK,
            Some(&json),
            None
        ));
        assert!(!seat_history_stream_response(
            StatusCode::OK,
            Some(&lookalike),
            None
        ));
        assert!(!seat_history_stream_response(StatusCode::OK, None, None));
    }

    #[test]
    fn credentials_are_bounded_transport_tokens() {
        assert!(bounded_token("live-123"));
        assert!(!bounded_token(""));
        assert!(!bounded_token("bad/token"));
        assert!(!bounded_token(&"a".repeat(65)));
    }

    #[test]
    fn store_names_files_by_key_hash_only() {
        let store = FileStore {
            root: PathBuf::from("/nonexistent"),
        };
        let path = store.path_for("ryeos.ui.seat.pending.v2:abc:p%2F1");
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        assert_eq!(name.len(), 64);
        assert!(name.bytes().all(|byte| byte.is_ascii_hexdigit()));
        assert!(store.hashes_session_keys());
    }
}

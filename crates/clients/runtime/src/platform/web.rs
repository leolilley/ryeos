//! The browser host: `fetch` with the session cookie, `setTimeout` timers,
//! local tasks and `localStorage`.
//!
//! The page is served by the daemon at `/ui` after the launch redirect has set
//! the session cookie, so every request is same-origin and carries that
//! cookie; nothing here signs. Event streams are read from `fetch` response
//! bodies rather than `EventSource`, because the runtime frames them itself.

use std::cell::{Cell, RefCell};
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::task::{Context, Poll, Waker};
use std::time::Duration;

use js_sys::{Promise, Reflect, Uint8Array};
use wasm_bindgen::{JsCast, JsValue};
use wasm_bindgen_futures::JsFuture;

use super::{
    Auth, ByteSource, Clock, Elapsed, EstablishMode, EstablishedSession, Platform, ReplacementMode,
    Spawner, Store, TaskHandle, Transport,
};
use crate::admission::decode_session;
use crate::api::SESSION_CURRENT_PATH;
use crate::error::RuntimeError;

fn window() -> web_sys::Window {
    web_sys::window().expect("the runtime's web platform runs in a window")
}

fn network(path: &str, what: &str, error: &JsValue) -> RuntimeError {
    RuntimeError::Network {
        path: path.to_owned(),
        message: format!("{what}: {}", js_message(error)),
    }
}

fn js_message(value: &JsValue) -> String {
    value
        .dyn_ref::<js_sys::Error>()
        .map(|error| String::from(error.message()))
        .or_else(|| value.as_string())
        .unwrap_or_else(|| format!("{value:?}"))
}

/// The page's route: its location hash without the leading `#`.
pub fn current_route() -> String {
    window()
        .location()
        .hash()
        .unwrap_or_default()
        .trim_start_matches('#')
        .to_owned()
}

/// A `hashchange` listener, removed when dropped.
pub struct RouteListener {
    callback: wasm_bindgen::closure::Closure<dyn FnMut()>,
}

impl Drop for RouteListener {
    fn drop(&mut self) {
        let _ = window().remove_event_listener_with_callback(
            "hashchange",
            self.callback.as_ref().unchecked_ref(),
        );
    }
}

/// Call `f` with the new route after every location hash change.
pub fn on_route_change(f: impl Fn(String) + 'static) -> Result<RouteListener, RuntimeError> {
    let callback = wasm_bindgen::closure::Closure::<dyn FnMut()>::new(move || f(current_route()));
    window()
        .add_event_listener_with_callback("hashchange", callback.as_ref().unchecked_ref())
        .map_err(|error| {
            RuntimeError::BindingRequest(format!("route listener: {}", js_message(&error)))
        })?;
    Ok(RouteListener { callback })
}

/// A `beforeunload` listener, removed when dropped.
pub struct UnloadListener {
    callback: wasm_bindgen::closure::Closure<dyn FnMut()>,
}

impl Drop for UnloadListener {
    fn drop(&mut self) {
        let _ = window().remove_event_listener_with_callback(
            "beforeunload",
            self.callback.as_ref().unchecked_ref(),
        );
    }
}

/// Call `f` as the page is about to unload: a reload, a close or a
/// navigation away. `beforeunload` runs before `pagehide`, whose windowing
/// handler ends the app; it does not run for a page kept in the back-forward
/// cache. `f` never asks the user to stay.
pub fn on_unload(f: impl Fn() + 'static) -> Result<UnloadListener, RuntimeError> {
    let callback = wasm_bindgen::closure::Closure::<dyn FnMut()>::new(f);
    window()
        .add_event_listener_with_callback("beforeunload", callback.as_ref().unchecked_ref())
        .map_err(|error| {
            RuntimeError::BindingRequest(format!("unload listener: {}", js_message(&error)))
        })?;
    Ok(UnloadListener { callback })
}

/// POST `body` to `path` so that it outlives the page (`keepalive`). Its
/// answer is never read: what it did is observed by whoever comes next.
pub fn send_outliving_page(path: &str, body: &str) {
    let init = web_sys::RequestInit::new();
    init.set_method("POST");
    init.set_credentials(web_sys::RequestCredentials::SameOrigin);
    init.set_redirect(web_sys::RequestRedirect::Error);
    // `keepalive` lets the request outlive the page; this web-sys has no
    // setter for it.
    let _ = Reflect::set(&init, &JsValue::from_str("keepalive"), &JsValue::TRUE);
    if let Ok(headers) = web_sys::Headers::new() {
        let _ = headers.set("Accept", "application/json");
        let _ = headers.set("Content-Type", "application/json");
        init.set_headers(&headers);
    }
    init.set_body(&JsValue::from_str(body));
    match web_sys::Request::new_with_str_and_init(path, &init) {
        Ok(request) => {
            // The promise settles after the page is gone; nothing awaits it.
            let _ = window().fetch_with_request(&request);
        }
        Err(error) => console_note(&format!(
            "RyeOS: the seat was not given back as the page closed: {}",
            js_message(&error)
        )),
    }
}

/// Note `message` in the browser console, for an operator reading it after
/// the fact.
pub fn console_note(message: &str) {
    let console = Reflect::get(&js_sys::global(), &JsValue::from_str("console"));
    if let Ok(console) = console
        && let Ok(info) = Reflect::get(&console, &JsValue::from_str("info"))
        && let Some(info) = info.dyn_ref::<js_sys::Function>()
    {
        let _ = info.call1(&console, &JsValue::from_str(message));
    }
}

/// The browser's side of the effects the runtime cannot perform itself:
/// location hash navigation, clipboard copying, opening an external URL and
/// navigating to a successor session's launch URL. Independent presentation
/// documents need a prepare-first document host this page is not, and stay
/// refused.
pub fn web_adapter() -> crate::effects::AdapterHandler {
    use ryeos_client_base::ui::{RyeOsEffect, RyeOsEffectKind};

    use crate::effects::AdapterFuture;

    fn host(what: &str, error: &JsValue) -> RuntimeError {
        RuntimeError::BindingRequest(format!("{what}: {}", js_message(error)))
    }

    Rc::new(|effect: RyeOsEffect| -> AdapterFuture {
        Box::pin(async move {
            match effect.kind {
                RyeOsEffectKind::SetLocationHash { hash } => {
                    window()
                        .location()
                        .set_hash(&hash)
                        .map_err(|error| host("location hash", &error))?;
                    Ok(None)
                }
                RyeOsEffectKind::CopyToClipboard { text } => {
                    let promise = window().navigator().clipboard().write_text(&text);
                    JsFuture::from(promise)
                        .await
                        .map_err(|error| host("clipboard", &error))?;
                    Ok(None)
                }
                RyeOsEffectKind::OpenUrl { url } => {
                    // With noopener the call may return no window even when it
                    // opened one: this records that the call returned, never
                    // that a window was seen opening.
                    window()
                        .open_with_url_and_target_and_features(
                            &url,
                            "_blank",
                            "noopener,noreferrer",
                        )
                        .map_err(|error| host("open URL", &error))?;
                    Ok(Some(serde_json::json!({
                        "action": "open_url", "dispatch": "returned", "opened": "unknown",
                    })))
                }
                RyeOsEffectKind::ReplaceSession { launch_url, .. } => {
                    let location = window().location();
                    let base = location.href().map_err(|error| host("location", &error))?;
                    let launch = web_sys::Url::new_with_base(&launch_url, &base)
                        .map_err(|error| host("replacement launch URL", &error))?;
                    let origin = location
                        .origin()
                        .map_err(|error| host("location", &error))?;
                    if launch.origin() != origin || !launch.pathname().starts_with("/ui/launch/") {
                        return Err(RuntimeError::BindingRequest(
                            "replacement UI session has an invalid launch origin or path"
                                .to_owned(),
                        ));
                    }
                    let target =
                        format!("{}{}{}", launch.pathname(), launch.search(), launch.hash());
                    location
                        .assign(&target)
                        .map_err(|error| host("navigation", &error))?;
                    Ok(None)
                }
                // Presentation documents are offered by the runtime
                // (`ClientRuntime::with_documents`), never by this adapter.
                _ => Err(RuntimeError::BindingRequest(
                    "this effect is not a browser host action".to_owned(),
                )),
            }
        })
    })
}

/// `text/event-stream` with optional parameters, and a body that is not
/// declared empty.
fn event_stream_response(response: &web_sys::Response) -> bool {
    let headers = response.headers();
    let content_type = headers.get("content-type").ok().flatten();
    let declared_empty = headers
        .get("content-length")
        .ok()
        .flatten()
        .is_some_and(|length| length.trim() == "0");
    response.status() == 200
        && !declared_empty
        && response.body().is_some()
        && content_type
            .as_deref()
            .and_then(|value| value.split(';').next())
            .is_some_and(|media_type| media_type.trim().eq_ignore_ascii_case("text/event-stream"))
}

// ---------------------------------------------------------------------------
// Clock and tasks
// ---------------------------------------------------------------------------

/// A `setTimeout` promise; dropping it clears the timer.
struct Sleep {
    promise: JsFuture,
    handle: Rc<Cell<Option<i32>>>,
}

impl Sleep {
    fn new(duration: Duration) -> Self {
        let millis = i32::try_from(duration.as_millis()).unwrap_or(i32::MAX);
        let handle = Rc::new(Cell::new(None));
        let slot = handle.clone();
        let promise = Promise::new(&mut |resolve, _reject| {
            slot.set(
                window()
                    .set_timeout_with_callback_and_timeout_and_arguments_0(&resolve, millis)
                    .ok(),
            );
        });
        Self {
            promise: JsFuture::from(promise),
            handle,
        }
    }
}

impl Future for Sleep {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        match Pin::new(&mut self.promise).poll(cx) {
            Poll::Ready(_) => {
                self.handle.set(None);
                Poll::Ready(())
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

impl Drop for Sleep {
    fn drop(&mut self) {
        if let Some(handle) = self.handle.take() {
            window().clear_timeout_with_handle(handle);
        }
    }
}

/// The first of a future and its deadline.
struct Deadline<F> {
    future: Pin<Box<F>>,
    sleep: Sleep,
}

impl<F: Future> Future for Deadline<F> {
    type Output = Result<F::Output, Elapsed>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        if let Poll::Ready(output) = self.future.as_mut().poll(cx) {
            return Poll::Ready(Ok(output));
        }
        match Pin::new(&mut self.sleep).poll(cx) {
            Poll::Ready(()) => Poll::Ready(Err(Elapsed)),
            Poll::Pending => Poll::Pending,
        }
    }
}

/// Wall clock and timers from the window.
#[derive(Debug, Default, Clone, Copy)]
pub struct WebClock;

impl Clock for WebClock {
    fn now_ms(&self) -> u64 {
        // `Date.now()` is a whole, non-negative millisecond count.
        js_sys::Date::now().max(0.0) as u64
    }

    fn sleep(&self, duration: Duration) -> impl Future<Output = ()> {
        Sleep::new(duration)
    }

    fn timeout<F: Future>(
        &self,
        duration: Duration,
        future: F,
    ) -> impl Future<Output = Result<F::Output, Elapsed>> {
        Deadline {
            future: Box::pin(future),
            sleep: Sleep::new(duration),
        }
    }
}

#[derive(Default)]
struct TaskState {
    aborted: Cell<bool>,
    finished: Cell<bool>,
    waker: RefCell<Option<Waker>>,
}

/// A future that ends early, unfinished, once its task is aborted.
struct Abortable<F> {
    future: Pin<Box<F>>,
    state: Rc<TaskState>,
}

impl<F: Future<Output = ()>> Future for Abortable<F> {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        if self.state.aborted.get() {
            self.state.finished.set(true);
            return Poll::Ready(());
        }
        *self.state.waker.borrow_mut() = Some(cx.waker().clone());
        let polled = self.future.as_mut().poll(cx);
        if polled.is_ready() {
            self.state.finished.set(true);
        }
        polled
    }
}

/// A task on the page's microtask queue. Dropping the handle aborts it.
pub struct WebTask(Rc<TaskState>);

impl TaskHandle for WebTask {
    fn abort(&self) {
        self.0.aborted.set(true);
        if let Some(waker) = self.0.waker.borrow_mut().take() {
            waker.wake();
        }
    }

    fn is_finished(&self) -> bool {
        self.0.finished.get()
    }
}

impl Drop for WebTask {
    fn drop(&mut self) {
        self.abort();
    }
}

/// Local tasks through `wasm_bindgen_futures::spawn_local`.
#[derive(Debug, Default, Clone, Copy)]
pub struct WebSpawner;

impl Spawner for WebSpawner {
    type Handle = WebTask;

    fn spawn(&self, future: impl Future<Output = ()> + 'static) -> WebTask {
        let state = Rc::new(TaskState::default());
        wasm_bindgen_futures::spawn_local(Abortable {
            future: Box::pin(future),
            state: state.clone(),
        });
        WebTask(state)
    }
}

// ---------------------------------------------------------------------------
// Transport
// ---------------------------------------------------------------------------

/// Aborts its fetch when dropped, so a dropped request future cancels the
/// request as the runtime requires.
struct InFlight(web_sys::AbortController);

impl Drop for InFlight {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// A response body read through its stream reader. Dropping it cancels the
/// stream.
pub struct BodySource {
    path: String,
    reader: web_sys::ReadableStreamDefaultReader,
    // Keeps the fetch alive while the body is read; dropped last.
    _request: InFlight,
}

impl ByteSource for BodySource {
    async fn next_chunk(&mut self) -> Result<Option<Vec<u8>>, RuntimeError> {
        loop {
            let read = JsFuture::from(self.reader.read())
                .await
                .map_err(|error| network(&self.path, "read stream", &error))?;
            let done = Reflect::get(&read, &JsValue::from_str("done"))
                .ok()
                .and_then(|value| value.as_bool())
                .unwrap_or(true);
            if done {
                return Ok(None);
            }
            let value = Reflect::get(&read, &JsValue::from_str("value"))
                .map_err(|error| network(&self.path, "read stream", &error))?;
            let bytes = value
                .dyn_into::<Uint8Array>()
                .map_err(|value| network(&self.path, "read stream", &value))?
                .to_vec();
            // An empty read is not an end of stream.
            if !bytes.is_empty() {
                return Ok(Some(bytes));
            }
        }
    }
}

impl Drop for BodySource {
    fn drop(&mut self) {
        let _ = self.reader.cancel();
    }
}

/// Same-origin requests carrying the session cookie.
#[derive(Debug, Default)]
pub struct WebTransport;

impl WebTransport {
    async fn send(
        &self,
        path: &str,
        method: &str,
        accept: &str,
        body: Option<String>,
    ) -> Result<(web_sys::Response, InFlight), RuntimeError> {
        let controller = web_sys::AbortController::new()
            .map_err(|error| network(path, "abort controller", &error))?;
        let init = web_sys::RequestInit::new();
        init.set_method(method);
        init.set_credentials(web_sys::RequestCredentials::SameOrigin);
        init.set_redirect(web_sys::RequestRedirect::Error);
        init.set_signal(Some(&controller.signal()));
        let headers =
            web_sys::Headers::new().map_err(|error| network(path, "request headers", &error))?;
        headers
            .set("Accept", accept)
            .map_err(|error| network(path, "request headers", &error))?;
        if let Some(body) = body {
            headers
                .set("Content-Type", "application/json")
                .map_err(|error| network(path, "request headers", &error))?;
            init.set_body(&JsValue::from_str(&body));
        }
        init.set_headers(&headers);
        let request = web_sys::Request::new_with_str_and_init(path, &init)
            .map_err(|error| network(path, "request build", &error))?;
        let in_flight = InFlight(controller);
        let response = JsFuture::from(window().fetch_with_request(&request))
            .await
            .map_err(|error| network(path, "request send", &error))?
            .dyn_into::<web_sys::Response>()
            .map_err(|value| network(path, "request send", &value))?;
        Ok((response, in_flight))
    }

    async fn body_text(path: &str, response: &web_sys::Response) -> Result<String, RuntimeError> {
        let unreadable = || RuntimeError::UnreadableResponse {
            path: path.to_owned(),
        };
        let text = JsFuture::from(response.text().map_err(|_| unreadable())?)
            .await
            .map_err(|_| unreadable())?;
        text.as_string().ok_or_else(unreadable)
    }

    /// The daemon's own message for a non-2xx answer.
    async fn refusal(path: &str, response: &web_sys::Response) -> RuntimeError {
        let message = Self::body_text(path, response)
            .await
            .map(|body| super::daemon_error_message(body.as_bytes()))
            .unwrap_or_default();
        RuntimeError::Daemon {
            path: path.to_owned(),
            status: response.status(),
            message,
        }
    }

    async fn text(path: &str, response: &web_sys::Response) -> Result<String, RuntimeError> {
        if !response.ok() {
            return Err(Self::refusal(path, response).await);
        }
        Self::body_text(path, response).await
    }

    fn stream(
        path: &str,
        response: &web_sys::Response,
        request: InFlight,
    ) -> Result<BodySource, RuntimeError> {
        let reader = response
            .body()
            .ok_or_else(|| RuntimeError::UnreadableResponse {
                path: path.to_owned(),
            })?
            .get_reader()
            .dyn_into::<web_sys::ReadableStreamDefaultReader>()
            .map_err(|value| network(path, "read stream", &value))?;
        Ok(BodySource {
            path: path.to_owned(),
            reader,
            _request: request,
        })
    }
}

impl Transport for WebTransport {
    type Sse = BodySource;
    type Frames = BodySource;

    async fn get(&self, path: &str) -> Result<String, RuntimeError> {
        let (response, _request) = self.send(path, "GET", "application/json", None).await?;
        Self::text(path, &response).await
    }

    async fn post_encoded(&self, path: &str, body: String) -> Result<String, RuntimeError> {
        let (response, _request) = self
            .send(path, "POST", "application/json", Some(body))
            .await?;
        Self::text(path, &response).await
    }

    async fn open_sse(&self, path: &str) -> Result<BodySource, RuntimeError> {
        let (response, request) = self.send(path, "GET", "text/event-stream", None).await?;
        if !response.ok() {
            return Err(Self::refusal(path, &response).await);
        }
        Self::stream(path, &response, request)
    }

    async fn open_history(&self, path: &str, body: String) -> Result<BodySource, RuntimeError> {
        let (response, request) = self
            .send(path, "POST", "text/event-stream", Some(body))
            .await?;
        if !response.ok() {
            return Err(Self::refusal(path, &response).await);
        }
        if !event_stream_response(&response) {
            return Err(RuntimeError::BindingRequest(
                "seat history did not return a nonempty event stream response".to_owned(),
            ));
        }
        Self::stream(path, &response, request)
    }
}

// ---------------------------------------------------------------------------
// Authentication
// ---------------------------------------------------------------------------

/// The page already holds the session cookie the launch redirect set; the
/// browser cannot mint, adopt or redeem in place. A successor session loads
/// in a new document.
#[derive(Debug, Default)]
pub struct WebAuth {
    transport: WebTransport,
}

impl Auth for WebAuth {
    async fn establish(&self, mode: EstablishMode) -> Result<EstablishedSession, RuntimeError> {
        if mode != EstablishMode::CookiePresent {
            return Err(RuntimeError::BindingRequest(
                "a browser page only reads the session its launch cookie names".to_owned(),
            ));
        }
        let text = self.transport.get(SESSION_CURRENT_PATH).await?;
        let (current, session) = decode_session(&text)?;
        Ok(EstablishedSession {
            session_id: session.session_id,
            current,
        })
    }

    async fn redeem(&self, _launch_url: &str) -> Result<EstablishedSession, RuntimeError> {
        Err(RuntimeError::BindingRequest(
            "a browser page navigates to a successor's launch URL instead of redeeming it"
                .to_owned(),
        ))
    }

    fn replacement(&self) -> ReplacementMode {
        ReplacementMode::Navigate
    }
}

// ---------------------------------------------------------------------------
// Store
// ---------------------------------------------------------------------------

/// `localStorage`. Private windows and blocked site data make it absent or
/// throwing; reads then find nothing and writes report the refusal.
#[derive(Debug, Default)]
pub struct WebStore;

impl WebStore {
    fn storage() -> Option<web_sys::Storage> {
        window().local_storage().ok().flatten()
    }
}

impl Store for WebStore {
    fn get(&self, key: &str) -> Option<String> {
        Self::storage()?.get_item(key).ok().flatten()
    }

    fn set(&self, key: &str, value: &str) -> Result<(), RuntimeError> {
        Self::storage()
            .ok_or(RuntimeError::StoreWrite)?
            .set_item(key, value)
            .map_err(|_| RuntimeError::StoreWrite)
    }

    fn remove(&self, key: &str) {
        if let Some(storage) = Self::storage() {
            let _ = storage.remove_item(key);
        }
    }
}

// ---------------------------------------------------------------------------
// The platform
// ---------------------------------------------------------------------------

/// The browser host for a page served by the daemon.
#[derive(Debug, Default)]
pub struct WebPlatform {
    clock: WebClock,
    spawner: WebSpawner,
    transport: WebTransport,
    auth: WebAuth,
    store: WebStore,
}

impl WebPlatform {
    pub fn new() -> Self {
        Self::default()
    }

    /// The page's address, for the `presentation_id` a prepared document
    /// names.
    pub fn document_url(&self) -> String {
        window().location().href().unwrap_or_default()
    }

    /// Replace the page's address without navigating or adding a history
    /// entry, keeping the entry's state.
    pub fn replace_location(&self, url: &str) -> Result<(), RuntimeError> {
        let history = window()
            .history()
            .map_err(|error| RuntimeError::BindingRequest(js_message(&error)))?;
        let state = history.state().unwrap_or(JsValue::NULL);
        history
            .replace_state_with_url(&state, "", Some(url))
            .map_err(|error| RuntimeError::BindingRequest(js_message(&error)))
    }

    /// `path` on this page's origin, as an absolute URL.
    pub fn same_origin_url(&self, path: &str) -> Option<String> {
        let base = window().location().href().ok()?;
        let url = web_sys::Url::new_with_base(path, &base).ok()?;
        let origin = window().location().origin().ok()?;
        (url.origin() == origin).then(|| url.href())
    }

    /// The page's size in CSS pixels and its device pixel ratio.
    pub fn viewport(&self) -> ryeos_client_base::ui::BrowserViewport {
        let window = window();
        let size = |value: Result<JsValue, JsValue>| {
            value
                .ok()
                .and_then(|value| value.as_f64())
                .map_or(0, |pixels| pixels.max(0.0) as u32)
        };
        ryeos_client_base::ui::BrowserViewport {
            width: size(window.inner_width()),
            height: size(window.inner_height()),
            device_pixel_ratio: window.device_pixel_ratio() as f32,
        }
    }
}

impl Platform for WebPlatform {
    type Clock = WebClock;
    type Spawner = WebSpawner;
    type Transport = WebTransport;
    type Auth = WebAuth;
    type Store = WebStore;

    fn clock(&self) -> &WebClock {
        &self.clock
    }

    fn spawner(&self) -> &WebSpawner {
        &self.spawner
    }

    fn transport(&self) -> &WebTransport {
        &self.transport
    }

    fn auth(&self) -> &WebAuth {
        &self.auth
    }

    fn store(&self) -> &WebStore {
        &self.store
    }

    fn random_bytes(&self, out: &mut [u8]) {
        window()
            .crypto()
            .expect("the window exposes crypto")
            .get_random_values_with_u8_array(out)
            .expect("crypto.getRandomValues fills the buffer");
    }
}

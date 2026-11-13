//! Session establishment and the running loop.
//!
//! The page already holds the launch cookie. Boot reads the session it names,
//! chooses the presentation (a prepared document's `presentation_id`, else a
//! fresh one under the session's root), claims the seat and builds the core.
//! That work is async and finishes in a slot that `poll` takes from.

use std::cell::RefCell;
use std::rc::Rc;

use ryeos_client_base::ui::{RyeOsCore, RyeOsEvent};
use ryeos_client_runtime::ClientRuntime;
use ryeos_client_runtime::documents::{DocumentOffer, DocumentState, document_path};
use ryeos_client_runtime::inbox::WakeFn;
use ryeos_client_runtime::platform::{Clock, Platform};
use ryeos_client_runtime::recovery::ReconnectChoice;
use ryeos_client_runtime::runtime::{Drained, StopReason};

#[cfg(target_arch = "wasm32")]
type Host = ryeos_client_runtime::platform::web::WebPlatform;
#[cfg(not(target_arch = "wasm32"))]
type Host = ryeos_client_runtime::platform::native::NativePlatform;

/// What one start, retry, inspection or replacement left behind.
pub enum Outcome {
    /// The seat is claimed and the core installed.
    Running(Box<Running>),
    /// The seat cannot be driven; the operator chooses what happens next.
    Recovery(Box<Recovery>),
    /// Nothing can continue on this page.
    Failed(String),
}

/// Where the asynchronous boot work leaves its outcome.
#[derive(Default)]
pub struct Boot {
    slot: Rc<RefCell<Option<Outcome>>>,
    wake: Option<WakeFn>,
}

impl Boot {
    pub fn start(&mut self, waker: rune::runtime::Waker) {
        let wake: WakeFn = Rc::new(move || waker());
        self.wake = Some(wake.clone());
        start(self.slot.clone(), wake);
    }

    pub fn take(&mut self) -> Option<Outcome> {
        self.slot.borrow_mut().take()
    }

    /// Carry out `action` on `recovery` in the background; its outcome
    /// arrives through [`Boot::take`].
    pub fn recover(&mut self, recovery: Box<Recovery>, action: RecoveryAction) {
        let Some(wake) = self.wake.clone() else {
            return;
        };
        recover(self.slot.clone(), wake, recovery, action);
    }
}

/// An operator's decision on the recovery screen.
#[derive(Debug, Clone)]
pub enum RecoveryAction {
    /// Repeat the failed start with its frozen request (only before a core
    /// was ever installed under this writer).
    RetryExact,
    /// Reconcile the retained append, then read the writer read-only.
    Inspect,
    /// Resume, take over, or open an independent presentation.
    Choose(ReconnectChoice),
}

/// A seat that cannot be driven and what may be done about it. Nothing here
/// declares the old writer dead: choices come from an observation, and the
/// node checks the predecessor again when a claim arrives.
pub struct Recovery {
    pub message: String,
    /// Whether the start may be repeated exactly.
    pub exact_retry: bool,
    /// Whether a presentation was prepared, so its writer can be inspected.
    pub can_inspect: bool,
    /// The writer last observed, and the replacements it supports.
    pub observed: Option<(String, Vec<ReconnectChoice>)>,
    /// What the decisions act on; only the browser host carries them out.
    #[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
    held: Held,
}

enum Held {
    /// A seat that was driven and then lost, with its core.
    #[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
    Live(Box<Running>),
    /// A start that failed before a core was installed.
    #[cfg(target_arch = "wasm32")]
    Unstarted(Box<Unstarted>),
    /// Nothing left to act on: a decision is under way or was refused.
    #[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
    Nothing,
}

#[cfg(target_arch = "wasm32")]
struct Unstarted {
    platform: Rc<Host>,
    runtime: ClientRuntime<Host>,
    session: ryeos_client_base::ui::BrowserSession,
    mode: ryeos_client_runtime::runtime::StartMode,
}

impl Recovery {
    /// The seat of a running presentation stopped.
    pub fn lost(mut running: Box<Running>, message: String) -> Box<Recovery> {
        // Hints and tails stop; effects already admitted keep running.
        running.runtime.begin_close();
        Box::new(Recovery {
            message,
            exact_retry: false,
            can_inspect: running.runtime.has_prepared_presentation(),
            observed: None,
            held: Held::Live(running),
        })
    }
}

fn readable(stop: &StopReason) -> String {
    match stop {
        StopReason::Fatal(message) | StopReason::Fenced(message) => message.clone(),
        StopReason::Unavailable(unavailable) => unavailable.message().to_owned(),
    }
}

#[cfg(target_arch = "wasm32")]
fn start(slot: Rc<RefCell<Option<Outcome>>>, wake: WakeFn) {
    wasm_bindgen_futures::spawn_local(async move {
        let outcome = establish(wake.clone(), None).await;
        *slot.borrow_mut() = Some(outcome);
        wake();
    });
}

#[cfg(not(target_arch = "wasm32"))]
fn start(slot: Rc<RefCell<Option<Outcome>>>, wake: WakeFn) {
    // The native window launches through the CLI's mint; until it exists the
    // headless host only renders fixtures.
    *slot.borrow_mut() = Some(Outcome::Failed(
        "this build renders fixtures only; open RyeOS in the browser".to_owned(),
    ));
    wake();
}

#[cfg(target_arch = "wasm32")]
fn recover(
    slot: Rc<RefCell<Option<Outcome>>>,
    wake: WakeFn,
    recovery: Box<Recovery>,
    action: RecoveryAction,
) {
    wasm_bindgen_futures::spawn_local(async move {
        let outcome = act(wake.clone(), recovery, action).await;
        *slot.borrow_mut() = Some(outcome);
        wake();
    });
}

#[cfg(not(target_arch = "wasm32"))]
fn recover(
    slot: Rc<RefCell<Option<Outcome>>>,
    wake: WakeFn,
    recovery: Box<Recovery>,
    _action: RecoveryAction,
) {
    *slot.borrow_mut() = Some(Outcome::Recovery(recovery));
    wake();
}

/// One recovery decision. A failure keeps the recovery screen with the
/// reason, and a failed observation never leaves older choices looking
/// current.
#[cfg(target_arch = "wasm32")]
async fn act(wake: WakeFn, mut recovery: Box<Recovery>, action: RecoveryAction) -> Outcome {
    match action {
        RecoveryAction::RetryExact => {
            let Held::Unstarted(unstarted) = std::mem::replace(&mut recovery.held, Held::Nothing)
            else {
                recovery.message = "this presentation can no longer retry its exact request".into();
                return Outcome::Recovery(recovery);
            };
            let Unstarted {
                platform,
                runtime,
                session,
                mode,
            } = *unstarted;
            begin(wake, platform, runtime, session, mode).await
        }
        RecoveryAction::Inspect => {
            recovery.observed = None;
            let observed = match &mut recovery.held {
                Held::Live(running) => running
                    .runtime
                    .observe_reconnect(&mut running.core)
                    .await
                    .map(|(observation, choices, effects)| {
                        running.runtime.submit(effects);
                        (observation, choices)
                    }),
                Held::Unstarted(unstarted) => {
                    unstarted.runtime.observe_reconnect_without_core().await
                }
                Held::Nothing => return Outcome::Recovery(recovery),
            };
            match observed {
                Ok((observation, choices)) => {
                    recovery.observed = Some((writer_label(&observation), choices));
                }
                Err(error) => recovery.message = error.to_string(),
            }
            Outcome::Recovery(recovery)
        }
        RecoveryAction::Choose(choice) => {
            let mode = match &recovery.held {
                Held::Live(running) => running.runtime.reconnect_mode(&choice, &running.core),
                Held::Unstarted(unstarted) => match &unstarted.mode {
                    // Refused before preparing anything: the choice applies
                    // to the presentation the start asked for.
                    ryeos_client_runtime::runtime::StartMode::Retained {
                        presentation_id, ..
                    } if !unstarted.runtime.has_prepared_presentation() => unstarted
                        .runtime
                        .reconnect_mode_for(&choice, presentation_id),
                    _ => unstarted.runtime.reconnect_mode_without_core(&choice),
                },
                Held::Nothing => return Outcome::Recovery(recovery),
            };
            let mode = match mode {
                Ok(mode) => mode,
                Err(error) => {
                    recovery.message = error.to_string();
                    return Outcome::Recovery(recovery);
                }
            };
            // The replaced runtime stops delivering; whatever it admitted
            // still finishes, unseen.
            match std::mem::replace(&mut recovery.held, Held::Nothing) {
                Held::Live(mut running) => running.runtime.detach(),
                Held::Unstarted(mut unstarted) => unstarted.runtime.detach(),
                Held::Nothing => {}
            }
            establish(wake, Some(mode)).await
        }
    }
}

#[cfg(target_arch = "wasm32")]
fn writer_label(observation: &ryeos_client_base::ui::UiPresentationProducerObservation) -> String {
    use ryeos_client_base::ui::UiObservedPresentationProducer as Writer;
    match &observation.producer {
        Writer::Unclaimed => "unclaimed",
        Writer::Active { .. } => "active",
        Writer::Relinquished { .. } => "relinquished",
        Writer::Closed { .. } => "closed",
    }
    .to_owned()
}

/// Read the session and start: the page's own selection when `mode` is
/// `None`, else exactly `mode` (a replacement the operator chose).
#[cfg(target_arch = "wasm32")]
async fn establish(
    wake: WakeFn,
    mode: Option<ryeos_client_runtime::runtime::StartMode>,
) -> Outcome {
    use ryeos_client_runtime::admission::{prepared_presentation_selection, query_values};
    use ryeos_client_runtime::platform::{Auth, EstablishMode};
    use ryeos_client_runtime::runtime::StartMode;
    use ryeos_client_runtime::seat::{RetainedClaim, RetryPolicy};

    let platform = Rc::new(Host::new());
    let established = match platform
        .auth()
        .establish(EstablishMode::CookiePresent)
        .await
    {
        Ok(established) => established,
        Err(error) => {
            return Outcome::Failed(format!("The UI session could not be read: {error}"));
        }
    };
    let mode = match mode {
        Some(mode) => mode,
        None => {
            let selections = query_values(&platform.document_url(), "presentation_id");
            match prepared_presentation_selection(&established.current, &selections) {
                Ok(Some(prepared)) => StartMode::Retained {
                    presentation_id: prepared.presentation.presentation_id,
                    claim: RetainedClaim::Reopen,
                },
                Ok(None) => StartMode::Fresh,
                Err(error) => return Outcome::Failed(error.to_string()),
            }
        }
    };
    let session = match serde_json::from_value::<ryeos_client_base::ui::BrowserSession>(
        established.current,
    ) {
        Ok(session) => session,
        Err(error) => {
            return Outcome::Failed(format!("The UI session record is unreadable: {error}"));
        }
    };
    let runtime = ClientRuntime::new(platform.clone(), wake.clone(), RetryPolicy::WEB)
        .with_adapter(ryeos_client_runtime::platform::web::web_adapter())
        .with_documents();
    begin(wake, platform, runtime, session, mode).await
}

/// Claim, replay and install. A failure keeps the runtime, so the frozen
/// request can be repeated exactly or the writer inspected.
#[cfg(target_arch = "wasm32")]
async fn begin(
    wake: WakeFn,
    platform: Rc<Host>,
    mut runtime: ClientRuntime<Host>,
    session: ryeos_client_base::ui::BrowserSession,
    mode: ryeos_client_runtime::runtime::StartMode,
) -> Outcome {
    use ryeos_client_runtime::replay::CoreConfig;

    let viewport = platform.viewport();
    let started = runtime
        .start(
            session.clone(),
            viewport,
            mode.clone(),
            // The runtime offers new presentation documents as links here.
            CoreConfig {
                supports_independent_documents: true,
            },
        )
        .await;
    let (core, effects) = match started {
        Ok(started) => started,
        // Another writer holds the seat: show it and its replacement choices
        // at once. Nothing was claimed, so retry rechecks it as before.
        Err(ryeos_client_runtime::RuntimeError::WriterHeld(observation)) => {
            let choices = ryeos_client_runtime::recovery::replacement_choices(&observation);
            return Outcome::Recovery(Box::new(Recovery {
                message: "This presentation is open somewhere else.".to_owned(),
                exact_retry: true,
                can_inspect: false,
                observed: Some((writer_label(&observation), choices)),
                held: Held::Unstarted(Box::new(Unstarted {
                    platform,
                    runtime,
                    session,
                    mode,
                })),
            }));
        }
        Err(error) => {
            let can_inspect = runtime.has_prepared_presentation();
            return Outcome::Recovery(Box::new(Recovery {
                message: format!("The presentation could not be opened: {error}"),
                exact_retry: true,
                can_inspect,
                observed: None,
                held: Held::Unstarted(Box::new(Unstarted {
                    platform,
                    runtime,
                    session,
                    mode,
                })),
            }));
        }
    };
    match install(wake, platform, runtime, core, effects) {
        Ok(running) => Outcome::Running(Box::new(running)),
        // A core was installed under this writer: it is not started again
        // in place, only replaced by an explicit, observed choice.
        Err((running, message)) => Outcome::Recovery(Recovery::lost(Box::new(running), message)),
    }
}

/// Finish a start: the resume address, hash routes, then the running loop.
#[cfg(target_arch = "wasm32")]
fn install(
    wake: WakeFn,
    platform: Rc<Host>,
    runtime: ClientRuntime<Host>,
    core: RyeOsCore,
    effects: Vec<ryeos_client_base::ui::RyeOsEffect>,
) -> Result<Running, (Running, String)> {
    let producer = runtime.producer();
    let mut running = Running::new(platform, runtime, core, effects);
    // The address becomes a resume coordinate only now, after membership,
    // the producer claim, retained replay and the core are all in place: a
    // reload reopens exactly this presentation.
    if let Some(producer) = producer {
        let rewritten = ryeos_client_runtime::admission::retained_presentation_url(
            &running.platform.document_url(),
            &producer.presentation_id,
        )
        .and_then(|url| running.platform.replace_location(&url));
        if let Err(error) = rewritten {
            return Err((running, error.to_string()));
        }
    }
    // Hash routes reach the core once it is installed: the page's own first,
    // then each change, folded in order by `poll`.
    let routes: Rc<RefCell<Vec<String>>> = Rc::default();
    let initial = ryeos_client_runtime::platform::web::current_route();
    if !initial.is_empty() {
        routes.borrow_mut().push(initial);
    }
    let queue = routes.clone();
    let listener = match ryeos_client_runtime::platform::web::on_route_change(move |route| {
        queue.borrow_mut().push(route);
        wake();
    }) {
        Ok(listener) => listener,
        Err(error) => return Err((running, error.to_string())),
    };
    running.routes = Some(Routes {
        queue: routes,
        _listener: listener,
    });
    Ok(running)
}

/// Location hash changes waiting for the core.
#[cfg(target_arch = "wasm32")]
struct Routes {
    queue: Rc<RefCell<Vec<String>>>,
    _listener: ryeos_client_runtime::platform::web::RouteListener,
}

/// A claimed seat and its core.
pub struct Running {
    platform: Rc<Host>,
    runtime: ClientRuntime<Host>,
    core: RyeOsCore,
    last_tick_ms: u64,
    documents_generation: u64,
    #[cfg(target_arch = "wasm32")]
    routes: Option<Routes>,
}

impl Running {
    #[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
    fn new(
        platform: Rc<Host>,
        mut runtime: ClientRuntime<Host>,
        core: RyeOsCore,
        effects: Vec<ryeos_client_base::ui::RyeOsEffect>,
    ) -> Running {
        runtime.submit(effects);
        runtime.after_mutation(&core);
        Running {
            platform,
            runtime,
            core,
            last_tick_ms: 0,
            documents_generation: 0,
            #[cfg(target_arch = "wasm32")]
            routes: None,
        }
    }

    pub fn core(&self) -> &RyeOsCore {
        &self.core
    }

    /// The presentation documents offered as links, each with the absolute
    /// URL that opens it once it is prepared.
    pub fn document_offers(&self) -> Vec<(DocumentOffer, Option<String>)> {
        let Some(documents) = self.runtime.documents() else {
            return Vec::new();
        };
        documents
            .offers()
            .into_iter()
            .map(|offer| {
                let link = match &offer.state {
                    DocumentState::Ready { presentation_id } => document_path(presentation_id)
                        .ok()
                        .and_then(|path| self.link(&path)),
                    _ => None,
                };
                (offer, link)
            })
            .collect()
    }

    #[cfg(target_arch = "wasm32")]
    fn link(&self, path: &str) -> Option<String> {
        self.platform.same_origin_url(path)
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn link(&self, _path: &str) -> Option<String> {
        None
    }

    pub fn retry_document(&mut self, id: u64) {
        self.runtime.retry_document(id);
    }

    pub fn dismiss_document(&self, id: u64) {
        if let Some(documents) = self.runtime.documents() {
            documents.dismiss(id);
        }
    }

    pub fn dispatch(&mut self, event: RyeOsEvent) {
        let effects = self.core.dispatch(event);
        self.runtime.submit(effects);
        self.runtime.after_mutation(&self.core);
    }

    /// Run a key through the core's keymap. False when the core leaves the
    /// key alone, so Rune may use it.
    pub fn apply_key(&mut self, event: ryeos_client_base::ui::RyeOsKeyEvent) -> bool {
        use ryeos_client_base::ui::{RyeOsKeyCommand, ryeos_key_command};
        let command = ryeos_key_command(event, self.core.key_context());
        if matches!(command, RyeOsKeyCommand::Ignore | RyeOsKeyCommand::Quit) {
            return false;
        }
        let effects = self.core.apply_key_command(command);
        self.runtime.submit(effects);
        self.runtime.after_mutation(&self.core);
        true
    }

    /// Fold what the runtime's tasks produced, then the frame tick when it is
    /// due. True when the view must be rebuilt.
    pub fn poll(&mut self) -> Result<bool, String> {
        let Drained {
            effects,
            changed,
            stop,
            ..
        } = self.runtime.drain(&mut self.core);
        self.runtime.submit(effects);
        if let Some(stop) = stop {
            return Err(format!(
                "The seat can no longer be driven: {}",
                readable(&stop)
            ));
        }
        let mut changed = changed;
        if let Some(documents) = self.runtime.documents()
            && documents.generation() != self.documents_generation
        {
            self.documents_generation = documents.generation();
            changed = true;
        }
        #[cfg(target_arch = "wasm32")]
        {
            let routes: Vec<String> = self
                .routes
                .as_ref()
                .map(|routes| routes.queue.borrow_mut().drain(..).collect())
                .unwrap_or_default();
            for route in routes {
                self.dispatch(RyeOsEvent::RouteChanged { route });
                changed = true;
            }
        }
        let now_ms = self.platform.clock().now_ms();
        if now_ms.saturating_sub(self.last_tick_ms) >= super::TICK.as_millis() as u64 {
            self.last_tick_ms = now_ms;
            let before = self.core.generation;
            self.dispatch(RyeOsEvent::Tick { now_ms });
            for message in self.runtime.take_layout_errors() {
                self.core
                    .notice(message, ryeos_client_base::ui::view_model::RyeOsTone::Warn);
            }
            changed |= self.core.generation != before;
        }
        Ok(changed)
    }
}

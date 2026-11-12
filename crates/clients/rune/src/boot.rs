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
use ryeos_client_runtime::inbox::WakeFn;
use ryeos_client_runtime::platform::{Clock, Platform};
use ryeos_client_runtime::runtime::Drained;

#[cfg(target_arch = "wasm32")]
type Host = ryeos_client_runtime::platform::web::WebPlatform;
#[cfg(not(target_arch = "wasm32"))]
type Host = ryeos_client_runtime::platform::native::NativePlatform;

type Outcome = Result<Running, String>;

/// Where the boot task leaves its result.
#[derive(Default)]
pub struct Boot {
    slot: Rc<RefCell<Option<Outcome>>>,
}

impl Boot {
    pub fn start(&mut self, waker: rune::runtime::Waker) {
        let slot = self.slot.clone();
        let wake: WakeFn = Rc::new(move || waker());
        start(slot, wake);
    }

    pub fn take(&mut self) -> Option<Outcome> {
        self.slot.borrow_mut().take()
    }
}

#[cfg(target_arch = "wasm32")]
fn start(slot: Rc<RefCell<Option<Outcome>>>, wake: WakeFn) {
    wasm_bindgen_futures::spawn_local(async move {
        let outcome = establish(wake.clone()).await;
        *slot.borrow_mut() = Some(outcome);
        wake();
    });
}

#[cfg(not(target_arch = "wasm32"))]
fn start(slot: Rc<RefCell<Option<Outcome>>>, wake: WakeFn) {
    // The native window launches through the CLI's mint; until it exists the
    // headless host only renders fixtures.
    *slot.borrow_mut() = Some(Err(
        "this build renders fixtures only; open RyeOS in the browser".to_owned(),
    ));
    wake();
}

#[cfg(target_arch = "wasm32")]
async fn establish(wake: WakeFn) -> Outcome {
    use ryeos_client_runtime::admission::{prepared_presentation_selection, query_values};
    use ryeos_client_runtime::platform::{Auth, EstablishMode};
    use ryeos_client_runtime::replay::CoreConfig;
    use ryeos_client_runtime::runtime::StartMode;
    use ryeos_client_runtime::seat::{RetainedClaim, RetryPolicy};

    let platform = Rc::new(Host::new());
    let established = platform
        .auth()
        .establish(EstablishMode::CookiePresent)
        .await
        .map_err(|error| format!("The UI session could not be read: {error}"))?;
    let selections = query_values(&platform.document_url(), "presentation_id");
    let selection = prepared_presentation_selection(&established.current, &selections)
        .map_err(|error| error.to_string())?;
    let session =
        serde_json::from_value::<ryeos_client_base::ui::BrowserSession>(established.current)
            .map_err(|error| format!("The UI session record is unreadable: {error}"))?;
    let mode = match selection {
        Some(prepared) => StartMode::Retained {
            presentation_id: prepared.presentation.presentation_id,
            claim: RetainedClaim::Fresh,
        },
        None => StartMode::Fresh,
    };
    let viewport = platform.viewport();
    let mut runtime = ClientRuntime::new(platform.clone(), wake.clone(), RetryPolicy::WEB)
        .with_adapter(ryeos_client_runtime::platform::web::web_adapter());
    let (core, effects) = runtime
        .start(session, viewport, mode, CoreConfig::default())
        .await
        .map_err(|error| format!("The presentation could not be opened: {error}"))?;
    // Hash routes reach the core once it is installed: the page's own first,
    // then each change, folded in order by `poll`.
    let routes: Rc<RefCell<Vec<String>>> = Rc::default();
    let initial = ryeos_client_runtime::platform::web::current_route();
    if !initial.is_empty() {
        routes.borrow_mut().push(initial);
    }
    let queue = routes.clone();
    let listener = ryeos_client_runtime::platform::web::on_route_change(move |route| {
        queue.borrow_mut().push(route);
        wake();
    })
    .map_err(|error| error.to_string())?;
    let mut running = Running::new(platform, runtime, core, effects);
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
            #[cfg(target_arch = "wasm32")]
            routes: None,
        }
    }

    pub fn core(&self) -> &RyeOsCore {
        &self.core
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
            return Err(format!("The seat can no longer be driven: {stop:?}"));
        }
        let mut changed = changed;
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

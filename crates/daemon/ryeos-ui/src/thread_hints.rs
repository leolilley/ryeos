//! Scoped fan-out of thread hints to browser sessions.
//!
//! A thread hint names a thread and its status, so it is thread evidence and
//! is judged like any other read of that thread: a session receives it only
//! when one of its attachments, as a seat, may observe the thread by the
//! thread's signed subject (owner, and live-project scope for a project
//! attachment). The judgement happens here, per session, before the hint
//! enters that session's bus; nothing is filtered in the client.
//!
//! What is cached is the signed subject of a thread, keyed by thread id. Its
//! owner and project authority are fixed when the thread is created, so the
//! entry never goes stale and has no invalidation beyond a size bound. The
//! per-session verdict is never cached: it is recomputed from the session's
//! live attachments on every hint, so a session's principal (fixed), its
//! attachments and their project authority are always read as they are now.
//!
//! A thread whose subject cannot be read or does not agree with the event is
//! undecidable. Its hint goes to no session and is counted and logged
//! server-side.

use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use ryeos_app::state::AppState;
use ryeos_app::state_store::AuthoritativeThreadSubject;
use serde_json::{Value, json};

use crate::state::UiState;
use crate::thread_authorization::{SessionObserver, load_thread_subject};

/// Most thread subjects held at once. Past it the cache is dropped whole; an
/// entry is only a saved read and is re-derived on demand.
const MAX_CACHED_SUBJECTS: usize = 8192;

/// The event name hints travel under on a session bus.
const HINT_EVENT: &str = "thread.hint";

#[derive(Default)]
pub struct ThreadHintFanout {
    subjects: Mutex<HashMap<String, Arc<AuthoritativeThreadSubject>>>,
    undecided: AtomicU64,
}

impl ThreadHintFanout {
    pub fn new() -> Self {
        Self::default()
    }

    /// Hints that went to no session because the thread's authority could not
    /// be decided.
    pub fn undecided_count(&self) -> u64 {
        self.undecided.load(Ordering::Relaxed)
    }

    /// The signed subject of `thread_id` for an event of `chain_root_id`, or
    /// `None` when it is undecidable.
    async fn subject(
        &self,
        state: &AppState,
        thread_id: &str,
        chain_root_id: &str,
    ) -> Option<Arc<AuthoritativeThreadSubject>> {
        let cached = self.subjects.lock().ok()?.get(thread_id).cloned();
        let subject = match cached {
            Some(subject) => subject,
            None => {
                let read_state = state.clone();
                let id = thread_id.to_owned();
                let loaded =
                    tokio::task::spawn_blocking(move || load_thread_subject(&read_state, &id))
                        .await;
                let subject = match loaded {
                    Ok(Ok(Some(subject))) => Arc::new(subject),
                    _ => return self.undecided(thread_id, "subject unreadable or absent"),
                };
                let mut cache = self.subjects.lock().ok()?;
                if cache.len() >= MAX_CACHED_SUBJECTS {
                    cache.clear();
                }
                cache.insert(thread_id.to_owned(), subject.clone());
                subject
            }
        };
        if subject.chain_root_id != chain_root_id {
            return self.undecided(thread_id, "event chain root contradicts the signed subject");
        }
        Some(subject)
    }

    fn undecided<T>(&self, thread_id: &str, reason: &'static str) -> Option<T> {
        self.undecided.fetch_add(1, Ordering::Relaxed);
        tracing::warn!(thread_id, reason, "thread hint withheld from every session");
        None
    }

    /// Publish one thread lifecycle hint to each session that may observe the
    /// thread.
    pub async fn publish_lifecycle(
        &self,
        state: &AppState,
        ui: &UiState,
        thread_id: &str,
        chain_root_id: &str,
        payload: Value,
    ) {
        let Some(subject) = self.subject(state, thread_id, chain_root_id).await else {
            return;
        };
        for (session_id, attachments) in ui.browser_sessions.hint_audiences() {
            if SessionObserver::new(&attachments).observes(state, &subject) {
                ui.session_bus
                    .publish(&session_id, HINT_EVENT, payload.clone());
            }
        }
    }

    /// Publish the activity of one interval. `counts` maps a thread to its
    /// event count and `chains` to its chain root. Each session is sent an
    /// activity hint only when it may observe at least one thread of the
    /// interval, and that hint carries only those threads and their count, so
    /// the activity of threads a session may not see is neither counted in
    /// nor signalled by what it receives.
    pub async fn publish_activity(
        &self,
        state: &AppState,
        ui: &UiState,
        counts: &HashMap<String, (String, u64)>,
    ) {
        let mut subjects = BTreeMap::new();
        for (thread_id, (chain_root_id, _)) in counts {
            if let Some(subject) = self.subject(state, thread_id, chain_root_id).await {
                subjects.insert(thread_id.as_str(), subject);
            }
        }
        if subjects.is_empty() {
            return;
        }
        for (session_id, attachments) in ui.browser_sessions.hint_audiences() {
            let observer = SessionObserver::new(&attachments);
            let mut thread_ids = Vec::new();
            let mut event_count = 0u64;
            for (thread_id, subject) in &subjects {
                if observer.observes(state, subject) {
                    thread_ids.push(*thread_id);
                    event_count += counts[*thread_id].1;
                }
            }
            if thread_ids.is_empty() {
                continue;
            }
            ui.session_bus.publish(
                &session_id,
                HINT_EVENT,
                json!({
                    "kind": "activity",
                    "thread_ids": thread_ids,
                    "event_count": event_count,
                }),
            );
        }
    }
}

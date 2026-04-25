//! Trace-capture harness for asserting spans in tests.
//!
//! # Capabilities
//!
//! - **Hierarchical capture** — parent/child relationships via [`tracing::Id`]
//! - **Late field recording** — captures fields added after span creation via
//!   [`tracing::Span::record`] (works with `tracing::field::Empty` placeholders)
//! - **Correct event attachment** — events attach to the *current* (entered) span,
//!   not the most recently created one
//! - **Lifecycle hooks** — tracks `on_enter`/`on_exit`/`on_close` for the
//!   underlying registry, enabling parent lookups and field updates
//!
//! # ⚠ Thread-safety limitation
//!
//! **This harness captures spans on the calling thread only.**
//!
//! It uses [`tracing::subscriber::set_default`], which installs a per-thread
//! dispatcher. Spans and events emitted from any of the following will be
//! **silently dropped**:
//!
//! - `tokio::spawn` on a multi-thread runtime
//! - `tokio::task::spawn_blocking`
//! - `std::thread::spawn`
//! - any future polled on a different thread than the one that called
//!   [`capture_traces`]
//!
//! ## Async tests
//!
//! Use the `current_thread` flavor so all work stays on the test thread:
//!
//! ```rust,ignore
//! #[tokio::test(flavor = "current_thread")]
//! async fn captures_async_work() {
//!     let (_, spans) = ryeos_tracing::test::capture_traces(|| {
//!         // run sync setup here
//!     });
//!     // For async work captured inside a closure, wrap it in
//!     // a current-thread runtime block_on inside the closure, OR
//!     // restructure the test to avoid spawning.
//!     # let _ = spans;
//! }
//! ```
//!
//! For tests that genuinely need cross-task capture across a multi-thread
//! tokio runtime or `std::thread::spawn`, this harness is **not** the right
//! tool. A serialized global-subscriber helper would be required; none is
//! provided yet because no current test needs it. See [the docs] for the
//! intended escape hatch when one is added.
//!
//! [the docs]: crate
//!
//! ## Other limitations
//!
//! - **Events without a current span are discarded** by the flat-span API.
//!   They remain observable via [`capture_full`] which returns a [`Capture`]
//!   struct including `orphan_events`.
//!
//! # Usage
//!
//! ```rust,ignore
//! use ryeos_tracing::test::{capture_traces, find_span};
//!
//! let (_, spans) = capture_traces(|| {
//!     let parent = tracing::info_span!("parent", foo = tracing::field::Empty);
//!     let _g = parent.enter();
//!     parent.record("foo", "bar");
//!     tracing::info!("hello");
//!     let _c = tracing::info_span!("child").entered();
//! });
//!
//! let parent = find_span(&spans, "parent").unwrap();
//! assert_eq!(parent.field("foo").unwrap(), "bar");
//! assert_eq!(parent.events.len(), 1);
//! assert_eq!(parent.children.len(), 1);
//! assert_eq!(parent.children[0].name, "child");
//! ```

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use tracing::{span, Event, Id, Level, Subscriber};
use tracing_subscriber::layer::Context;
use tracing_subscriber::prelude::*;
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::Layer;

/// A recorded span with its fields, child spans, and events.
#[derive(Debug, Clone)]
pub struct RecordedSpan {
    pub name: String,
    pub level: Level,
    pub target: String,
    pub fields: Vec<(String, String)>,
    pub children: Vec<RecordedSpan>,
    pub events: Vec<RecordedEvent>,
}

impl RecordedSpan {
    /// Look up a field by name, returning the recorded string value.
    pub fn field(&self, name: &str) -> Option<&str> {
        self.fields
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }
}

/// A recorded event with its level, message, and fields.
#[derive(Debug, Clone)]
pub struct RecordedEvent {
    pub level: Level,
    pub target: String,
    pub message: Option<String>,
    pub fields: Vec<(String, String)>,
}

impl RecordedEvent {
    pub fn field(&self, name: &str) -> Option<&str> {
        self.fields
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }
}

/// Full capture result: roots plus events emitted with no current span.
#[derive(Debug, Clone, Default)]
pub struct Capture {
    pub roots: Vec<RecordedSpan>,
    pub orphan_events: Vec<RecordedEvent>,
}

// ── Internal: in-progress span graph ────────────────────────────────────

#[derive(Default)]
struct CaptureState {
    nodes: HashMap<u64, SpanNode>,
    /// Root span ids in the order they were created.
    root_order: Vec<u64>,
    /// Events that fired with no enclosing span.
    orphan_events: Vec<RecordedEvent>,
}

struct SpanNode {
    name: String,
    level: Level,
    target: String,
    fields: Vec<(String, String)>,
    /// Child span ids in creation order.
    children: Vec<u64>,
    events: Vec<RecordedEvent>,
}

struct CaptureLayer {
    state: Arc<Mutex<CaptureState>>,
}

impl<S> Layer<S> for CaptureLayer
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_new_span(&self, attrs: &span::Attributes<'_>, id: &Id, ctx: Context<'_, S>) {
        let mut fields = Vec::new();
        attrs.values().record(&mut FieldRecorder(&mut fields));

        // Determine parent: explicit attrs.parent() takes precedence over current span.
        let parent_id = if let Some(pid) = attrs.parent() {
            Some(pid.into_u64())
        } else if attrs.is_contextual() {
            ctx.lookup_current().map(|sref| sref.id().into_u64())
        } else {
            None
        };

        let meta = attrs.metadata();
        let node = SpanNode {
            name: meta.name().to_string(),
            level: *meta.level(),
            target: meta.target().to_string(),
            fields,
            children: Vec::new(),
            events: Vec::new(),
        };

        let key = id.into_u64();
        if let Ok(mut state) = self.state.lock() {
            if let Some(pid) = parent_id {
                if let Some(parent) = state.nodes.get_mut(&pid) {
                    parent.children.push(key);
                } else {
                    // Parent unknown (created outside our layer scope). Treat as root.
                    state.root_order.push(key);
                }
            } else {
                state.root_order.push(key);
            }
            state.nodes.insert(key, node);
        }
    }

    fn on_record(&self, id: &Id, values: &span::Record<'_>, _ctx: Context<'_, S>) {
        let key = id.into_u64();
        if let Ok(mut state) = self.state.lock() {
            if let Some(node) = state.nodes.get_mut(&key) {
                values.record(&mut FieldRecorder(&mut node.fields));
            }
        }
    }

    fn on_event(&self, event: &Event<'_>, ctx: Context<'_, S>) {
        let mut fields = Vec::new();
        let mut message = None;
        event.record(&mut EventFieldRecorder {
            fields: &mut fields,
            message: &mut message,
        });

        let meta = event.metadata();
        let recorded = RecordedEvent {
            level: *meta.level(),
            target: meta.target().to_string(),
            message,
            fields,
        };

        // Attach to the current span (entered), falling back to event's parent
        // if available, else orphan.
        let target_id = ctx
            .event_span(event)
            .map(|sref| sref.id().into_u64())
            .or_else(|| ctx.lookup_current().map(|sref| sref.id().into_u64()));

        if let Ok(mut state) = self.state.lock() {
            if let Some(id) = target_id {
                if let Some(node) = state.nodes.get_mut(&id) {
                    node.events.push(recorded);
                    return;
                }
            }
            state.orphan_events.push(recorded);
        }
    }
}

struct FieldRecorder<'a>(&'a mut Vec<(String, String)>);

impl tracing::field::Visit for FieldRecorder<'_> {
    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        self.0.push((field.name().to_string(), value.to_string()));
    }
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        self.0
            .push((field.name().to_string(), format!("{:?}", value)));
    }
    fn record_u64(&mut self, field: &tracing::field::Field, value: u64) {
        self.0.push((field.name().to_string(), value.to_string()));
    }
    fn record_i64(&mut self, field: &tracing::field::Field, value: i64) {
        self.0.push((field.name().to_string(), value.to_string()));
    }
    fn record_bool(&mut self, field: &tracing::field::Field, value: bool) {
        self.0.push((field.name().to_string(), value.to_string()));
    }
    fn record_f64(&mut self, field: &tracing::field::Field, value: f64) {
        self.0.push((field.name().to_string(), value.to_string()));
    }
    fn record_error(
        &mut self,
        field: &tracing::field::Field,
        value: &(dyn std::error::Error + 'static),
    ) {
        self.0.push((field.name().to_string(), value.to_string()));
    }
}

struct EventFieldRecorder<'a> {
    fields: &'a mut Vec<(String, String)>,
    message: &'a mut Option<String>,
}

impl tracing::field::Visit for EventFieldRecorder<'_> {
    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        if field.name() == "message" {
            *self.message = Some(value.to_string());
        } else {
            self.fields
                .push((field.name().to_string(), value.to_string()));
        }
    }
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            *self.message = Some(format!("{:?}", value));
        } else {
            self.fields
                .push((field.name().to_string(), format!("{:?}", value)));
        }
    }
    fn record_u64(&mut self, field: &tracing::field::Field, value: u64) {
        self.fields
            .push((field.name().to_string(), value.to_string()));
    }
    fn record_i64(&mut self, field: &tracing::field::Field, value: i64) {
        self.fields
            .push((field.name().to_string(), value.to_string()));
    }
    fn record_bool(&mut self, field: &tracing::field::Field, value: bool) {
        self.fields
            .push((field.name().to_string(), value.to_string()));
    }
    fn record_f64(&mut self, field: &tracing::field::Field, value: f64) {
        self.fields
            .push((field.name().to_string(), value.to_string()));
    }
    fn record_error(
        &mut self,
        field: &tracing::field::Field,
        value: &(dyn std::error::Error + 'static),
    ) {
        self.fields
            .push((field.name().to_string(), value.to_string()));
    }
}

// ── Public capture entry points ─────────────────────────────────────────

/// Run a closure with a capture subscriber installed on the current thread,
/// returning the result and the recorded root spans (each with their child
/// hierarchy). Events emitted with no current span are discarded; use
/// [`capture_full`] to retrieve them.
///
/// See module docs for thread-safety limitations.
pub fn capture_traces<F, R>(f: F) -> (R, Vec<RecordedSpan>)
where
    F: FnOnce() -> R,
{
    let (result, capture) = capture_full(f);
    (result, capture.roots)
}

/// Like [`capture_traces`] but also returns events that fired with no
/// enclosing span.
pub fn capture_full<F, R>(f: F) -> (R, Capture)
where
    F: FnOnce() -> R,
{
    let state = Arc::new(Mutex::new(CaptureState::default()));
    let layer = CaptureLayer {
        state: state.clone(),
    };

    let subscriber = tracing_subscriber::registry().with(layer);
    let guard = tracing::subscriber::set_default(subscriber);

    let result = f();

    drop(guard);

    // Drain the state. We hold an Arc clone, so try_unwrap may fail; instead
    // take the contents out via the mutex.
    let mut locked = state.lock().expect("capture state mutex poisoned");
    let nodes = std::mem::take(&mut locked.nodes);
    let root_order = std::mem::take(&mut locked.root_order);
    let orphan_events = std::mem::take(&mut locked.orphan_events);
    drop(locked);

    let mut nodes = nodes;
    let roots = root_order
        .into_iter()
        .filter_map(|id| build_recorded(id, &mut nodes))
        .collect();

    (result, Capture { roots, orphan_events })
}

fn build_recorded(id: u64, nodes: &mut HashMap<u64, SpanNode>) -> Option<RecordedSpan> {
    let node = nodes.remove(&id)?;
    let children = node
        .children
        .into_iter()
        .filter_map(|cid| build_recorded(cid, nodes))
        .collect();
    Some(RecordedSpan {
        name: node.name,
        level: node.level,
        target: node.target,
        fields: node.fields,
        children,
        events: node.events,
    })
}

// ── Search helpers ──────────────────────────────────────────────────────

/// Find the first span matching `name`, searching the full hierarchy
/// (depth-first, pre-order).
pub fn find_span<'a>(spans: &'a [RecordedSpan], name: &str) -> Option<&'a RecordedSpan> {
    for s in spans {
        if s.name == name {
            return Some(s);
        }
        if let Some(found) = find_span(&s.children, name) {
            return Some(found);
        }
    }
    None
}

/// Find a direct child span by name (does not recurse).
pub fn find_child<'a>(parent: &'a RecordedSpan, name: &str) -> Option<&'a RecordedSpan> {
    parent.children.iter().find(|s| s.name == name)
}

/// Find all spans matching `name` in the hierarchy (depth-first).
pub fn find_all_spans<'a>(spans: &'a [RecordedSpan], name: &str) -> Vec<&'a RecordedSpan> {
    let mut out = Vec::new();
    collect_matching(spans, name, &mut out);
    out
}

fn collect_matching<'a>(spans: &'a [RecordedSpan], name: &str, out: &mut Vec<&'a RecordedSpan>) {
    for s in spans {
        if s.name == name {
            out.push(s);
        }
        collect_matching(&s.children, name, out);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn captures_basic_span_with_fields() {
        let (_, spans) = capture_traces(|| {
            let _g = tracing::info_span!("test:span", foo = "bar").entered();
            tracing::info!(baz = 42, "hello");
        });

        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].name, "test:span");
        assert_eq!(spans[0].field("foo"), Some("bar"));
        assert_eq!(spans[0].events.len(), 1);
        assert_eq!(spans[0].events[0].message.as_deref(), Some("hello"));
    }

    #[test]
    fn captures_late_recorded_fields() {
        // This is the key correctness fix: Span::record() after span creation
        // must be captured.
        let (_, spans) = capture_traces(|| {
            let span = tracing::info_span!("late", x = tracing::field::Empty);
            let _g = span.enter();
            span.record("x", "filled-in");
        });

        let span = find_span(&spans, "late").expect("late span exists");
        assert_eq!(
            span.field("x"),
            Some("filled-in"),
            "late record() must be captured; got fields: {:?}",
            span.fields
        );
    }

    #[test]
    fn events_attach_to_current_span_not_last_created() {
        // Critical correctness test: an event inside `outer` must attach to
        // `outer`, not to the more recently *created* (but already exited)
        // `unrelated` sibling.
        let (_, spans) = capture_traces(|| {
            let outer = tracing::info_span!("outer");
            {
                let _u = tracing::info_span!("unrelated").entered();
            } // unrelated exits and closes here
            let _g = outer.enter();
            tracing::info!("inside outer");
        });

        let outer = find_span(&spans, "outer").expect("outer exists");
        assert_eq!(outer.events.len(), 1, "event should attach to outer");
        assert_eq!(outer.events[0].message.as_deref(), Some("inside outer"));

        let unrelated = find_span(&spans, "unrelated").expect("unrelated exists");
        assert_eq!(unrelated.events.len(), 0, "unrelated must have no events");
    }

    #[test]
    fn captures_parent_child_hierarchy() {
        let (_, spans) = capture_traces(|| {
            let _p = tracing::info_span!("parent").entered();
            let _c = tracing::info_span!("child").entered();
            tracing::info!("inside child");
        });

        assert_eq!(spans.len(), 1, "should be one root: {:?}", spans);
        let parent = &spans[0];
        assert_eq!(parent.name, "parent");
        assert_eq!(parent.children.len(), 1);
        let child = &parent.children[0];
        assert_eq!(child.name, "child");
        assert_eq!(child.events.len(), 1);
    }

    #[test]
    fn find_span_recurses_into_children() {
        let (_, spans) = capture_traces(|| {
            let _p = tracing::info_span!("p").entered();
            let _c = tracing::info_span!("c").entered();
            let _gc = tracing::info_span!("gc").entered();
        });

        assert!(find_span(&spans, "p").is_some());
        assert!(find_span(&spans, "c").is_some());
        assert!(find_span(&spans, "gc").is_some());
    }

    #[test]
    fn find_child_does_not_recurse() {
        let (_, spans) = capture_traces(|| {
            let _p = tracing::info_span!("p").entered();
            let _c = tracing::info_span!("c").entered();
            let _gc = tracing::info_span!("gc").entered();
        });

        let p = find_span(&spans, "p").unwrap();
        assert!(find_child(p, "c").is_some());
        assert!(
            find_child(p, "gc").is_none(),
            "find_child must not recurse"
        );
    }

    #[test]
    fn returns_closure_result() {
        let (result, _) = capture_traces(|| 42);
        assert_eq!(result, 42);
    }

    #[test]
    fn orphan_events_captured_in_full_capture() {
        let (_, capture) = capture_full(|| {
            tracing::info!("no span here");
        });
        assert_eq!(capture.roots.len(), 0);
        assert_eq!(capture.orphan_events.len(), 1);
        assert_eq!(
            capture.orphan_events[0].message.as_deref(),
            Some("no span here")
        );
    }

    #[test]
    fn find_all_spans_returns_every_match() {
        let (_, spans) = capture_traces(|| {
            let _a = tracing::info_span!("rep").entered();
            let _b = tracing::info_span!("rep").entered();
        });
        let all = find_all_spans(&spans, "rep");
        assert_eq!(all.len(), 2);
    }
}

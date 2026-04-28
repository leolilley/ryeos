pub mod dispatch_launch;
pub mod thread_events;

use std::sync::Arc;

use serde_json::Value;

use crate::dispatch_error::RouteConfigError;
use crate::routes::compile::RouteDispatchContext;
use crate::routes::raw::RawRouteSpec;
use crate::state::AppState;

pub trait StreamingSource: Send + Sync {
    fn key(&self) -> &'static str;

    fn compile(
        &self,
        raw_route: &RawRouteSpec,
        raw_event_stream: &RawEventStreamResponse,
        ctx: &SourceCompileContext,
    ) -> Result<Arc<dyn BoundStreamingSource>, RouteConfigError>;
}

pub struct SourceCompileContext<'a> {
    pub auth_verifier_key: &'a str,
}

#[derive(Debug, Clone, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawEventStreamResponse {
    pub source: String,
    #[serde(default)]
    pub source_config: Value,
}

#[axum::async_trait]
pub trait BoundStreamingSource: Send + Sync {
    async fn open(
        &self,
        ctx: &RouteDispatchContext,
        last_event_id: Option<i64>,
        state: &AppState,
    ) -> Result<SseEventStream, crate::dispatch_error::RouteDispatchError>;
}

pub struct SseEventStream {
    pub stream: std::pin::Pin<
        Box<
            dyn tokio_stream::Stream<
                    Item = Result<axum::response::sse::Event, std::convert::Infallible>,
                > + Send,
        >,
    >,
    pub keep_alive_secs: u64,
}

pub struct StreamingSourceRegistry {
    sources: Vec<Arc<dyn StreamingSource>>,
}

impl StreamingSourceRegistry {
    pub fn new() -> Self {
        Self {
            sources: Vec::new(),
        }
    }

    pub fn register(&mut self, source: Arc<dyn StreamingSource>) {
        let key = source.key();
        if self.sources.iter().any(|s| s.key() == key) {
            panic!("StreamingSourceRegistry: duplicate source `{key}`");
        }
        self.sources.push(source);
    }

    pub fn get(&self, key: &str) -> Option<&dyn StreamingSource> {
        self.sources.iter().find(|s| s.key() == key).map(|s| s.as_ref())
    }

    pub fn with_builtins() -> Self {
        let mut r = Self::new();
        r.register(Arc::new(dispatch_launch::DispatchLaunchSource));
        r.register(Arc::new(thread_events::ThreadEventsSource));
        r
    }
}

impl Default for StreamingSourceRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtins_register_thread_events() {
        let r = StreamingSourceRegistry::with_builtins();
        assert!(r.get("thread_events").is_some());
        assert!(r.get("dispatch_launch").is_some());
        assert!(r.get("nonexistent").is_none());
    }

    #[test]
    #[should_panic(expected = "duplicate source")]
    fn duplicate_registration_panics() {
        let mut r = StreamingSourceRegistry::new();
        r.register(Arc::new(thread_events::ThreadEventsSource));
        r.register(Arc::new(thread_events::ThreadEventsSource));
    }
}

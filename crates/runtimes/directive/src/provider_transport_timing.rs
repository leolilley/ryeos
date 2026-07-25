//! Transport instrumentation for the directive runtime's reqwest client.
//!
//! Reqwest exposes its DNS resolver and its complete connector as extension
//! points. The resolver gives us an exact DNS interval. The connector layer
//! completes only after reqwest's connection establishment has finished, so
//! that interval is deliberately reported as one aggregate which can include
//! DNS, TCP, proxy negotiation, and TLS. Splitting that aggregate into
//! separately named TCP and TLS intervals would require replacing or patching
//! reqwest's connector implementation.

use std::error::Error;
use std::future::Future;
use std::io;
use std::net::ToSocketAddrs;
use std::pin::Pin;
use std::task::{Context, Poll};

use tower::{Layer, Service};

type BoxError = Box<dyn Error + Send + Sync>;

/// The same `getaddrinfo`-backed resolver used by reqwest's default connector,
/// wrapped only to record the resolver future's elapsed time.
#[derive(Clone, Copy, Debug, Default)]
pub struct InstrumentedDnsResolver;

struct AbortOnDrop<T>(tokio::task::JoinHandle<T>);

impl<T> Drop for AbortOnDrop<T> {
    fn drop(&mut self) {
        self.0.abort();
    }
}

impl reqwest::dns::Resolve for InstrumentedDnsResolver {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        let timing = crate::startup_timing::begin_dns_lookup();
        let host = name.as_str().to_owned();
        let mut lookup = AbortOnDrop(tokio::task::spawn_blocking(move || {
            (host.as_str(), 0)
                .to_socket_addrs()
                .map(|addresses| addresses.collect::<Vec<_>>())
        }));

        Box::pin(async move {
            let result: Result<reqwest::dns::Addrs, BoxError> = match (&mut lookup.0).await {
                Ok(Ok(addresses)) => Ok(Box::new(addresses.into_iter()) as reqwest::dns::Addrs),
                Ok(Err(error)) => Err(Box::new(error) as BoxError),
                Err(error) if error.is_cancelled() => {
                    Err(Box::new(io::Error::new(io::ErrorKind::Interrupted, error)) as BoxError)
                }
                Err(error) => Err(Box::new(io::Error::other(error)) as BoxError),
            };
            crate::startup_timing::finish_dns_lookup(timing, result.is_ok());
            result
        })
    }
}

/// Times reqwest's whole connection-establishment future without changing its
/// behavior. This layer is not invoked when reqwest can serve a request from
/// its connection pool; an absent observation is therefore useful evidence
/// for a later-call reuse analysis, but is not labelled as proof of reuse.
#[derive(Clone, Copy, Debug, Default)]
pub struct ConnectionEstablishmentTimingLayer;

impl<S> Layer<S> for ConnectionEstablishmentTimingLayer {
    type Service = ConnectionEstablishmentTimingService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        ConnectionEstablishmentTimingService { inner }
    }
}

#[derive(Clone, Debug)]
pub struct ConnectionEstablishmentTimingService<S> {
    inner: S,
}

impl<S, Request> Service<Request> for ConnectionEstablishmentTimingService<S>
where
    S: Service<Request>,
    S::Future: Send + 'static,
    S::Response: 'static,
    S::Error: 'static,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future =
        Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send + 'static>>;

    fn poll_ready(&mut self, context: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(context)
    }

    fn call(&mut self, request: Request) -> Self::Future {
        let timing = crate::startup_timing::begin_connection_establishment();
        let future = self.inner.call(request);

        Box::pin(async move {
            let result = future.await;
            crate::startup_timing::finish_connection_establishment(timing, result.is_ok());
            result
        })
    }
}

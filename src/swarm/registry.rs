//! ServiceRegistry: owns pluggable discovery backends and builds locators.

use std::sync::Arc;
use std::time::Duration;

use futures_util::Stream;
use iroh::Endpoint;
use tonic::server::NamedService;
use tonic::transport::Channel;

use super::discovery::{Discovery, Peer};
use super::engine::SwarmEngine;
use super::locator::{Locator, LocatorConfig};
use super::peers::PeerFeedSpec;
use crate::server::service_to_alpn;

/// Unified registry for pluggable peer discovery backends.
#[derive(Clone)]
pub struct ServiceRegistry {
    endpoint: Endpoint,
    backends: Vec<Arc<dyn Discovery>>,
}

impl ServiceRegistry {
    /// Create an empty registry with no backends.
    pub fn new(endpoint: &Endpoint) -> Self {
        Self {
            endpoint: endpoint.clone(),
            backends: Vec::new(),
        }
    }

    /// Add a discovery backend. Returns `&mut Self` for chaining.
    pub fn add<D: Discovery>(&mut self, backend: D) -> &mut Self {
        self.backends.push(Arc::new(backend));
        self
    }

    /// Add a pre-wrapped `Arc<dyn Discovery>` backend.
    pub fn add_shared(&mut self, backend: Arc<dyn Discovery>) -> &mut Self {
        self.backends.push(backend);
        self
    }

    /// Access the endpoint.
    pub fn endpoint(&self) -> &Endpoint {
        &self.endpoint
    }

    /// Discover peers for a service.
    pub fn discover<S: NamedService>(&self) -> impl Stream<Item = crate::Result<Peer>> {
        let alpn = service_to_alpn::<S>();
        let feeds = self.build_feeds(&alpn);
        SwarmEngine::new(self.endpoint.id(), &alpn, feeds)
    }

    /// Build a locator builder for racing connections.
    pub fn find<S: NamedService + Send + 'static>(&self) -> LocatorBuilder<'_, S> {
        LocatorBuilder::new(self)
    }

    fn build_feeds(&self, alpn: &[u8]) -> Vec<PeerFeedSpec> {
        self.backends
            .iter()
            .flat_map(|b| b.feeds(alpn))
            .collect()
    }
}

/// Fluent builder to configure a locator.
pub struct LocatorBuilder<'a, S>
where
    S: NamedService + Send + 'static,
{
    registry: &'a ServiceRegistry,
    locator_cfg: LocatorConfig,
    _marker: std::marker::PhantomData<S>,
}

impl<'a, S> LocatorBuilder<'a, S>
where
    S: NamedService + Send + 'static,
{
    fn new(registry: &'a ServiceRegistry) -> Self {
        Self {
            registry,
            locator_cfg: LocatorConfig::default(),
            _marker: std::marker::PhantomData,
        }
    }

    /// Maximum simultaneous connection attempts.
    pub fn max_inflight(mut self, n: usize) -> Self {
        self.locator_cfg.max_inflight = n;
        self
    }

    /// Timeout for each individual attempt.
    pub fn timeout_each(mut self, d: Duration) -> Self {
        self.locator_cfg.timeout_each = d;
        self
    }

    /// Overall timeout for the locator.
    pub fn timeout(mut self, d: Duration) -> Self {
        self.locator_cfg.timeout = Some(d);
        self
    }

    /// Buffer size for successful connections.
    pub fn limit(mut self, n: usize) -> Self {
        self.locator_cfg.limit = n;
        self
    }

    /// Start the locator and return its handle.
    pub fn start(self) -> Locator {
        let alpn = service_to_alpn::<S>();
        let feeds = self.registry.build_feeds(&alpn);
        let stream = SwarmEngine::new(self.registry.endpoint.id(), &alpn, feeds);
        Locator::spawn::<S, _>(self.registry.endpoint.clone(), stream, self.locator_cfg)
    }

    /// Convenience: return the first successful channel.
    pub async fn first(self) -> crate::Result<Channel> {
        self.start().first().await
    }
}

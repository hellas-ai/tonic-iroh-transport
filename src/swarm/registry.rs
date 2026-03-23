//! `ServiceRegistry`: owns pluggable discovery backends and builds locators.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::Stream;
use iroh::Endpoint;
use tonic::server::NamedService;
use tonic::transport::Channel;

use super::discovery::{Discovery, Peer};
use super::engine::SwarmEngine;
use super::locator::{Locator, LocatorConfig};
use super::peers::PeerFeedSpec;
use crate::alpn::service_to_alpn;
use crate::{ConnectionPool, PoolOptions};

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct PoolKey {
    alpn: Vec<u8>,
    options: PoolOptions,
}

/// Unified registry for pluggable peer discovery backends.
#[derive(Clone)]
pub struct ServiceRegistry {
    endpoint: Endpoint,
    backends: Vec<Arc<dyn Discovery>>,
    pool_options: PoolOptions,
    pools: Arc<Mutex<HashMap<PoolKey, ConnectionPool>>>,
}

impl ServiceRegistry {
    /// Create an empty registry with no backends.
    #[must_use]
    pub fn new(endpoint: &Endpoint) -> Self {
        Self {
            endpoint: endpoint.clone(),
            backends: Vec::new(),
            pool_options: PoolOptions::default(),
            pools: Arc::new(Mutex::new(HashMap::new())),
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

    /// Set default pool options used for service-scoped connection reuse.
    ///
    /// Existing cached pools are retained; the new options apply to pools that
    /// are created after this call.
    pub fn with_pool_options(&mut self, options: PoolOptions) -> &mut Self {
        self.pool_options = options;
        self
    }

    /// Access the endpoint.
    #[must_use]
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
    #[must_use]
    pub fn find<S: NamedService + Send + 'static>(&self) -> LocatorBuilder<'_, S> {
        LocatorBuilder::new(self)
    }

    /// Get a shared pool for a service ALPN using the registry defaults.
    #[must_use]
    pub fn pool<S: NamedService>(&self) -> ConnectionPool {
        let alpn = service_to_alpn::<S>();
        self.pool_for_alpn(&alpn, self.pool_options.clone())
    }

    fn build_feeds(&self, alpn: &[u8]) -> Vec<PeerFeedSpec> {
        self.backends.iter().flat_map(|b| b.feeds(alpn)).collect()
    }

    fn pool_for_alpn(&self, alpn: &[u8], options: PoolOptions) -> ConnectionPool {
        let key = PoolKey {
            alpn: alpn.to_vec(),
            options: options.clone(),
        };
        let mut pools = self
            .pools
            .lock()
            .expect("service registry pool cache should not be poisoned");
        pools
            .entry(key)
            .or_insert_with(|| ConnectionPool::new(self.endpoint.clone(), alpn, options))
            .clone()
    }
}

/// Fluent builder to configure a locator.
pub struct LocatorBuilder<'a, S>
where
    S: NamedService + Send + 'static,
{
    registry: &'a ServiceRegistry,
    locator_cfg: LocatorConfig,
    pool_options: PoolOptions,
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
            pool_options: registry.pool_options.clone(),
            _marker: std::marker::PhantomData,
        }
    }

    /// Maximum simultaneous connection attempts.
    #[must_use]
    pub fn max_inflight(mut self, n: usize) -> Self {
        self.locator_cfg.max_inflight = n;
        self
    }

    /// Timeout for each individual attempt.
    #[must_use]
    pub fn timeout_each(mut self, d: Duration) -> Self {
        self.locator_cfg.timeout_each = d;
        self.pool_options.connect_timeout = d;
        self
    }

    /// Overall timeout for the locator.
    #[must_use]
    pub fn timeout(mut self, d: Duration) -> Self {
        self.locator_cfg.timeout = Some(d);
        self
    }

    /// Buffer size for successful connections.
    #[must_use]
    pub fn limit(mut self, n: usize) -> Self {
        self.locator_cfg.limit = n;
        self
    }

    /// Override the pool options used for this service locator.
    #[must_use]
    pub fn pool_options(mut self, options: PoolOptions) -> Self {
        self.locator_cfg.timeout_each = options.connect_timeout;
        self.pool_options = options;
        self
    }

    /// Start the locator and return its handle.
    #[must_use]
    pub fn start(self) -> Locator {
        let alpn = service_to_alpn::<S>();
        let feeds = self.registry.build_feeds(&alpn);
        let stream = SwarmEngine::new(self.registry.endpoint.id(), &alpn, feeds);
        let pool = self.registry.pool_for_alpn(&alpn, self.pool_options);
        Locator::spawn_with_pool::<S, _>(pool, stream, self.locator_cfg)
    }

    /// Convenience: return the first successful channel.
    ///
    /// # Errors
    ///
    /// Returns an error if no peer could be reached.
    pub async fn first(self) -> crate::Result<Channel> {
        self.start().first().await
    }
}

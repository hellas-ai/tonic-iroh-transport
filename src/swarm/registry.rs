//! Service registry for unified mDNS + DHT discovery.
//!
//! Owns a single shared mainline DHT instance used for both server-side
//! publishing and client-side resolution.

use std::marker::PhantomData;
use std::sync::Arc;
use std::time::Duration;

use futures_util::Stream;
use iroh::discovery::mdns::MdnsDiscovery;
use iroh::{Endpoint, EndpointId};
use mainline::Dht;
use tonic::server::NamedService;
use tonic::transport::Channel;

use super::connect::{self, ConnectOptions, Locator};
use super::dht::publisher::{DhtPublisher, DhtPublisherConfig};
use super::stream::DiscoveryOptions;
use crate::server::service_to_alpn;

/// Unified service registry for mDNS + DHT discovery.
///
/// Owns a single shared mainline DHT instance. On the server side, pass to
/// [`TransportBuilder::with_registry()`](crate::TransportBuilder::with_registry)
/// to enable DHT publishing. On the client side, call
/// [`discover()`](ServiceRegistry::discover) to get a stream of peers.
///
/// # Server
///
/// ```ignore
/// let registry = ServiceRegistry::new(&endpoint)?;
/// let guard = TransportBuilder::new(endpoint)
///     .with_registry(&registry)
///     .add_rpc(MyServiceServer::new(svc))
///     .spawn()
///     .await?;
/// ```
///
/// # Client
///
/// ```ignore
/// let registry = ServiceRegistry::new(&endpoint)?;
/// let mut stream = Box::pin(registry.discover::<MyServiceServer<()>>());
/// let peer = stream.next().await;
/// ```
#[derive(Clone)]
pub struct ServiceRegistry {
    endpoint: Endpoint,
    dht: Arc<Dht>,
    mdns: Option<Arc<MdnsDiscovery>>,
}

impl ServiceRegistry {
    /// Create a new registry with a shared DHT client.
    pub fn new(endpoint: &Endpoint) -> std::io::Result<Self> {
        let dht =
            Arc::new(Dht::client().map_err(|e| std::io::Error::other(format!("DHT client: {e}")))?);
        Ok(Self {
            dht,
            endpoint: endpoint.clone(),
            mdns: None,
        })
    }

    /// Create a registry with mDNS enabled.
    pub fn with_mdns(endpoint: &Endpoint, mdns: MdnsDiscovery) -> std::io::Result<Self> {
        let mut reg = Self::new(endpoint)?;
        reg.mdns = Some(Arc::new(mdns));
        Ok(reg)
    }

    /// Enable mDNS on an existing registry.
    pub fn set_mdns(&mut self, mdns: MdnsDiscovery) {
        self.mdns = Some(Arc::new(mdns));
    }

    /// Discover peers running a specific tonic service.
    ///
    /// Returns a stream of `EndpointId`s discovered via mDNS (local network)
    /// and DHT (internet), deduplicated.
    pub fn discover<S: NamedService>(&self) -> impl Stream<Item = crate::Result<EndpointId>> {
        self.discover_with_options::<S>(DiscoveryOptions::default())
    }

    /// Discover peers with custom options.
    pub fn discover_with_options<S: NamedService>(
        &self,
        options: DiscoveryOptions,
    ) -> impl Stream<Item = crate::Result<EndpointId>> {
        super::stream::discover_service_alpn(
            &self.endpoint,
            self.mdns.clone(),
            Arc::clone(&self.dht),
            service_to_alpn::<S>(),
            options,
        )
    }

    /// Start building a connection race for a service.
    pub fn find<S: NamedService + Send + 'static>(&self) -> LocatorBuilder<'_, S> {
        LocatorBuilder::new(self)
    }

    /// Discover peers by raw ALPN bytes.
    pub fn discover_alpn(
        &self,
        alpn: Vec<u8>,
        options: DiscoveryOptions,
    ) -> impl Stream<Item = crate::Result<EndpointId>> {
        super::stream::discover_service_alpn(
            &self.endpoint,
            self.mdns.clone(),
            Arc::clone(&self.dht),
            alpn,
            options,
        )
    }

    /// Create a DHT publisher for server-side use.
    pub(crate) fn create_publisher(&self, config: DhtPublisherConfig) -> DhtPublisher {
        DhtPublisher::new(
            Arc::clone(&self.dht),
            *self.endpoint.id().as_bytes(),
            config,
        )
    }
}

/// Fluent builder for racing connections to a service.
pub struct LocatorBuilder<'a, S>
where
    S: NamedService + Send + 'static,
{
    registry: &'a ServiceRegistry,
    opts: ConnectOptions,
    _marker: PhantomData<S>,
}

impl<'a, S> LocatorBuilder<'a, S>
where
    S: NamedService + Send + 'static,
{
    fn new(registry: &'a ServiceRegistry) -> Self {
        Self {
            registry,
            opts: ConnectOptions::default(),
            _marker: PhantomData,
        }
    }

    pub fn max_inflight(mut self, n: usize) -> Self {
        self.opts.max_inflight = n;
        self
    }

    pub fn per_attempt_timeout(mut self, d: Duration) -> Self {
        self.opts.per_attempt_timeout = d;
        self
    }

    pub fn overall_timeout(mut self, d: Duration) -> Self {
        self.opts.overall_timeout = Some(d);
        self
    }

    pub fn success_buffer(mut self, n: usize) -> Self {
        self.opts.success_buffer = n;
        self
    }

    /// Start the background racer and return the handle.
    pub fn start(self) -> Locator {
        let peers = self.registry.discover::<S>();
        connect::connect::<S, _>(self.registry.endpoint.clone(), peers, self.opts)
    }

    /// Convenience: get the first successful channel.
    pub async fn first(self) -> crate::Result<Channel> {
        self.start().first().await
    }
}

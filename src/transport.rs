//! Unified transport builder for composing RPC services over a single iroh router.

use std::convert::Infallible;

use iroh::discovery::UserData;
use iroh::protocol::{Router, RouterBuilder};
use iroh::Endpoint;
use tokio::sync::broadcast;
use tokio::task::JoinSet;
use tonic::server::NamedService;
use tonic::transport::server::Router as TonicRouter;
use tonic::transport::Server;

use crate::error::Result;
use crate::server::{service_to_alpn, GrpcProtocolHandler, IrohIncoming};

#[cfg(feature = "discovery")]
use crate::swarm::dht::publisher::DhtPublisher;
#[cfg(feature = "discovery")]
use crate::swarm::{DhtPublisherConfig, ServiceRegistry};

const ALPN_SEPARATOR: char = ',';

fn encode_alpns(alpns: &[Vec<u8>]) -> UserData {
    let strings: Vec<String> = alpns
        .iter()
        .filter_map(|a| String::from_utf8(a.clone()).ok())
        .collect();
    strings.join(&ALPN_SEPARATOR.to_string()).parse().unwrap()
}

fn decode_alpns(user_data: &UserData) -> Vec<Vec<u8>> {
    let s: &str = user_data.as_ref();
    s.split(ALPN_SEPARATOR)
        .map(|part| part.as_bytes().to_vec())
        .collect()
}

/// Check if user_data contains the ALPN for a specific tonic service.
pub fn user_data_has_service<S: NamedService>(user_data: &UserData) -> bool {
    let target = service_to_alpn::<S>();
    decode_alpns(user_data).iter().any(|alpn| alpn == &target)
}

/// Check if user_data contains a specific ALPN.
pub fn user_data_has_alpn(user_data: &UserData, alpn: &[u8]) -> bool {
    decode_alpns(user_data).iter().any(|a| a == alpn)
}

/// Get all ALPNs from user_data.
pub fn user_data_alpns(user_data: &UserData) -> Vec<Vec<u8>> {
    decode_alpns(user_data)
}

/// Trait object to register heterogeneous tonic services.
trait AddService: Send {
    fn register(
        self: Box<Self>,
        router: RouterBuilder,
        sender: tokio::sync::mpsc::UnboundedSender<
            std::result::Result<crate::stream::IrohStream, std::io::Error>,
        >,
        tonic_router: Option<TonicRouter>,
    ) -> (RouterBuilder, TonicRouter);
}

impl<S> AddService for S
where
    S: NamedService
        + Clone
        + Send
        + Sync
        + 'static
        + tower::Service<http::Request<tonic::body::Body>, Error = Infallible>,
    S::Future: Send + 'static,
    <S as tower::Service<http::Request<tonic::body::Body>>>::Response: axum::response::IntoResponse,
{
    fn register(
        self: Box<Self>,
        router: RouterBuilder,
        sender: tokio::sync::mpsc::UnboundedSender<
            std::result::Result<crate::stream::IrohStream, std::io::Error>,
        >,
        tonic_router: Option<TonicRouter>,
    ) -> (RouterBuilder, TonicRouter) {
        let service = *self;
        let alpn = service_to_alpn::<S>();
        let handler = GrpcProtocolHandler::with_sender(S::NAME, sender);
        let router = router.accept(alpn, handler);

        let tonic_router = match tonic_router {
            Some(r) => r.add_service(service),
            None => Server::builder().add_service(service),
        };

        (router, tonic_router)
    }
}

/// Builder for composing RPC services over a single iroh router.
pub struct TransportBuilder {
    endpoint: Endpoint,
    services: Vec<Box<dyn AddService>>,
    alpns: Vec<Vec<u8>>,
    #[cfg(feature = "discovery")]
    registry: Option<(ServiceRegistry, DhtPublisherConfig)>,
}

impl TransportBuilder {
    /// Create a new builder for an endpoint.
    pub fn new(endpoint: Endpoint) -> Self {
        Self {
            endpoint,
            services: Vec::new(),
            alpns: Vec::new(),
            #[cfg(feature = "discovery")]
            registry: None,
        }
    }

    /// Enable DHT publishing using a shared service registry.
    #[cfg(feature = "discovery")]
    pub fn with_registry(mut self, registry: &ServiceRegistry) -> Self {
        self.registry = Some((registry.clone(), DhtPublisherConfig::default()));
        self
    }

    /// Enable DHT publishing with custom configuration.
    #[cfg(feature = "discovery")]
    pub fn with_registry_config(mut self, registry: &ServiceRegistry, config: DhtPublisherConfig) -> Self {
        self.registry = Some((registry.clone(), config));
        self
    }

    /// Add a tonic RPC service.
    pub fn add_rpc<S>(mut self, service: S) -> Self
    where
        S: NamedService
            + Clone
            + Send
            + Sync
            + 'static
            + tower::Service<http::Request<tonic::body::Body>, Error = Infallible>,
        S::Future: Send + 'static,
        <S as tower::Service<http::Request<tonic::body::Body>>>::Response:
            axum::response::IntoResponse,
    {
        self.alpns.push(service_to_alpn::<S>());
        self.services.push(Box::new(service));
        self
    }

    /// Spawn the transport (router, tonic server).
    pub async fn spawn(self) -> Result<TransportGuard> {
        let (shutdown_tx, _) = broadcast::channel(1);
        let (incoming, sender) = IrohIncoming::new();
        let mut builder = Router::builder(self.endpoint.clone());
        let mut tonic_router: Option<TonicRouter> = None;

        for svc in self.services.into_iter() {
            let (b, tr) = svc.register(builder, sender.clone(), tonic_router);
            builder = b;
            tonic_router = Some(tr);
        }

        if !self.alpns.is_empty() {
            let user_data = encode_alpns(&self.alpns);
            self.endpoint.set_user_data_for_discovery(Some(user_data));
        }

        // Start DHT publisher if a service registry was provided
        #[cfg(feature = "discovery")]
        let dht_publisher = if let Some((registry, config)) = self.registry {
            let mut publisher = registry.create_publisher(config);
            for alpn in &self.alpns {
                publisher.add_service(alpn.clone());
            }
            publisher.start(shutdown_tx.subscribe());
            Some(publisher)
        } else {
            None
        };

        let router = builder.spawn();

        let mut driver = JoinSet::new();

        if let Some(tr) = tonic_router {
            let mut shutdown_rx = shutdown_tx.subscribe();
            let incoming = incoming;
            driver.spawn(async move {
                let shutdown_fut = async move {
                    let _ = shutdown_rx.recv().await;
                };
                if let Err(e) = tr
                    .serve_with_incoming_shutdown(incoming, shutdown_fut)
                    .await
                {
                    tracing::error!("tonic server error: {e}");
                }
            });
        }

        let driver_handle = tokio::spawn(async move {
            while let Some(res) = driver.join_next().await {
                if let Err(e) = res {
                    tracing::warn!("transport task failed: {e}");
                }
            }
        });

        Ok(TransportGuard {
            endpoint: self.endpoint,
            shutdown_tx,
            driver: driver_handle,
            router,
            #[cfg(feature = "discovery")]
            dht_publisher,
        })
    }
}

/// Guard for a running transport (router + optional RPC).
pub struct TransportGuard {
    endpoint: Endpoint,
    shutdown_tx: broadcast::Sender<()>,
    driver: tokio::task::JoinHandle<()>,
    router: Router,
    #[cfg(feature = "discovery")]
    dht_publisher: Option<DhtPublisher>,
}

impl TransportGuard {
    /// Access the endpoint.
    pub fn endpoint(&self) -> &Endpoint {
        &self.endpoint
    }

    /// Access the router handle.
    pub fn router(&self) -> Option<&Router> {
        Some(&self.router)
    }

    /// Graceful shutdown of tonic and router.
    #[allow(unused_mut)]
    pub async fn shutdown(mut self) -> Result<()> {
        let _ = self.shutdown_tx.send(());
        let _ = self.driver.await;

        // Stop DHT publisher if running
        #[cfg(feature = "discovery")]
        if let Some(ref mut publisher) = self.dht_publisher {
            publisher.stop().await;
        }

        if let Err(e) = self.router.shutdown().await {
            tracing::warn!("router shutdown error: {e}");
        }

        Ok(())
    }
}

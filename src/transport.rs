//! Unified transport builder for composing RPC services over a single iroh router.

use std::convert::Infallible;

use iroh::protocol::{Router, RouterBuilder};
use iroh::Endpoint;
use tokio::sync::broadcast;
use tokio::task::JoinSet;
use tonic::server::NamedService;
use tonic::transport::server::Router as TonicRouter;
use tonic::transport::Server;

use crate::alpn::service_to_alpn;
use crate::error::Result;
use crate::server::{GrpcProtocolHandler, IrohIncoming, DEFAULT_INCOMING_BUFFER_CAPACITY};
use crate::user_data::encode_alpns;

#[cfg(feature = "discovery")]
use crate::swarm::dht::publisher::DhtPublisher;

/// Trait object to register heterogeneous tonic services.
trait AddService: Send {
    fn register(
        self: Box<Self>,
        router: RouterBuilder,
        sender: tokio::sync::mpsc::Sender<
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
        sender: tokio::sync::mpsc::Sender<
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
    incoming_buffer_capacity: usize,
    #[cfg(feature = "discovery")]
    publisher: Option<DhtPublisher>,
}

impl TransportBuilder {
    /// Create a new builder for an endpoint.
    #[must_use] 
    pub fn new(endpoint: Endpoint) -> Self {
        Self {
            endpoint,
            services: Vec::new(),
            alpns: Vec::new(),
            incoming_buffer_capacity: DEFAULT_INCOMING_BUFFER_CAPACITY,
            #[cfg(feature = "discovery")]
            publisher: None,
        }
    }

    /// Enable DHT publishing with a pre-configured publisher.
    #[cfg(feature = "discovery")]
    #[must_use] 
    pub fn with_publisher(mut self, publisher: DhtPublisher) -> Self {
        self.publisher = Some(publisher);
        self
    }

    /// Add a tonic RPC service.
    #[must_use]
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

    /// Set the maximum number of accepted RPC streams buffered for tonic.
    ///
    /// When the queue is full, newly accepted streams are rejected immediately
    /// instead of being buffered unboundedly in memory.
    #[must_use] 
    pub fn incoming_buffer_capacity(mut self, capacity: usize) -> Self {
        self.incoming_buffer_capacity = capacity.max(1);
        self
    }

    /// Spawn the transport (router, tonic server).
    ///
    /// # Errors
    ///
    /// Returns an error if ALPN metadata encoding fails or the router cannot be built.
    #[allow(clippy::unused_async)]
    pub async fn spawn(self) -> Result<TransportGuard> {
        let (shutdown_tx, _) = broadcast::channel(1);
        let (incoming, sender) = IrohIncoming::new(self.incoming_buffer_capacity);
        let mut builder = Router::builder(self.endpoint.clone());
        let mut tonic_router: Option<TonicRouter> = None;

        for svc in self.services {
            let (b, tr) = svc.register(builder, sender.clone(), tonic_router);
            builder = b;
            tonic_router = Some(tr);
        }

        if !self.alpns.is_empty() {
            let user_data = encode_alpns(&self.alpns)?;
            self.endpoint
                .set_user_data_for_address_lookup(Some(user_data));
        }

        // Start DHT publisher if one was provided
        let router = builder.spawn();

        let mut driver = JoinSet::new();

        #[cfg(feature = "discovery")]
        if let Some(mut publisher) = self.publisher {
            for alpn in &self.alpns {
                publisher.add_service(alpn.clone());
            }

            let shutdown_rx = shutdown_tx.subscribe();
            driver.spawn(async move {
                publisher.run(shutdown_rx).await;
            });
        }

        if let Some(tr) = tonic_router {
            let mut shutdown_rx = shutdown_tx.subscribe();
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
        })
    }
}

/// Guard for a running transport (router + optional RPC).
pub struct TransportGuard {
    endpoint: Endpoint,
    shutdown_tx: broadcast::Sender<()>,
    driver: tokio::task::JoinHandle<()>,
    router: Router,
}

impl TransportGuard {
    /// Access the endpoint.
    #[must_use] 
    pub fn endpoint(&self) -> &Endpoint {
        &self.endpoint
    }

    /// Access the router handle.
    #[must_use] 
    pub fn router(&self) -> &Router {
        &self.router
    }

    /// Graceful shutdown of tonic and router.
    ///
    /// # Errors
    ///
    /// Returns an error if the router shutdown fails.
    pub async fn shutdown(self) -> Result<()> {
        let TransportGuard {
            shutdown_tx,
            driver,
            router,
            ..
        } = self;

        let _ = shutdown_tx.send(());
        let _ = driver.await;

        if let Err(e) = router.shutdown().await {
            tracing::warn!("router shutdown error: {e}");
        }

        Ok(())
    }
}

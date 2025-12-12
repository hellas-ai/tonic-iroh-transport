//! Unified transport builder for composing RPC and gossip over a single iroh router.

use std::convert::Infallible;

use iroh::protocol::{Router, RouterBuilder};
use iroh::Endpoint;
use tokio::sync::broadcast;
use tokio::task::JoinSet;
use tonic::server::NamedService;
use tonic::transport::server::Router as TonicRouter;
use tonic::transport::Server;

use crate::error::Result;
use crate::server::{service_to_alpn, GrpcProtocolHandler, IrohIncoming};

#[cfg(feature = "gossip")]
use crate::gossip::{GossipConfig, HandlerSpawner};

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

/// Builder for composing RPC and gossip over a single router.
pub struct TransportBuilder {
    endpoint: Endpoint,
    services: Vec<Box<dyn AddService>>,
    #[cfg(feature = "gossip")]
    gossip_handlers: Vec<HandlerSpawner>,
    #[cfg(feature = "gossip")]
    gossip_config: GossipConfig,
    #[cfg(feature = "gossip")]
    gossip_enabled: bool,
}

impl TransportBuilder {
    /// Create a new builder for an endpoint.
    pub fn new(endpoint: Endpoint) -> Self {
        Self {
            endpoint,
            services: Vec::new(),
            #[cfg(feature = "gossip")]
            gossip_handlers: Vec::new(),
            #[cfg(feature = "gossip")]
            gossip_config: GossipConfig::default(),
            #[cfg(feature = "gossip")]
            gossip_enabled: false,
        }
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
        self.services.push(Box::new(service));
        self
    }

    /// Set the gossip configuration (bootstrap peers, etc.).
    #[cfg(feature = "gossip")]
    pub fn with_gossip_config(mut self, config: GossipConfig) -> Self {
        self.gossip_config = config;
        self.gossip_enabled = true;
        self
    }

    /// Add a gossip handler using the shared gossip configuration.
    #[cfg(feature = "gossip")]
    pub fn add_gossip<T, H>(mut self, handler: H) -> Self
    where
        T: prost::Message + prost::Name + Send + Default + 'static,
        H: crate::gossip::GossipHandler<T>,
    {
        self.gossip_handlers
            .push(crate::gossip::handler::<T, _>(handler));
        self.gossip_enabled = true;
        self
    }

    /// Spawn the transport (router, tonic server, gossip handlers).
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

        #[cfg(feature = "gossip")]
        let gossip = if self.gossip_enabled {
            let gossip = iroh_gossip::net::Gossip::builder().spawn(self.endpoint.clone());
            builder = builder.accept(iroh_gossip::net::GOSSIP_ALPN, gossip.clone());
            Some(gossip)
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

        #[cfg(feature = "gossip")]
        if let Some(gossip) = gossip.clone() {
            let mut shutdown_rx = shutdown_tx.subscribe();
            let mut tasks = JoinSet::new();
            for spawn in self.gossip_handlers.into_iter() {
                tasks.spawn(spawn(
                    gossip.clone(),
                    self.gossip_config.clone(),
                    shutdown_tx.subscribe(),
                ));
            }

            driver.spawn(async move {
                loop {
                    tokio::select! {
                        _ = shutdown_rx.recv() => {
                            tasks.shutdown().await;
                            break;
                        }
                        Some(res) = tasks.join_next() => {
                            if let Err(e) = res {
                                tracing::warn!("gossip handler task failed: {e}");
                            }
                        }
                        else => break,
                    }
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
            #[cfg(feature = "gossip")]
            gossip,
        })
    }
}

/// Guard for a running transport (router + optional RPC + optional gossip).
pub struct TransportGuard {
    endpoint: Endpoint,
    shutdown_tx: broadcast::Sender<()>,
    driver: tokio::task::JoinHandle<()>,
    router: Router,
    #[cfg(feature = "gossip")]
    gossip: Option<iroh_gossip::net::Gossip>,
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

    /// Access the gossip handle, if configured.
    #[cfg(feature = "gossip")]
    pub fn gossip(&self) -> Option<&iroh_gossip::net::Gossip> {
        self.gossip.as_ref()
    }

    /// Graceful shutdown of gossip, tonic, and router.
    #[allow(unused_mut)]
    pub async fn shutdown(mut self) -> Result<()> {
        let _ = self.shutdown_tx.send(());
        let _ = self.driver.await;

        #[cfg(feature = "gossip")]
        if let Some(gossip) = self.gossip.take() {
            if let Err(e) = gossip.shutdown().await {
                tracing::warn!("gossip shutdown error: {e}");
            }
        }

        if let Err(e) = self.router.shutdown().await {
            tracing::warn!("router shutdown error: {e}");
        }

        Ok(())
    }
}

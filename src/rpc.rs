//! Helper for serving tonic gRPC services over iroh with sane defaults.
//!
//! The default path hides the router and incoming stream lifetimes:
//!
//! ```no_run
//! use tonic_iroh_transport::RpcServer;
//! use http::Request;
//! use tonic::body::Body;
//! use std::convert::Infallible;
//! use std::task::Poll;
//! use axum::response::Response;
//! use tower::Service;
//!
//! #[derive(Clone)]
//! struct Dummy;
//! impl tonic::server::NamedService for Dummy {
//!     const NAME: &'static str = "test.Service";
//! }
//! impl Service<Request<Body>> for Dummy {
//!     type Response = Response;
//!     type Error = Infallible;
//!     type Future = std::future::Ready<Result<Self::Response, Self::Error>>;
//!     fn poll_ready(&mut self, _cx: &mut std::task::Context<'_>) -> Poll<Result<(), Self::Error>> {
//!         Poll::Ready(Ok(()))
//!     }
//!     fn call(&mut self, _req: Request<Body>) -> Self::Future {
//!         std::future::ready(Ok(Response::new(axum::body::Body::empty())))
//!     }
//! }
//!
//! # async fn run(endpoint: iroh::Endpoint) -> Result<(), Box<dyn std::error::Error>> {
//! let guard = RpcServer::new(endpoint)
//!     .add_service(Dummy)
//!     .serve()
//!     .await?;
//! // ... later
//! guard.shutdown().await?;
//! # Ok(())
//! # }
//! ```
//!
//! For advanced use, you can attach handlers to an existing `RouterBuilder`
//! and receive the shared incoming stream plus a tonic router to serve manually.

use std::convert::Infallible;

use iroh::protocol::{Router, RouterBuilder};
use iroh::Endpoint;
use tokio::sync::broadcast;
use tokio::task::JoinHandle;
use tonic::server::NamedService;
use tonic::transport::server::Router as TonicRouter;
use tonic::transport::Server;

use crate::error::Result;
use crate::server::{GrpcProtocolHandler, IrohIncoming};

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
        let alpn = crate::server::service_to_alpn::<S>();
        let handler = GrpcProtocolHandler::with_sender(S::NAME, sender);
        let router = router.accept(alpn, handler);

        let tonic_router = match tonic_router {
            Some(r) => r.add_service(service),
            None => Server::builder().add_service(service),
        };

        (router, tonic_router)
    }
}

/// Builder for serving tonic services over iroh.
///
/// The default [`serve`](RpcServer::serve) path starts the iroh router and a
/// single tonic server hosting all added services over a shared incoming stream.
pub struct RpcServer {
    endpoint: Endpoint,
    services: Vec<Box<dyn AddService>>,
}

impl RpcServer {
    /// Create a new RPC server builder for the endpoint.
    pub fn new(endpoint: Endpoint) -> Self {
        Self {
            endpoint,
            services: Vec::new(),
        }
    }

    /// Add a tonic service to be served over iroh.
    pub fn add_service<S>(mut self, service: S) -> Self
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

    /// Start the router and tonic server, returning a guard for shutdown.
    pub async fn serve(self) -> Result<RpcGuard> {
        let (shutdown_tx, _) = broadcast::channel(1);

        let (incoming, sender) = IrohIncoming::new();
        let mut builder = Router::builder(self.endpoint.clone());
        let mut tonic_router: Option<TonicRouter> = None;

        for svc in self.services.into_iter() {
            let (b, tr) = svc.register(builder, sender.clone(), tonic_router);
            builder = b;
            tonic_router = Some(tr);
        }

        let tonic_router = tonic_router.expect("at least one service is required");
        let router = builder.spawn();

        let mut shutdown_rx = shutdown_tx.subscribe();
        let tonic_handle = tokio::spawn(async move {
            let shutdown_fut = async move {
                let _ = shutdown_rx.recv().await;
            };
            if let Err(e) = tonic_router
                .serve_with_incoming_shutdown(incoming, shutdown_fut)
                .await
            {
                tracing::error!("tonic server error: {e}");
            }
        });

        Ok(RpcGuard {
            router: Some(router),
            endpoint: self.endpoint,
            shutdown_tx,
            tonic_handles: vec![tonic_handle],
        })
    }

    /// Advanced: attach handlers to an existing router builder and return the
    /// incoming stream plus a tonic router to be served manually.
    pub fn attach(self, mut builder: RouterBuilder) -> (RouterBuilder, IrohIncoming, TonicRouter) {
        let (incoming, sender) = IrohIncoming::new();
        let mut tonic_router: Option<TonicRouter> = None;
        for svc in self.services.into_iter() {
            let (b, tr) = svc.register(builder, sender.clone(), tonic_router);
            builder = b;
            tonic_router = Some(tr);
        }
        let tonic_router = tonic_router.expect("at least one service is required");
        (builder, incoming, tonic_router)
    }
}

/// Guard for a running RPC server.
pub struct RpcGuard {
    router: Option<Router>,
    endpoint: Endpoint,
    shutdown_tx: broadcast::Sender<()>,
    tonic_handles: Vec<JoinHandle<()>>,
}

impl RpcGuard {
    /// Endpoint for this server.
    pub fn endpoint(&self) -> &Endpoint {
        &self.endpoint
    }

    /// Router handle if you need direct access.
    pub fn router(&self) -> Option<&Router> {
        self.router.as_ref()
    }

    /// Graceful shutdown.
    pub async fn shutdown(mut self) -> Result<()> {
        let _ = self.shutdown_tx.send(());

        for handle in self.tonic_handles.drain(..) {
            let _ = handle.await;
        }

        if let Some(router) = self.router.take() {
            let _ = router.shutdown().await;
        }

        Ok(())
    }
}

impl Drop for RpcGuard {
    fn drop(&mut self) {
        let _ = self.shutdown_tx.send(());
    }
}

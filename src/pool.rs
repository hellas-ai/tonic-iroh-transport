//! Connection pool for reusing iroh connections.
//!
//! The [`ConnectionPool`] manages connections for a specific ALPN protocol.
//! Connections are reused across requests and cleaned up after an idle timeout.
//!
//! # Example
//!
//! ```rust,no_run
//! use tonic_iroh_transport::pool::{ConnectionPool, PoolOptions};
//! # use tonic_iroh_transport::iroh::{Endpoint, EndpointId, endpoint::presets};
//!
//! # async fn example() -> Result<(), Box<dyn std::error::Error>> {
//! let endpoint = Endpoint::bind(presets::N0).await?;
//! # let peer_id = endpoint.id();
//!
//! // Create a pool for a specific service
//! # struct MyService;
//! # impl tonic::server::NamedService for MyService { const NAME: &'static str = "my.Service"; }
//! let pool = ConnectionPool::for_service::<MyService>(endpoint, PoolOptions::default());
//!
//! // Get a tonic Channel (reuses connections automatically)
//! let channel = pool.channel(peer_id).await?;
//! # Ok(())
//! # }
//! ```

use std::collections::{HashMap, VecDeque};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::task::{Context, Poll};
use std::time::Duration;

use futures_util::stream::FuturesUnordered;
use futures_util::StreamExt;
use http::Uri;
use hyper_util::rt::TokioIo;
use iroh::endpoint::{
    AcceptBi, AcceptUni, Connection, ConnectionError, ConnectionStats, OpenBi, OpenUni,
    ReadDatagram,
};
use iroh::{Endpoint, EndpointId};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::sync::{mpsc, oneshot, Notify};
use tonic::transport::Channel;
use tower::service_fn;
use tracing::{debug, trace};

use crate::stream::write_error_to_io;

const POOL_CHANNEL_CAPACITY: usize = 256;
const CONNECTION_CHANNEL_CAPACITY: usize = 64;
const OPEN_STREAM_RETRIES: usize = 2;

/// Configuration for the connection pool.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PoolOptions {
    /// Duration to keep idle connections alive. Default: 10s.
    pub idle_timeout: Duration,
    /// Timeout for connection establishment. Default: 5s.
    pub connect_timeout: Duration,
    /// Maximum concurrent connections. Default: 1024.
    pub max_connections: usize,
}

impl Default for PoolOptions {
    fn default() -> Self {
        Self {
            idle_timeout: Duration::from_secs(10),
            connect_timeout: Duration::from_secs(5),
            max_connections: 1024,
        }
    }
}

/// Error from pool operations.
#[derive(Debug, Clone, thiserror::Error)]
pub enum PoolError {
    /// The connection pool has been shut down.
    #[error("connection pool is shut down")]
    Shutdown,
    /// Connection timed out.
    #[error("connection timed out")]
    Timeout,
    /// No Tokio runtime is active.
    #[error("no tokio runtime is active")]
    NoRuntime,
    /// Too many active connections.
    #[error("too many connections")]
    TooManyConnections,
    /// The pooled connection actor is saturated.
    #[error("pooled connection is busy")]
    Busy,
    /// The pooled connection closed before it could be reused.
    #[error("connection closed before reuse")]
    Closed,
    /// Iroh connection error.
    #[error("connect error: {0}")]
    Connect(Arc<iroh::endpoint::ConnectError>),
}

impl From<iroh::endpoint::ConnectError> for PoolError {
    fn from(e: iroh::endpoint::ConnectError) -> Self {
        Self::Connect(Arc::new(e))
    }
}

/// A handle to a pooled connection.
///
/// Keep this alive while using the connection. When all handles for a given
/// peer are dropped, the connection enters an idle state and may be closed
/// after [`PoolOptions::idle_timeout`].
#[derive(Debug)]
pub struct ConnectionRef {
    id: EndpointId,
    generation: u64,
    connection: Connection,
    // Keep the per-connection actor's mailbox alive while a lease exists, so
    // dropping the last pool handle does not make `rx.recv()` return `None`
    // before all outstanding leased work has finished.
    _actor: mpsc::Sender<RequestMsg>,
    _permit: OneConnection,
}

impl ConnectionRef {
    fn new(
        id: EndpointId,
        generation: u64,
        connection: Connection,
        actor: mpsc::Sender<RequestMsg>,
        permit: OneConnection,
    ) -> Self {
        Self {
            id,
            generation,
            connection,
            _actor: actor,
            _permit: permit,
        }
    }

    #[must_use]
    pub(crate) fn generation(&self) -> u64 {
        self.generation
    }

    #[must_use]
    pub(crate) fn id(&self) -> EndpointId {
        self.id
    }

    /// Open a bidirectional stream on the leased connection.
    #[must_use]
    pub fn open_bi(&self) -> OpenBi<'_> {
        self.connection.open_bi()
    }

    /// Open a unidirectional stream on the leased connection.
    #[must_use]
    pub fn open_uni(&self) -> OpenUni<'_> {
        self.connection.open_uni()
    }

    /// Read an application datagram from the leased connection.
    #[must_use]
    pub fn read_datagram(&self) -> ReadDatagram<'_> {
        self.connection.read_datagram()
    }

    /// Accept the next incoming bidirectional stream.
    #[must_use]
    pub fn accept_bi(&self) -> AcceptBi<'_> {
        self.connection.accept_bi()
    }

    /// Accept the next incoming unidirectional stream.
    #[must_use]
    pub fn accept_uni(&self) -> AcceptUni<'_> {
        self.connection.accept_uni()
    }

    /// Wait until the leased connection closes.
    pub async fn closed(&self) -> ConnectionError {
        self.connection.closed().await
    }

    /// Return the negotiated ALPN.
    #[must_use]
    pub fn alpn(&self) -> &[u8] {
        self.connection.alpn()
    }

    /// Return the remote endpoint id.
    #[must_use]
    pub fn remote_id(&self) -> EndpointId {
        self.connection.remote_id()
    }

    /// Return the close reason, if the connection is closed.
    #[must_use]
    pub fn close_reason(&self) -> Option<ConnectionError> {
        self.connection.close_reason()
    }

    /// Return a stable identifier for the underlying connection.
    #[must_use]
    pub fn stable_id(&self) -> usize {
        self.connection.stable_id()
    }

    /// Return current connection statistics.
    #[must_use]
    pub fn stats(&self) -> ConnectionStats {
        self.connection.stats()
    }
}

// ---------------------------------------------------------------------------
// Actor internals
// ---------------------------------------------------------------------------

enum Msg {
    Request(RequestMsg),
    Idle { id: EndpointId, generation: u64 },
    Shutdown { id: EndpointId, generation: u64 },
}

struct RequestMsg {
    id: EndpointId,
    tx: oneshot::Sender<Result<ConnectionRef, PoolError>>,
}

#[derive(Debug)]
struct RuntimeHandle {
    tx: mpsc::Sender<Msg>,
}

#[derive(Debug, Default)]
struct Shutdown {
    closed: AtomicBool,
    notify: Notify,
}

impl Shutdown {
    fn is_closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst)
    }

    fn close(&self) {
        if self.closed.swap(true, Ordering::SeqCst) {
            return;
        }
        self.notify.notify_waiters();
    }
}

#[derive(Debug)]
struct Shared {
    endpoint: Endpoint,
    alpn: Vec<u8>,
    options: PoolOptions,
    runtime: OnceLock<RuntimeHandle>,
    runtime_init: Mutex<()>,
    shutdown: Arc<Shutdown>,
}

impl Shared {
    fn new(endpoint: Endpoint, alpn: &[u8], options: PoolOptions) -> Self {
        Self {
            endpoint,
            alpn: alpn.to_vec(),
            options,
            runtime: OnceLock::new(),
            runtime_init: Mutex::new(()),
            shutdown: Arc::new(Shutdown::default()),
        }
    }

    fn tx(&self) -> Result<mpsc::Sender<Msg>, PoolError> {
        if self.shutdown.is_closed() {
            return Err(PoolError::Shutdown);
        }

        if let Some(runtime) = self.runtime.get() {
            return Ok(runtime.tx.clone());
        }

        let _guard = self
            .runtime_init
            .lock()
            .expect("pool runtime initialization should not be poisoned");

        if self.shutdown.is_closed() {
            return Err(PoolError::Shutdown);
        }

        if let Some(runtime) = self.runtime.get() {
            return Ok(runtime.tx.clone());
        }

        let handle = tokio::runtime::Handle::try_current().map_err(|_| PoolError::NoRuntime)?;
        let (actor, tx) = PoolActor::new(self.endpoint.clone(), &self.alpn, self.options.clone());
        handle.spawn(actor.run(tx.clone(), Arc::clone(&self.shutdown)));
        self.runtime
            .set(RuntimeHandle { tx: tx.clone() })
            .expect("pool runtime should only be initialized once");
        Ok(tx)
    }

    fn shutdown(&self) {
        self.shutdown.close();
    }
}

struct Ctx {
    options: PoolOptions,
    endpoint: Endpoint,
    alpn: Vec<u8>,
}

impl Ctx {
    /// Per-connection actor: establishes a connection, hands out refs, manages
    /// idle timeout, and watches for remote close.
    #[allow(clippy::too_many_lines)]
    #[tracing::instrument(
        name = "pool.connection",
        skip(self, pool_tx, shutdown, actor_tx, rx),
        fields(%node_id, generation),
    )]
    async fn run_connection(
        self: Arc<Self>,
        pool_tx: mpsc::Sender<Msg>,
        shutdown: Arc<Shutdown>,
        actor_tx: mpsc::Sender<RequestMsg>,
        node_id: EndpointId,
        generation: u64,
        mut rx: mpsc::Receiver<RequestMsg>,
    ) {
        let connect_result = tokio::time::timeout(self.options.connect_timeout, async {
            self.endpoint
                .connect(node_id, &self.alpn)
                .await
                .map_err(PoolError::from)
        })
        .await
        .map_err(|_| PoolError::Timeout)
        .and_then(|r| r);

        let (connection, error) = match connect_result {
            Ok(connection) => (Some(connection), None),
            Err(error) => (None, Some(error)),
        };

        if error.is_some() {
            debug!(%node_id, generation, "connection failed, notifying pool");
            let _ = pool_tx
                .send(Msg::Shutdown {
                    id: node_id,
                    generation,
                })
                .await;
        }

        let counter = ConnectionCounter::new();
        let idle_sleep = tokio::time::sleep(Duration::from_secs(0));
        tokio::pin!(idle_sleep);
        let mut idle_active = false;
        let mut shutdown_pending = shutdown.is_closed();

        loop {
            let notified = counter.inner.notify.notified();
            let shutdown_wait = shutdown.notify.notified();

            tokio::select! {
                biased;

                () = shutdown_wait, if !shutdown_pending => {
                    shutdown_pending = true;
                    if counter.is_idle() {
                        break;
                    }
                }

                () = async {
                    if let Some(conn) = &connection {
                        conn.closed().await;
                    } else {
                        std::future::pending::<()>().await;
                    }
                }, if connection.is_some() => {
                    debug!(%node_id, generation, "connection closed remotely");
                    let _ = pool_tx
                        .send(Msg::Shutdown {
                            id: node_id,
                            generation,
                        })
                        .await;
                    break;
                }

                req = rx.recv() => {
                    match req {
                        Some(RequestMsg { tx, .. }) => {
                            if let Some(conn) = &connection {
                                if conn.close_reason().is_some() {
                                    let _ = pool_tx
                                        .send(Msg::Shutdown {
                                            id: node_id,
                                            generation,
                                        })
                                        .await;
                                    tx.send(Err(PoolError::Closed)).ok();
                                    break;
                                }

                                let permit = counter.get_one();
                                trace!(
                                    %node_id,
                                    generation,
                                    refs = counter.current(),
                                    "handing out connection ref"
                                );
                                idle_active = false;
                                tx.send(Ok(ConnectionRef::new(
                                    node_id,
                                    generation,
                                    conn.clone(),
                                    actor_tx.clone(),
                                    permit,
                                )))
                                .ok();
                            } else {
                                tx.send(Err(error.clone().unwrap_or(PoolError::Shutdown))).ok();
                            }
                        }
                        None => break,
                    }
                }

                () = notified, if connection.is_some() => {
                    if !counter.is_idle() {
                        continue;
                    }

                    if shutdown_pending {
                        break;
                    }

                    trace!(%node_id, generation, "all refs dropped, starting idle timer");
                    let _ = pool_tx
                        .send(Msg::Idle {
                            id: node_id,
                            generation,
                        })
                        .await;
                    idle_active = true;
                    idle_sleep
                        .as_mut()
                        .reset(tokio::time::Instant::now() + self.options.idle_timeout);
                }

                () = &mut idle_sleep, if idle_active => {
                    trace!(%node_id, generation, "idle timeout expired");
                    let _ = pool_tx
                        .send(Msg::Shutdown {
                            id: node_id,
                            generation,
                        })
                        .await;
                    break;
                }
            }
        }

        if let Some(conn) = connection {
            let reason = if counter.is_idle() { b"idle" } else { b"drop" };
            conn.close(0u32.into(), reason);
        }
        trace!(%node_id, generation, "connection actor stopped");
    }
}

// ---------------------------------------------------------------------------
// Main pool actor
// ---------------------------------------------------------------------------

struct ConnectionSlot {
    generation: u64,
    tx: mpsc::Sender<RequestMsg>,
}

struct PoolActor {
    rx: mpsc::Receiver<Msg>,
    connections: HashMap<EndpointId, ConnectionSlot>,
    ctx: Arc<Ctx>,
    idle: VecDeque<(EndpointId, u64)>,
    next_generation: AtomicU64,
    tasks: FuturesUnordered<Pin<Box<dyn std::future::Future<Output = ()> + Send>>>,
}

impl PoolActor {
    fn new(endpoint: Endpoint, alpn: &[u8], options: PoolOptions) -> (Self, mpsc::Sender<Msg>) {
        let (tx, rx) = mpsc::channel(POOL_CHANNEL_CAPACITY);
        (
            Self {
                rx,
                connections: HashMap::new(),
                idle: VecDeque::new(),
                ctx: Arc::new(Ctx {
                    options,
                    alpn: alpn.to_vec(),
                    endpoint,
                }),
                next_generation: AtomicU64::new(1),
                tasks: FuturesUnordered::new(),
            },
            tx,
        )
    }

    fn add_idle(&mut self, id: EndpointId, generation: u64) {
        self.remove_idle(id);
        self.idle.push_back((id, generation));
    }

    fn remove_idle(&mut self, id: EndpointId) {
        self.idle.retain(|(existing, _)| *existing != id);
    }

    fn matches_generation(&self, id: EndpointId, generation: u64) -> bool {
        self.connections
            .get(&id)
            .is_some_and(|slot| slot.generation == generation)
    }

    fn remove_connection_if_match(&mut self, id: EndpointId, generation: u64) {
        if self.matches_generation(id, generation) {
            self.connections.remove(&id);
            self.remove_idle(id);
        }
    }

    fn next_generation(&self) -> u64 {
        self.next_generation.fetch_add(1, Ordering::SeqCst)
    }

    fn evict_idle_connection(&mut self) -> bool {
        while let Some((id, generation)) = self.idle.pop_front() {
            if self.matches_generation(id, generation) {
                trace!(%id, generation, "evicting idle connection for new one");
                self.connections.remove(&id);
                return true;
            }
        }
        false
    }

    async fn handle(&mut self, pool_tx: &mpsc::Sender<Msg>, shutdown: &Arc<Shutdown>, msg: Msg) {
        match msg {
            Msg::Request(mut req) => {
                let id = req.id;
                self.remove_idle(id);

                if let Some(slot) = self.connections.get(&id) {
                    match slot.tx.try_send(req) {
                        Ok(()) => return,
                        Err(mpsc::error::TrySendError::Closed(returned)) => {
                            req = returned;
                            self.connections.remove(&id);
                        }
                        Err(mpsc::error::TrySendError::Full(returned)) => {
                            req = returned;
                            req.tx.send(Err(PoolError::Busy)).ok();
                            return;
                        }
                    }
                }

                if self.connections.len() >= self.ctx.options.max_connections
                    && !self.evict_idle_connection()
                {
                    req.tx.send(Err(PoolError::TooManyConnections)).ok();
                    return;
                }

                let generation = self.next_generation();
                let (conn_tx, conn_rx) = mpsc::channel(CONNECTION_CHANNEL_CAPACITY);
                self.connections.insert(
                    id,
                    ConnectionSlot {
                        generation,
                        tx: conn_tx.clone(),
                    },
                );

                let ctx = Arc::clone(&self.ctx);
                let pool_tx = pool_tx.clone();
                let shutdown = Arc::clone(shutdown);
                let actor_tx = conn_tx.clone();
                self.tasks.push(Box::pin(async move {
                    ctx.run_connection(pool_tx, shutdown, actor_tx, id, generation, conn_rx)
                        .await;
                }));

                if conn_tx.send(req).await.is_err() {
                    self.remove_connection_if_match(id, generation);
                }
            }
            Msg::Idle { id, generation } => {
                if self.matches_generation(id, generation) {
                    self.add_idle(id, generation);
                    trace!(%id, generation, "connection marked idle");
                }
            }
            Msg::Shutdown { id, generation } => {
                self.remove_connection_if_match(id, generation);
                trace!(%id, generation, "connection removed");
            }
        }
    }

    async fn run(mut self, pool_tx: mpsc::Sender<Msg>, shutdown: Arc<Shutdown>) {
        loop {
            if shutdown.is_closed() {
                break;
            }

            let shutdown_wait = shutdown.notify.notified();
            tokio::select! {
                biased;
                () = shutdown_wait => break,
                msg = self.rx.recv() => {
                    match msg {
                        Some(msg) => self.handle(&pool_tx, &shutdown, msg).await,
                        None => break,
                    }
                }
                _ = self.tasks.next(), if !self.tasks.is_empty() => {}
            }
        }

        self.rx.close();
        self.connections.clear();
        self.idle.clear();
        while self.tasks.next().await.is_some() {}
    }
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// A connection pool for a specific ALPN protocol.
///
/// Manages iroh connections with idle timeout and connection limits.
/// Connections are automatically reused and cleaned up.
#[derive(Clone)]
pub struct ConnectionPool {
    shared: Arc<Shared>,
}

impl std::fmt::Debug for ConnectionPool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConnectionPool")
            .field("alpn", &String::from_utf8_lossy(&self.shared.alpn))
            .finish_non_exhaustive()
    }
}

impl Drop for ConnectionPool {
    fn drop(&mut self) {
        if Arc::strong_count(&self.shared) == 1 {
            self.shared.shutdown();
        }
    }
}

impl ConnectionPool {
    /// Create a new connection pool for the given ALPN.
    #[must_use]
    pub fn new(endpoint: Endpoint, alpn: &[u8], options: PoolOptions) -> Self {
        Self {
            shared: Arc::new(Shared::new(endpoint, alpn, options)),
        }
    }

    /// Create a pool for a specific tonic service.
    ///
    /// The ALPN is derived from the service name (e.g. `"my.Service"` → `"/my.Service/1.0"`).
    #[must_use]
    pub fn for_service<S: tonic::server::NamedService>(
        endpoint: Endpoint,
        options: PoolOptions,
    ) -> Self {
        let alpn = crate::alpn::service_to_alpn::<S>();
        Self::new(endpoint, &alpn, options)
    }

    /// Get or create a connection to the given peer.
    ///
    /// Returns a [`ConnectionRef`] that keeps the connection alive.
    /// The underlying connection may be reused from a previous request.
    ///
    /// # Errors
    ///
    /// Returns [`PoolError`] if the pool is shut down, the connection times
    /// out, a pooled actor is saturated, or the connection limit is reached.
    #[tracing::instrument(name = "pool.get_or_connect", skip(self), fields(peer_id = %id))]
    pub async fn get_or_connect(&self, id: EndpointId) -> Result<ConnectionRef, PoolError> {
        let tx = self.shared.tx()?;

        for attempt in 0..OPEN_STREAM_RETRIES {
            let (reply_tx, reply_rx) = oneshot::channel();
            tx.send(Msg::Request(RequestMsg { id, tx: reply_tx }))
                .await
                .map_err(|_| PoolError::Shutdown)?;

            match reply_rx.await.map_err(|_| PoolError::Shutdown)? {
                Err(PoolError::Closed) if attempt + 1 < OPEN_STREAM_RETRIES => {}
                other => return other,
            }
        }

        Err(PoolError::Closed)
    }

    async fn invalidate(&self, id: EndpointId, generation: u64) {
        if let Ok(tx) = self.shared.tx() {
            let _ = tx.send(Msg::Shutdown { id, generation }).await;
        }
    }

    async fn open_stream(&self, id: EndpointId) -> std::io::Result<PooledStream> {
        let mut last_error = None;

        for attempt in 0..OPEN_STREAM_RETRIES {
            let conn_ref = self
                .get_or_connect(id)
                .await
                .map_err(std::io::Error::other)?;

            match conn_ref.open_bi().await {
                Ok((send, recv)) => {
                    return Ok(PooledStream {
                        send,
                        recv,
                        _ref: conn_ref,
                    });
                }
                Err(err) => {
                    let generation = conn_ref.generation();
                    let peer_id = conn_ref.id();
                    self.invalidate(peer_id, generation).await;

                    let io_err = std::io::Error::new(std::io::ErrorKind::ConnectionAborted, err);
                    last_error = Some(io_err);
                    if attempt + 1 == OPEN_STREAM_RETRIES {
                        break;
                    }
                }
            }
        }

        Err(last_error.unwrap_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::ConnectionAborted,
                "failed to open stream on pooled connection",
            )
        }))
    }

    /// Get a tonic [`Channel`] for the given peer, backed by a pooled connection.
    ///
    /// The returned channel opens a bidirectional QUIC stream on the pooled
    /// connection. The connection is kept alive as long as the channel's
    /// underlying transport exists.
    ///
    /// # Errors
    ///
    /// Returns an error if the connection cannot be established or the tonic
    /// channel cannot be created.
    #[tracing::instrument(name = "pool.channel", skip(self), fields(peer_id = %id))]
    pub async fn channel(&self, id: EndpointId) -> crate::Result<Channel> {
        let pool = self.clone();
        let channel = tonic::transport::Endpoint::try_from("http://[::]:50051")?
            .connect_with_connector(service_fn(move |_: Uri| {
                let pool = pool.clone();
                async move { pool.open_stream(id).await.map(TokioIo::new) }
            }))
            .await?;
        Ok(channel)
    }
}

// ---------------------------------------------------------------------------
// Internal stream wrapper (keeps ConnectionRef alive)
// ---------------------------------------------------------------------------

struct PooledStream {
    send: iroh::endpoint::SendStream,
    recv: iroh::endpoint::RecvStream,
    _ref: ConnectionRef,
}

impl Unpin for PooledStream {}

impl AsyncRead for PooledStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.recv).poll_read(cx, buf)
    }
}

impl AsyncWrite for PooledStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        match Pin::new(&mut self.send).poll_write(cx, buf) {
            Poll::Ready(Ok(n)) => Poll::Ready(Ok(n)),
            Poll::Ready(Err(e)) => Poll::Ready(Err(write_error_to_io(e))),
            Poll::Pending => Poll::Pending,
        }
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.send).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.send).poll_shutdown(cx)
    }
}

// ---------------------------------------------------------------------------
// Connection counting
// ---------------------------------------------------------------------------

#[derive(Debug)]
struct ConnectionCounterInner {
    count: AtomicUsize,
    notify: Notify,
}

#[derive(Debug, Clone)]
struct ConnectionCounter {
    inner: Arc<ConnectionCounterInner>,
}

impl ConnectionCounter {
    fn new() -> Self {
        Self {
            inner: Arc::new(ConnectionCounterInner {
                count: AtomicUsize::new(0),
                notify: Notify::new(),
            }),
        }
    }

    fn current(&self) -> usize {
        self.inner.count.load(Ordering::SeqCst)
    }

    fn get_one(&self) -> OneConnection {
        self.inner.count.fetch_add(1, Ordering::SeqCst);
        OneConnection {
            inner: Arc::clone(&self.inner),
        }
    }

    fn is_idle(&self) -> bool {
        self.inner.count.load(Ordering::SeqCst) == 0
    }
}

/// Guard for a single active reference. Decrements the counter on drop.
#[derive(Debug)]
struct OneConnection {
    inner: Arc<ConnectionCounterInner>,
}

impl Drop for OneConnection {
    fn drop(&mut self) {
        if self.inner.count.fetch_sub(1, Ordering::SeqCst) == 1 {
            self.inner.notify.notify_one();
        }
    }
}

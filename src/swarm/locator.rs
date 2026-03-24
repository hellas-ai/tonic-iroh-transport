//! Locator: races connection attempts to discovered peers.

use std::{
    marker::PhantomData,
    pin::Pin,
    task::{Context, Poll},
    time::Duration,
};

use crate::channel::IrohChannel;
use futures_util::{future::BoxFuture, stream::FuturesUnordered, Stream, StreamExt};
use iroh::Endpoint;
use tokio::{sync::mpsc, task::JoinHandle, time};
use tonic::server::NamedService;

use tracing::debug;

use super::discovery::Peer;
use crate::{ConnectionPool, PoolOptions, Result};

/// Configuration for connection racing.
#[derive(Clone, Debug)]
pub struct LocatorConfig {
    /// Maximum simultaneous connection attempts.
    pub max_inflight: usize,
    /// Timeout for each individual attempt.
    pub timeout_each: Duration,
    /// Overall timeout for the locator.
    pub timeout: Option<Duration>,
    /// Buffer size for successful connections.
    pub limit: usize,
}

impl Default for LocatorConfig {
    fn default() -> Self {
        Self {
            max_inflight: 8,
            timeout_each: Duration::from_secs(2),
            timeout: None,
            limit: 4,
        }
    }
}

/// A handle that races connections and yields successful channels.
pub struct Locator {
    task: JoinHandle<()>,
    rx: mpsc::Receiver<Result<IrohChannel>>,
}

impl Locator {
    /// Await the first successful channel and stop the background task.
    ///
    /// # Errors
    ///
    /// Returns an error if no peer could be reached.
    pub async fn first(mut self) -> Result<IrohChannel> {
        let mut last_error = None;

        while let Some(res) = self.rx.recv().await {
            match res {
                Ok(channel) => {
                    self.task.abort();
                    return Ok(channel);
                }
                Err(err) => {
                    last_error = Some(err);
                }
            }
        }

        Err(last_error.unwrap_or_else(|| crate::Error::connection("no peers reachable")))
    }

    /// Spawn a locator over a stream of peers.
    pub fn spawn<S, St>(endpoint: Endpoint, peers: St, opts: LocatorConfig) -> Locator
    where
        S: NamedService + Send + 'static,
        St: Stream<Item = Result<Peer>> + Send + 'static,
    {
        let pool = ConnectionPool::for_service::<S>(
            endpoint,
            PoolOptions {
                connect_timeout: opts.timeout_each,
                ..PoolOptions::default()
            },
        );
        Self::spawn_with_pool::<S, St>(pool, peers, opts)
    }

    /// Spawn a locator over a stream of peers using a shared connection pool.
    pub(crate) fn spawn_with_pool<S, St>(
        pool: ConnectionPool,
        peers: St,
        opts: LocatorConfig,
    ) -> Locator
    where
        S: NamedService + Send + 'static,
        St: Stream<Item = Result<Peer>> + Send + 'static,
    {
        let (tx, rx) = mpsc::channel::<Result<IrohChannel>>(opts.limit);
        let mut state = LocatorState::<S, St>::new(pool, peers, opts, tx);

        let task = tokio::spawn(async move {
            state.run().await;
        });

        Locator { task, rx }
    }
}

impl Stream for Locator {
    type Item = Result<IrohChannel>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        Pin::new(&mut this.rx).poll_recv(cx)
    }
}

impl Drop for Locator {
    fn drop(&mut self) {
        self.task.abort();
    }
}

struct LocatorState<S, St>
where
    S: NamedService + Send + 'static,
    St: Stream<Item = Result<Peer>> + Send + 'static,
{
    pool: ConnectionPool,
    peers: Pin<Box<St>>,
    inflight: FuturesUnordered<BoxFuture<'static, Result<IrohChannel>>>,
    stream_exhausted: bool,
    opts: LocatorConfig,
    tx: mpsc::Sender<Result<IrohChannel>>,
    _marker: PhantomData<S>,
}

impl<S, St> LocatorState<S, St>
where
    S: NamedService + Send + 'static,
    St: Stream<Item = Result<Peer>> + Send + 'static,
{
    fn new(
        pool: ConnectionPool,
        peers: St,
        opts: LocatorConfig,
        tx: mpsc::Sender<Result<IrohChannel>>,
    ) -> Self {
        Self {
            pool,
            peers: Box::pin(peers),
            inflight: FuturesUnordered::new(),
            stream_exhausted: false,
            opts,
            tx,
            _marker: PhantomData,
        }
    }

    async fn run(&mut self) {
        let timeout = self.opts.timeout;
        let run = async {
            loop {
                let can_discover =
                    !self.stream_exhausted && self.inflight.len() < self.opts.max_inflight;
                let has_inflight = !self.inflight.is_empty();

                if !can_discover && !has_inflight {
                    debug!("locator: no more peers to discover and no inflight attempts");
                    break;
                }

                tokio::select! {
                    biased;

                    // Check completed connection attempts first (higher priority).
                    Some(res) = self.inflight.next(), if has_inflight => {
                        debug!("locator: connection attempt completed");
                        if self.tx.send(res).await.is_err() {
                            break;
                        }
                    }

                    // Discover new peers when there's room for more inflight attempts.
                    peer = self.peers.next(), if can_discover => {
                        match peer {
                            Some(Ok(peer)) => {
                                debug!(
                                    peer_id = %peer.id(),
                                    source = peer.source(),
                                    "locator: discovered peer, pushing connect attempt"
                                );
                                self.push_attempt(&peer);
                            }
                            Some(Err(err)) => {
                                debug!(?err, "locator: discovery stream error");
                                let _ = self.tx.send(Err(err)).await;
                            }
                            None => {
                                debug!("locator: discovery stream exhausted");
                                self.stream_exhausted = true;
                            }
                        }
                    }
                }
            }
        };

        if let Some(timeout) = timeout {
            let _ = time::timeout(timeout, run).await;
        } else {
            run.await;
        }
    }

    fn push_attempt(&mut self, peer: &Peer) {
        let pool = self.pool.clone();
        let timeout = self.opts.timeout_each;
        let node_id = peer.id();

        let fut: BoxFuture<'static, Result<IrohChannel>> = Box::pin(async move {
            // The pool timeout covers iroh connection establishment; this outer
            // timeout also bounds post-connect stream/channel setup.
            time::timeout(timeout, pool.channel(node_id))
                .await
                .map_err(|_| {
                    crate::Error::connection(format!("connection timed out after {timeout:?}"))
                })?
        });
        self.inflight.push(fut);
    }
}

#[cfg(test)]
mod tests {
    use tokio::sync::mpsc;

    use super::Locator;
    use crate::channel::IrohChannel;

    #[tokio::test]
    async fn first_skips_errors_until_success() {
        let dummy = IrohChannel::dummy().await;
        let (tx, rx) = mpsc::channel(4);
        let task = tokio::spawn(async move {
            tx.send(Err(crate::Error::connection("initial failure")))
                .await
                .expect("error send should succeed");
            tx.send(Ok(dummy))
                .await
                .expect("success send should succeed");
        });

        let locator = Locator { task, rx };
        let result = locator.first().await;

        assert!(result.is_ok(), "expected success after initial failure");
    }

    #[tokio::test]
    async fn first_returns_last_error_if_no_success_arrives() {
        let (tx, rx) = mpsc::channel(4);
        let task = tokio::spawn(async move {
            tx.send(Err(crate::Error::connection("first failure")))
                .await
                .expect("first error send should succeed");
            tx.send(Err(crate::Error::connection("final failure")))
                .await
                .expect("second error send should succeed");
        });

        let locator = Locator { task, rx };
        let result = locator.first().await;

        match result {
            Err(crate::Error::Connection(message)) => assert_eq!(message, "final failure"),
            other => panic!("expected final connection error, got {other:?}"),
        }
    }
}

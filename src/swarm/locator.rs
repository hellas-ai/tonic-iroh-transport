//! Locator: races connection attempts to discovered peers.

use std::{marker::PhantomData, pin::Pin, task::{Context, Poll}, time::Duration};

use futures_util::{future::BoxFuture, stream::FuturesUnordered, Stream, StreamExt};
use iroh::{Endpoint, EndpointAddr};
use tokio::{sync::mpsc, task::JoinHandle, time};
use tonic::server::NamedService;
use tonic::transport::Channel;

use tracing::debug;

use crate::{IrohConnect, Result};
use super::discovery::Peer;

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
    rx: mpsc::Receiver<Result<Channel>>,
}

impl Locator {
    /// Await the first successful channel and stop the background task.
    pub async fn first(mut self) -> Result<Channel> {
        match self.rx.recv().await {
            Some(res) => {
                self.task.abort();
                res
            }
            None => Err(crate::Error::connection("no peers reachable")),
        }
    }

    /// Spawn a locator over a stream of peers.
    pub fn spawn<S, St>(endpoint: Endpoint, peers: St, opts: LocatorConfig) -> Locator
    where
        S: NamedService + Send + 'static,
        St: Stream<Item = Result<Peer>> + Send + 'static,
    {
        let (tx, rx) = mpsc::channel::<Result<Channel>>(opts.limit);
        let mut state = LocatorState::<S, St>::new(endpoint, peers, opts, tx);

        let task = tokio::spawn(async move {
            state.run().await;
        });

        Locator { task, rx }
    }
}

impl Stream for Locator {
    type Item = Result<Channel>;

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
    endpoint: Endpoint,
    peers: Pin<Box<St>>,
    inflight: FuturesUnordered<BoxFuture<'static, Result<Channel>>>,
    stream_exhausted: bool,
    opts: LocatorConfig,
    tx: mpsc::Sender<Result<Channel>>,
    _marker: PhantomData<S>,
}

impl<S, St> LocatorState<S, St>
where
    S: NamedService + Send + 'static,
    St: Stream<Item = Result<Peer>> + Send + 'static,
{
    fn new(endpoint: Endpoint, peers: St, opts: LocatorConfig, tx: mpsc::Sender<Result<Channel>>) -> Self {
        Self {
            endpoint,
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
                let can_discover = !self.stream_exhausted
                    && self.inflight.len() < self.opts.max_inflight;
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
                                self.push_attempt(peer);
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

    fn push_attempt(&mut self, peer: Peer) {
        let ep = self.endpoint.clone();
        let timeout = self.opts.timeout_each;
        let node_id = peer.id();

        let fut: BoxFuture<'static, Result<Channel>> = Box::pin(async move {
            let builder = S::connect(&ep, EndpointAddr::new(node_id)).connect_timeout(timeout);
            builder.await
        });
        self.inflight.push(fut);
    }
}

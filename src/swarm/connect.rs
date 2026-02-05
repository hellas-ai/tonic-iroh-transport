//! Helper to race connections against a discovery stream.
//!
//! This is intended to be the default client path: take the stream of
//! `EndpointId`s from [`ServiceRegistry`] and fan out a bounded number of
//! connection attempts. Keep the handle if you want warm fallbacks; drop it
//! to stop discovery.

use std::time::Duration;

use futures_util::{future::poll_fn, stream::FuturesUnordered, Stream, StreamExt};
use iroh::{Endpoint, EndpointAddr, EndpointId};
use tokio::{sync::mpsc, task::JoinHandle, time};
use tonic::server::NamedService;
use tonic::transport::Channel;

use crate::{IrohConnect, Result};

/// Options controlling concurrent connection attempts.
#[derive(Clone, Debug)]
pub struct ConnectOptions {
    /// Maximum simultaneous connection attempts.
    pub max_inflight: usize,
    /// Timeout for each individual attempt.
    pub per_attempt_timeout: Duration,
    /// Optional overall timeout for the entire race.
    pub overall_timeout: Option<Duration>,
    /// How many successful connections to buffer.
    pub success_buffer: usize,
}

impl Default for ConnectOptions {
    fn default() -> Self {
        Self {
            max_inflight: 8,
            per_attempt_timeout: Duration::from_secs(2),
            overall_timeout: None,
            success_buffer: 4,
        }
    }
}

/// Background connection racer. Holds internal task and buffered successes.
pub struct Locator {
    task: JoinHandle<()>,
    rx: mpsc::Receiver<Result<Channel>>,
}

impl Locator {
    /// Await the next connection attempt result (success or error).
    pub async fn next(&mut self) -> Option<Result<Channel>> {
        self.rx.recv().await
    }

    /// Convenience: return the first successful channel, dropping the rest.
    pub async fn first(mut self) -> Result<Channel> {
        match self.rx.recv().await {
            Some(res) => {
                // Stop background task; receiver drops here.
                self.task.abort();
                res
            }
            None => Err(crate::Error::connection("no peers reachable")),
        }
    }
}

impl Drop for Locator {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Spawn a background task that continuously races connection attempts and
/// yields successes through a buffered channel.
pub fn connect<S, St>(
    endpoint: Endpoint,
    peers: St,
    opts: ConnectOptions,
) -> Locator
where
    S: NamedService + Send + 'static,
    St: Stream<Item = Result<EndpointId>> + Send + 'static,
{
    let (tx, rx) = mpsc::channel::<Result<Channel>>(opts.success_buffer);
    let mut peers = Box::pin(peers);
    let endpoint = endpoint.clone();

    let task = tokio::spawn(async move {
        let run = async {
            let mut inflight = FuturesUnordered::new();
            let mut stream_exhausted = false;

            loop {
                // Refill inflight up to cap.
                while inflight.len() < opts.max_inflight && !stream_exhausted {
                    let next = poll_fn(|cx| peers.as_mut().poll_next(cx)).await;
                    match next {
                        Some(Ok(node_id)) => {
                            let ep = endpoint.clone();
                            inflight.push(async move {
                                let builder = S::connect(&ep, EndpointAddr::new(node_id))
                                    .connect_timeout(opts.per_attempt_timeout);
                                builder.await
                            });
                        }
                        Some(Err(err)) => {
                            tracing::debug!(error = %err, "discovery entry skipped");
                        }
                        None => {
                            stream_exhausted = true;
                        }
                    }
                }

                // If nothing left to do, exit.
                if inflight.is_empty() && stream_exhausted {
                    break;
                }

                match inflight.next().await {
                    Some(Ok(channel)) => {
                        if tx.send(Ok(channel)).await.is_err() {
                            break;
                        }
                    }
                    Some(Err(err)) => {
                        // Only forward the error if the receiver is listening.
                        if tx.send(Err(err)).await.is_err() {
                            break;
                        }
                    }
                    None => {
                        // No inflight but stream may still have items; continue loop.
                    }
                }
            }
        };

        if let Some(overall) = opts.overall_timeout {
            let _ = time::timeout(overall, run).await;
        } else {
            run.await;
        }
        // tx drops here and closes the channel.
    });

    Locator { task, rx }
}

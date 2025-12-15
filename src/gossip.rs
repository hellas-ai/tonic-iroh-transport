//! Gossip protocol integration for tonic-iroh.
//!
//! This module provides typed gossip messaging that integrates with tonic-style services.
//!
//! # Example
//!
//! ```no_run
//! use tonic_iroh_transport::gossip::{GossipHandler, GossipRequest};
//! use tonic_iroh_transport::{iroh, TransportBuilder};
//! use tonic::Status;
//! use async_trait::async_trait;
//! use bytes::Bytes;
//!
//! #[derive(Clone)]
//! struct MyService;
//!
//! #[async_trait]
//! impl GossipHandler<MyMessage> for MyService {
//!     async fn handle(&self, _request: GossipRequest<MyMessage>) -> Result<(), Status> {
//!         Ok(())
//!     }
//! }
//!
//! #[derive(Clone, PartialEq, prost::Message)]
//! struct MyMessage {
//!     #[prost(bytes, tag = "1")]
//!     data: Bytes,
//! }
//! impl prost::Name for MyMessage {
//!     const NAME: &'static str = "MyMessage";
//!     const PACKAGE: &'static str = "pkg";
//! }
//!
//! # async fn run() -> Result<(), Box<dyn std::error::Error>> {
//! let endpoint = iroh::Endpoint::builder().bind().await?;
//! let guard = TransportBuilder::new(endpoint)
//!     .add_gossip::<MyMessage, _>(MyService)
//!     .spawn()
//!     .await?;
//! guard.shutdown().await?;
//! # Ok(())
//! # }
//! ```

use std::future::Future;
use std::marker::PhantomData;
use std::pin::Pin;
use std::time::Instant;

use async_trait::async_trait;
use futures_util::StreamExt;
use iroh_gossip::net::Gossip;
use iroh_gossip::proto::TopicId;
use prost::bytes::Bytes;
use prost::Name;
use tonic::Status;
use tracing::Instrument;

pub use iroh_gossip::api::{
    ApiError, Event, GossipSender as RawGossipSender, GossipTopic, Message,
};
pub use iroh_gossip::net::Gossip as GossipInstance;
use tokio::sync::broadcast;

/// Derive a [`TopicId`] from a protobuf message type.
///
/// Uses the message's package and name from [`prost::Name`] to create a deterministic topic ID.
pub fn topic_for<T: Name>() -> TopicId {
    let path = format!("{}.{}", T::PACKAGE, T::NAME);
    TopicId::from(*blake3::hash(path.as_bytes()).as_bytes())
}

/// Derive a [`TopicId`] from a protobuf message type with a discriminator.
///
/// Useful for creating per-instance topics (e.g., per-room, per-document).
pub fn topic_for_with<T: Name>(discriminator: impl AsRef<[u8]>) -> TopicId {
    let discriminator_hex = discriminator
        .as_ref()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>();
    let path = format!("{}.{}/{}", T::PACKAGE, T::NAME, discriminator_hex);
    TopicId::from(*blake3::hash(path.as_bytes()).as_bytes())
}

/// Context information for gossip messages.
///
/// Similar to [`crate::IrohContext`] but for gossip messages.
#[derive(Debug, Clone)]
pub struct GossipContext {
    /// The topic this message was received on.
    pub topic_id: TopicId,
    /// When the message was received.
    pub received_at: Instant,
    /// The peer that delivered this message (not necessarily the original sender).
    pub delivered_from: iroh::PublicKey,
}

/// Broadcast capability for sending messages to a gossip topic.
#[derive(Debug, Clone)]
pub struct Sender {
    inner: RawGossipSender,
}

impl Sender {
    /// Create a new sender from a raw gossip sender.
    pub(crate) fn new(inner: RawGossipSender) -> Self {
        Self { inner }
    }

    /// Broadcast a typed message to all peers in the topic.
    pub async fn broadcast<T: prost::Message>(&self, msg: &T) -> std::result::Result<(), ApiError> {
        let bytes = msg.encode_to_vec();
        let len = bytes.len();
        let span = tracing::trace_span!("gossip_broadcast", bytes = len);
        self.inner
            .broadcast(Bytes::from(bytes))
            .instrument(span)
            .await
    }

    /// Broadcast a typed message only to direct neighbors.
    pub async fn broadcast_neighbors<T: prost::Message>(
        &self,
        msg: &T,
    ) -> std::result::Result<(), ApiError> {
        let bytes = msg.encode_to_vec();
        let len = bytes.len();
        let span = tracing::trace_span!("gossip_broadcast_neighbors", bytes = len);
        self.inner
            .broadcast_neighbors(Bytes::from(bytes))
            .instrument(span)
            .await
    }
}

/// Request wrapper for gossip messages.
///
/// Mirrors [`tonic::Request`] but for gossip messages.
pub struct GossipRequest<T> {
    message: T,
    context: GossipContext,
    sender: Sender,
}

impl<T> GossipRequest<T> {
    /// Create a new gossip request.
    pub(crate) fn new(message: T, context: GossipContext, sender: Sender) -> Self {
        Self {
            message,
            context,
            sender,
        }
    }

    /// Consume the request and return the inner message.
    pub fn into_inner(self) -> T {
        self.message
    }

    /// Get a reference to the message.
    pub fn get_ref(&self) -> &T {
        &self.message
    }

    /// Get the gossip context.
    pub fn context(&self) -> &GossipContext {
        &self.context
    }

    /// Get the sender for broadcasting replies.
    pub fn sender(&self) -> &Sender {
        &self.sender
    }
}

/// Handler trait for gossip messages.
///
/// Implement this trait on your service struct to handle gossip messages.
/// The same struct can implement both tonic service traits and `GossipHandler`.
#[async_trait]
pub trait GossipHandler<T>: Clone + Send + Sync + 'static
where
    T: prost::Message + Name,
{
    /// Handle an incoming gossip message.
    async fn handle(&self, request: GossipRequest<T>) -> Result<(), Status>;
}

/// A typed gossip topic for sending and receiving messages.
///
/// This wraps [`GossipTopic`] with typed serialization/deserialization.
pub struct Topic<T> {
    inner: GossipTopic,
    topic_id: TopicId,
    _marker: PhantomData<T>,
}

impl<T> Topic<T>
where
    T: prost::Message + Default,
{
    /// Create a new typed topic from a raw gossip topic.
    pub fn new(inner: GossipTopic, topic_id: TopicId) -> Self {
        Self {
            inner,
            topic_id,
            _marker: PhantomData,
        }
    }

    /// Broadcast a typed message to all peers in the topic.
    pub async fn broadcast(&mut self, msg: &T) -> Result<(), ApiError> {
        let bytes = msg.encode_to_vec();
        self.inner.broadcast(Bytes::from(bytes)).await
    }

    /// Broadcast a typed message only to direct neighbors.
    pub async fn broadcast_neighbors(&mut self, msg: &T) -> Result<(), ApiError> {
        let bytes = msg.encode_to_vec();
        self.inner.broadcast_neighbors(Bytes::from(bytes)).await
    }

    /// Wait until connected to at least one peer.
    pub async fn joined(&mut self) -> Result<(), ApiError> {
        self.inner.joined().await
    }

    /// Check if connected to at least one peer.
    pub fn is_joined(&self) -> bool {
        self.inner.is_joined()
    }

    /// Get the topic ID.
    pub fn topic_id(&self) -> TopicId {
        self.topic_id
    }
}

impl<T> Topic<T>
where
    T: prost::Message + Default,
{
    /// Receive the next message from the topic.
    ///
    /// This filters out neighbor events and only returns received messages.
    pub async fn recv(&mut self) -> Option<Result<(GossipContext, T), GossipError>> {
        use futures_util::StreamExt;
        loop {
            match self.inner.next().await {
                Some(Ok(Event::Received(msg))) => {
                    let ctx = GossipContext {
                        topic_id: self.topic_id,
                        received_at: Instant::now(),
                        delivered_from: msg.delivered_from,
                    };
                    return Some(match T::decode(msg.content) {
                        Ok(decoded) => Ok((ctx, decoded)),
                        Err(e) => Err(GossipError::Decode(e)),
                    });
                }
                Some(Ok(ev @ (Event::NeighborUp(_) | Event::NeighborDown(_) | Event::Lagged))) => {
                    tracing::trace!(?ev, "gossip neighbor event");
                }
                Some(Err(e)) => return Some(Err(GossipError::Api(e))),
                None => return None,
            }
        }
    }
}

/// Error type for gossip operations.
#[derive(Debug, thiserror::Error)]
pub enum GossipError {
    /// Failed to decode protobuf message.
    #[error("failed to decode message: {0}")]
    Decode(#[from] prost::DecodeError),
    /// Gossip API error.
    #[error("gossip error: {0}")]
    Api(#[from] ApiError),
}

impl From<GossipError> for Status {
    fn from(err: GossipError) -> Self {
        match err {
            GossipError::Decode(e) => Status::invalid_argument(format!("decode error: {e}")),
            GossipError::Api(e) => Status::internal(format!("gossip error: {e}")),
        }
    }
}

/// Helper to join a typed gossip topic.
pub async fn join<T>(gossip: &Gossip, bootstrap: Vec<iroh::PublicKey>) -> Result<Topic<T>, ApiError>
where
    T: prost::Message + Name + Default,
{
    let topic_id = topic_for::<T>();
    let inner = gossip.subscribe(topic_id, bootstrap).await?;
    Ok(Topic::new(inner, topic_id))
}

/// Helper to join a typed gossip topic with a discriminator.
pub async fn join_with<T>(
    gossip: &Gossip,
    discriminator: impl AsRef<[u8]>,
    bootstrap: Vec<iroh::PublicKey>,
) -> Result<Topic<T>, ApiError>
where
    T: prost::Message + Name + Default,
{
    let topic_id = topic_for_with::<T>(discriminator);
    let inner = gossip.subscribe(topic_id, bootstrap).await?;
    Ok(Topic::new(inner, topic_id))
}

/// Configuration for gossip sessions.
#[derive(Debug, Clone, Default)]
pub struct GossipConfig {
    /// Bootstrap peers for the gossip network.
    pub bootstrap: Vec<iroh::PublicKey>,
}

type HandlerFuture = Pin<Box<dyn Future<Output = ()> + Send + 'static>>;

/// Type-erased handler spawner for gossip subscriptions.
pub type HandlerSpawner =
    Box<dyn FnOnce(Gossip, GossipConfig, broadcast::Receiver<()>) -> HandlerFuture + Send>;

/// Create a handler spawner for a typed `GossipHandler`.
pub fn handler<T, H>(handler: H) -> HandlerSpawner
where
    T: prost::Message + Name + Send + Default + 'static,
    H: GossipHandler<T>,
{
    Box::new(move |gossip, cfg, mut shutdown_rx| {
        let bootstrap = cfg.bootstrap.clone();
        Box::pin(async move {
            let topic_id = topic_for::<T>();
            let bootstrap_len = bootstrap.len();
            let handler_span = tracing::debug_span!(
                "gossip_handler",
                topic = ?topic_id,
                bootstrap = bootstrap_len
            );

            async move {
                let topic = match gossip.subscribe(topic_id, bootstrap).await {
                    Ok(t) => t,
                    Err(e) => {
                        tracing::error!("failed to subscribe to gossip topic: {e}");
                        return;
                    }
                };

                let (raw_sender, mut receiver) = topic.split();
                let sender = Sender::new(raw_sender);

                loop {
                    tokio::select! {
                        _ = shutdown_rx.recv() => {
                            tracing::debug!("gossip handler received shutdown signal");
                            break;
                        }
                        msg = receiver.next() => {
                            match msg {
                                Some(Ok(Event::Received(msg))) => {
                                    let delivered_from = msg.delivered_from;
                                    let content = msg.content;
                                    let len = content.len();
                                    let recv_span = tracing::trace_span!(
                                        "gossip_recv",
                                        topic = ?topic_id,
                                        from = %delivered_from.fmt_short(),
                                        len = len
                                    );

                                    async {
                                        let ctx = GossipContext {
                                            topic_id,
                                            received_at: Instant::now(),
                                            delivered_from,
                                        };

                                        match T::decode(content) {
                                            Ok(decoded) => {
                                                let request = GossipRequest::new(decoded, ctx, sender.clone());
                                                if let Err(e) = handler.handle(request).await {
                                                    tracing::warn!("gossip handler error: {e}");
                                                }
                                            }
                                            Err(e) => {
                                                tracing::warn!("failed to decode gossip message: {e}");
                                            }
                                        }
                                    }
                                    .instrument(recv_span)
                                    .await;
                                }
                                Some(Ok(Event::NeighborUp(peer))) => {
                                    tracing::debug!("gossip neighbor up: {}", peer.fmt_short());
                                }
                                Some(Ok(Event::NeighborDown(peer))) => {
                                    tracing::debug!("gossip neighbor down: {}", peer.fmt_short());
                                }
                                Some(Ok(Event::Lagged)) => {
                                    tracing::warn!("gossip subscription lagged");
                                }
                                Some(Err(e)) => {
                                    tracing::error!("gossip error: {e}");
                                    break;
                                }
                                None => {
                                    tracing::debug!("gossip subscription ended");
                                    break;
                                }
                            }
                        }
                    }
                }
            }
            .instrument(handler_span)
            .await;
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper to create topic ID from package and name strings (for testing)
    fn topic_from_path(package: &str, name: &str) -> TopicId {
        let path = format!("{}.{}", package, name);
        TopicId::from(*blake3::hash(path.as_bytes()).as_bytes())
    }

    /// Helper to create topic ID with discriminator (for testing)
    fn topic_from_path_with(package: &str, name: &str, discriminator: &[u8]) -> TopicId {
        let discriminator_hex = discriminator
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>();
        let path = format!("{}.{}/{}", package, name, discriminator_hex);
        TopicId::from(*blake3::hash(path.as_bytes()).as_bytes())
    }

    #[test]
    fn test_topic_for_deterministic() {
        let topic1 = topic_from_path("test.package", "TestMessage");
        let topic2 = topic_from_path("test.package", "TestMessage");
        assert_eq!(topic1, topic2);
    }

    #[test]
    fn test_topic_for_different_types() {
        let topic1 = topic_from_path("test.package", "MessageA");
        let topic2 = topic_from_path("test.package", "MessageB");
        assert_ne!(topic1, topic2);
    }

    #[test]
    fn test_topic_for_with_discriminator() {
        let topic1 = topic_from_path_with("test.package", "TestMessage", b"room1");
        let topic2 = topic_from_path_with("test.package", "TestMessage", b"room2");
        let topic3 = topic_from_path_with("test.package", "TestMessage", b"room1");

        assert_ne!(topic1, topic2);
        assert_eq!(topic1, topic3);
    }

    #[test]
    fn test_discriminator_changes_topic() {
        let base = topic_from_path("test.package", "TestMessage");
        let with_disc = topic_from_path_with("test.package", "TestMessage", b"room1");
        assert_ne!(base, with_disc);
    }
}

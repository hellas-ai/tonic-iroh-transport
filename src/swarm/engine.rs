//! Swarm engine: merges peer feeds with priority and deduplication.

use std::collections::HashSet;
use std::pin::Pin;
use std::task::{Context, Poll};

use futures_util::{stream::SelectAll, Stream};
use iroh::EndpointId;

use crate::Result;

use super::discovery::Peer;
use super::peers::{scope_matches, PeerFeed, PeerFeedSpec};

/// A tagged feed: wraps a PeerFeed to attach source metadata to each item.
struct TaggedFeed {
    name: &'static str,
    trust: u8,
    inner: PeerFeed,
}

impl Stream for TaggedFeed {
    type Item = Result<Peer>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        match Pin::new(&mut this.inner).poll_next(cx) {
            Poll::Ready(Some(Ok(discovered))) => {
                let peer = Peer::new(discovered, this.name, this.trust);
                Poll::Ready(Some(Ok(peer)))
            }
            Poll::Ready(Some(Err(e))) => Poll::Ready(Some(Err(e))),
            Poll::Ready(None) => Poll::Ready(None),
            Poll::Pending => Poll::Pending,
        }
    }
}

type TaggedPeerFeed = Pin<Box<dyn Stream<Item = Result<Peer>> + Send>>;

/// Merges multiple peer feeds into a single deduped stream.
pub struct SwarmEngine {
    inner: SelectAll<TaggedPeerFeed>,
    seen: HashSet<[u8; 32]>,
    local_id: EndpointId,
}

impl SwarmEngine {
    /// Build an engine for a specific ALPN using the provided feed specs.
    pub fn new(local_id: EndpointId, alpn: &[u8], mut feeds: Vec<PeerFeedSpec>) -> Self {
        let mut inner = SelectAll::new();
        feeds.sort_by_key(|f| f.priority);
        for spec in feeds.into_iter() {
            if !scope_matches(&spec.scope, alpn) {
                continue;
            }
            let tagged: TaggedPeerFeed = Box::pin(TaggedFeed {
                name: spec.name,
                trust: spec.trust,
                inner: spec.stream,
            });
            inner.push(tagged);
        }
        Self {
            inner,
            seen: HashSet::new(),
            local_id,
        }
    }
}

impl Stream for SwarmEngine {
    type Item = Result<Peer>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        loop {
            match Pin::new(&mut this.inner).poll_next(cx) {
                Poll::Ready(Some(res)) => match res {
                    Ok(peer) => {
                        if peer.id() == this.local_id {
                            continue;
                        }
                        if this.seen.insert(*peer.id().as_bytes()) {
                            return Poll::Ready(Some(Ok(peer)));
                        }
                    }
                    Err(e) => return Poll::Ready(Some(Err(e))),
                },
                Poll::Ready(None) => return Poll::Ready(None),
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use futures_util::StreamExt;
    use iroh::{EndpointId, SecretKey};

    use super::super::peers::{static_feed, Scope};
    use super::SwarmEngine;

    fn endpoint_id(byte: u8) -> EndpointId {
        SecretKey::from([byte; 32]).public()
    }

    #[tokio::test]
    async fn skips_local_peer_and_dedupes() {
        let local = endpoint_id(1);
        let remote = endpoint_id(2);

        let feeds = vec![
            static_feed(vec![local, remote], 10, 100, Scope::Any),
            static_feed(vec![remote], 20, 100, Scope::Any),
        ];
        let mut engine = SwarmEngine::new(local, b"/svc.Test/1.0", feeds);

        let first = engine
            .next()
            .await
            .expect("one remote peer expected")
            .expect("peer should be ok");
        assert_eq!(first.id(), remote);

        assert!(
            engine.next().await.is_none(),
            "duplicate peer should be deduped"
        );
    }
}

//! gRPC channel backed by h2 directly, without hyper.
//!
//! [`IrohChannel`] wraps an [`h2::client::SendRequest`] and implements
//! [`tower::Service`] so that tonic-generated clients can use it as a
//! drop-in transport.

use std::pin::Pin;
use std::task::{Context, Poll};

use bytes::Bytes;
use h2::client::SendRequest;
use http::uri::{Authority, Scheme};
use http::{Request, Response, Uri};
use http_body::Body as HttpBody;
use n0_future::boxed::BoxFuture;
use tokio::io::{AsyncRead, AsyncWrite};
use tonic::body::Body;
use tracing::debug;

use crate::error::Error;

/// Maximum request body size before we reject (4 MiB).
///
/// This is a safety limit for the V1 body pump which buffers outbound data
/// without h2 capacity reservation. A future version should use
/// `SendStream::reserve_capacity` / `poll_capacity` for true back-pressure.
const MAX_OUTBOUND_BODY_BYTES: usize = 4 * 1024 * 1024;

/// A gRPC channel over an HTTP/2 connection managed by [`h2`].
///
/// Created via [`IrohChannel::handshake`]. Cheaply cloneable — all clones
/// share the same underlying HTTP/2 connection.
#[derive(Clone)]
pub struct IrohChannel {
    sender: SendRequest<Bytes>,
    origin: Uri,
}

impl IrohChannel {
    /// Perform the HTTP/2 handshake over `io` and return a ready channel.
    ///
    /// The connection driver is spawned as a background task via
    /// [`n0_future::task::spawn`] (tokio on native, `spawn_local` on wasm).
    ///
    /// `origin` supplies the scheme and authority injected into every outbound
    /// request (matching tonic's `AddOrigin` behaviour).
    ///
    /// # Errors
    ///
    /// Returns an error if the HTTP/2 handshake fails.
    #[cfg(not(target_arch = "wasm32"))]
    pub async fn handshake<T>(io: T, origin: Uri) -> Result<Self, Error>
    where
        T: AsyncRead + AsyncWrite + Send + Unpin + 'static,
    {
        Self::handshake_inner(io, origin).await
    }

    /// Perform the HTTP/2 handshake over `io` and return a ready channel.
    ///
    /// wasm variant — no `Send` bound required.
    ///
    /// # Errors
    ///
    /// Returns an error if the HTTP/2 handshake fails.
    #[cfg(target_arch = "wasm32")]
    pub async fn handshake<T>(io: T, origin: Uri) -> Result<Self, Error>
    where
        T: AsyncRead + AsyncWrite + Unpin + 'static,
    {
        Self::handshake_inner(io, origin).await
    }

    #[cfg(not(target_arch = "wasm32"))]
    async fn handshake_inner<T>(io: T, origin: Uri) -> Result<Self, Error>
    where
        T: AsyncRead + AsyncWrite + Send + Unpin + 'static,
    {
        let (sender, connection) = h2::client::handshake(io).await.map_err(Error::H2)?;
        debug!("h2 client handshake completed");

        n0_future::task::spawn(async move {
            if let Err(e) = connection.await {
                debug!("h2 connection driver error: {e}");
            }
        });

        Ok(Self { sender, origin })
    }

    #[cfg(target_arch = "wasm32")]
    async fn handshake_inner<T>(io: T, origin: Uri) -> Result<Self, Error>
    where
        T: AsyncRead + AsyncWrite + Unpin + 'static,
    {
        let (sender, connection) = h2::client::handshake(io).await.map_err(Error::H2)?;
        debug!("h2 client handshake completed successfully (wasm)");

        // Drive the HTTP/2 state machine in the background.
        n0_future::task::spawn(async move {
            if let Err(e) = connection.await {
                debug!("h2 connection driver error: {e}");
            }
        });

        Ok(Self { sender, origin })
    }
}

#[cfg(test)]
impl IrohChannel {
    /// Create a dummy channel for testing. Performs a real h2 handshake over
    /// an in-memory duplex stream. The channel is functional but has no real
    /// server — requests will fail at the protocol level.
    pub(crate) async fn dummy() -> Self {
        let (client_io, mut server_io) = tokio::io::duplex(1024);
        // Spawn a task that acts as a minimal h2 server to complete the handshake.
        n0_future::task::spawn(async move {
            let mut conn = h2::server::handshake(&mut server_io).await.unwrap();
            // Just accept and ignore — we don't need to serve anything.
            while let Some(Ok(_)) = conn.accept().await {}
        });
        Self::handshake(client_io, Uri::from_static("http://test.local"))
            .await
            .expect("dummy h2 handshake should succeed")
    }
}

impl tower::Service<Request<Body>> for IrohChannel {
    type Response = Response<IrohResponseBody>;
    type Error = Error;
    type Future = BoxFuture<Result<Self::Response, Self::Error>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.sender.poll_ready(cx).map_err(Error::H2)
    }

    fn call(&mut self, request: Request<Body>) -> Self::Future {
        let mut sender = self.sender.clone();
        let origin = self.origin.clone();

        Box::pin(async move {
            // --- 1. Inject origin (scheme + authority) ---
            let (mut head, body) = request.into_parts();
            head.uri = apply_origin(head.uri, &origin);

            // --- 2. Determine if body is empty ---
            let body_is_empty = body.is_end_stream();

            // --- 3. Send request headers ---
            let request = Request::from_parts(head, ());
            let (response_future, mut send_stream) = sender
                .send_request(request, body_is_empty)
                .map_err(Error::H2)?;

            // --- 4. Pump request body (if any) ---
            if !body_is_empty {
                if let Err(e) = pump_body(body, &mut send_stream).await {
                    // After headers are sent we cannot return the error from
                    // call() directly — reset the stream so the peer sees a
                    // failure instead of a silent hang.
                    send_stream.send_reset(h2::Reason::INTERNAL_ERROR);
                    return Err(e);
                }
            }

            // --- 5. Await response ---
            let response = response_future.await.map_err(Error::H2)?;
            let (parts, recv_stream) = response.into_parts();

            Ok(Response::from_parts(
                parts,
                IrohResponseBody::new(recv_stream),
            ))
        })
    }
}

/// Always rewrite scheme + authority from `origin`, matching tonic's
/// `AddOrigin` middleware exactly (`add_origin.rs:53-58`).
fn apply_origin(uri: Uri, origin: &Uri) -> Uri {
    let mut parts: http::uri::Parts = uri.into();
    parts.scheme = origin
        .scheme()
        .cloned()
        .or_else(|| Some(Scheme::HTTP));
    parts.authority = origin.authority().cloned().or_else(|| {
        Authority::try_from("iroh.local").ok()
    });
    Uri::from_parts(parts).expect("valid uri after origin injection")
}

/// Pump frames from a tonic `Body` into an h2 `SendStream`.
///
/// **V1 limitation**: data is sent without explicit capacity reservation.
/// A hard limit of [`MAX_OUTBOUND_BODY_BYTES`] prevents unbounded buffering.
async fn pump_body(
    body: Body,
    send_stream: &mut h2::SendStream<Bytes>,
) -> Result<(), Error> {
    let mut body = Box::pin(body);
    let mut total_sent = 0usize;

    loop {
        match std::future::poll_fn(|cx| Pin::as_mut(&mut body).poll_frame(cx)).await {
            Some(Ok(frame)) => {
                if frame.is_data() {
                    let data = frame.into_data().expect("checked is_data");
                    total_sent += data.len();
                    if total_sent > MAX_OUTBOUND_BODY_BYTES {
                        return Err(Error::Connection(format!(
                            "outbound body exceeded {MAX_OUTBOUND_BODY_BYTES} byte limit"
                        )));
                    }
                    // TODO: use reserve_capacity/poll_capacity for proper
                    // outbound flow control instead of unbounded send_data.
                    send_stream.send_data(data, false).map_err(Error::H2)?;
                } else if frame.is_trailers() {
                    let trailers = frame.into_trailers().expect("checked is_trailers");
                    send_stream
                        .send_trailers(trailers)
                        .map_err(Error::H2)?;
                    return Ok(());
                }
            }
            Some(Err(e)) => {
                return Err(Error::Connection(format!("request body error: {e}")));
            }
            None => {
                // Body exhausted — signal end-of-stream.
                send_stream
                    .send_data(Bytes::new(), true)
                    .map_err(Error::H2)?;
                return Ok(());
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Response body
// ---------------------------------------------------------------------------

/// HTTP response body wrapping an h2 [`RecvStream`](h2::RecvStream).
///
/// Implements [`http_body::Body`] so tonic can decode gRPC frames from it.
pub struct IrohResponseBody {
    state: BodyState,
}

enum BodyState {
    /// Receiving DATA frames.
    Data(h2::RecvStream),
    /// DATA exhausted, waiting for optional TRAILERS.
    Trailers(h2::RecvStream),
    /// Terminal state.
    Done,
}

impl Default for IrohResponseBody {
    /// Returns an empty body (required by tonic's `with_interceptor` path).
    fn default() -> Self {
        Self {
            state: BodyState::Done,
        }
    }
}

impl IrohResponseBody {
    fn new(recv: h2::RecvStream) -> Self {
        Self {
            state: BodyState::Data(recv),
        }
    }
}

impl HttpBody for IrohResponseBody {
    type Data = Bytes;
    type Error = tonic::Status;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<http_body::Frame<Self::Data>, Self::Error>>> {
        let this = self.get_mut();
        loop {
            match &mut this.state {
                BodyState::Data(recv) => {
                    match recv.poll_data(cx) {
                        Poll::Ready(Some(Ok(data))) => {
                            // Release flow-control capacity so the sender can
                            // continue. Without this the connection stalls
                            // after the initial window is consumed.
                            let len = data.len();
                            if let Err(e) = recv.flow_control().release_capacity(len) {
                                this.state = BodyState::Done;
                                return Poll::Ready(Some(Err(tonic::Status::internal(
                                    format!("flow control error: {e}"),
                                ))));
                            }
                            return Poll::Ready(Some(Ok(http_body::Frame::data(data))));
                        }
                        Poll::Ready(Some(Err(e))) => {
                            this.state = BodyState::Done;
                            return Poll::Ready(Some(Err(tonic::Status::internal(
                                format!("h2 data error: {e}"),
                            ))));
                        }
                        Poll::Ready(None) => {
                            // DATA exhausted — transition to trailers.
                            // We need to take ownership, so swap with Done temporarily.
                            let recv = std::mem::replace(&mut this.state, BodyState::Done);
                            if let BodyState::Data(r) = recv {
                                this.state = BodyState::Trailers(r);
                            }
                        }
                        Poll::Pending => return Poll::Pending,
                    }
                }
                BodyState::Trailers(recv) => {
                    match recv.poll_trailers(cx) {
                        Poll::Ready(Ok(Some(trailers))) => {
                            this.state = BodyState::Done;
                            return Poll::Ready(Some(Ok(http_body::Frame::trailers(trailers))));
                        }
                        Poll::Ready(Ok(None)) => {
                            this.state = BodyState::Done;
                            return Poll::Ready(None);
                        }
                        Poll::Ready(Err(e)) => {
                            this.state = BodyState::Done;
                            return Poll::Ready(Some(Err(tonic::Status::internal(
                                format!("h2 trailers error: {e}"),
                            ))));
                        }
                        Poll::Pending => return Poll::Pending,
                    }
                }
                BodyState::Done => return Poll::Ready(None),
            }
        }
    }
}

impl std::fmt::Debug for IrohChannel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IrohChannel")
            .field("sender", &"h2::SendRequest<..>")
            .field("origin", &self.origin)
            .finish()
    }
}

impl std::fmt::Debug for IrohResponseBody {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IrohResponseBody").finish()
    }
}

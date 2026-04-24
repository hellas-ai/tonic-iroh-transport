//! gRPC interoperability tests.
//!
//! These tests validate that [`IrohChannel`] (h2 client) correctly speaks
//! HTTP/2 gRPC to a native tonic server (hyper/h2 server) over iroh streams.

use std::net::SocketAddr;
use std::time::Duration;

use tokio::time::timeout;
use tokio_stream::StreamExt;
use tonic::{Request, Response, Status};

use tonic_iroh_transport::iroh::{
    address_lookup::memory::MemoryLookup, endpoint::presets, Endpoint, EndpointAddr, TransportAddr,
};
use tonic_iroh_transport::{IrohConnect, TransportBuilder};

// Generated proto code (checked in at tests/pb/).
#[path = "pb/interop.v1.rs"]
#[allow(clippy::all, clippy::pedantic, missing_docs)]
mod pb;

use pb::interop_service_client::InteropServiceClient;
use pb::interop_service_server::{InteropService, InteropServiceServer};
use pb::Msg;

// ---------------------------------------------------------------------------
// Test service implementation
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct TestInteropService;

#[tonic::async_trait]
impl InteropService for TestInteropService {
    async fn unary(&self, request: Request<Msg>) -> Result<Response<Msg>, Status> {
        let msg = request.into_inner();
        Ok(Response::new(Msg {
            value: format!("echo:{}", msg.value),
        }))
    }

    type ServerStreamStream = tokio_stream::wrappers::ReceiverStream<Result<Msg, Status>>;

    async fn server_stream(
        &self,
        request: Request<Msg>,
    ) -> Result<Response<Self::ServerStreamStream>, Status> {
        let count: usize = request
            .into_inner()
            .value
            .parse()
            .map_err(|_| Status::invalid_argument("expected a number"))?;

        let (tx, rx) = tokio::sync::mpsc::channel(count.max(1));
        tokio::spawn(async move {
            for i in 0..count {
                let _ = tx
                    .send(Ok(Msg {
                        value: format!("item-{i}"),
                    }))
                    .await;
            }
        });
        Ok(Response::new(tokio_stream::wrappers::ReceiverStream::new(
            rx,
        )))
    }

    async fn client_stream(
        &self,
        request: Request<tonic::Streaming<Msg>>,
    ) -> Result<Response<Msg>, Status> {
        let mut stream = request.into_inner();
        let mut values = Vec::new();
        while let Some(msg) = stream.next().await {
            values.push(msg?.value);
        }
        Ok(Response::new(Msg {
            value: values.join(","),
        }))
    }

    type BidiStreamStream = tokio_stream::wrappers::ReceiverStream<Result<Msg, Status>>;

    async fn bidi_stream(
        &self,
        request: Request<tonic::Streaming<Msg>>,
    ) -> Result<Response<Self::BidiStreamStream>, Status> {
        let mut stream = request.into_inner();
        let (tx, rx) = tokio::sync::mpsc::channel(64);
        tokio::spawn(async move {
            while let Some(Ok(msg)) = stream.next().await {
                let reply = Msg {
                    value: format!("bidi:{}", msg.value),
                };
                if tx.send(Ok(reply)).await.is_err() {
                    break;
                }
            }
        });
        Ok(Response::new(tokio_stream::wrappers::ReceiverStream::new(
            rx,
        )))
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn to_localhost_addrs(addrs: Vec<SocketAddr>) -> impl Iterator<Item = TransportAddr> {
    addrs.into_iter().map(|addr| {
        let local_addr = match addr {
            SocketAddr::V4(v4) if v4.ip().is_unspecified() => SocketAddr::new(
                std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
                v4.port(),
            ),
            SocketAddr::V6(v6) if v6.ip().is_unspecified() => SocketAddr::new(
                std::net::IpAddr::V6(std::net::Ipv6Addr::LOCALHOST),
                v6.port(),
            ),
            other => other,
        };
        TransportAddr::Ip(local_addr)
    })
}

async fn local_endpoint() -> Endpoint {
    Endpoint::builder(presets::Minimal).bind().await.unwrap()
}

async fn local_endpoint_with_lookup(address_lookup: MemoryLookup) -> Endpoint {
    Endpoint::builder(presets::Minimal)
        .address_lookup(address_lookup)
        .bind()
        .await
        .unwrap()
}

fn endpoint_addr(endpoint: &Endpoint) -> EndpointAddr {
    EndpointAddr::new(endpoint.id()).with_addrs(to_localhost_addrs(endpoint.bound_sockets()))
}

/// Set up a server and connect a client, returning the typed gRPC client.
/// The returned `Endpoint` must be kept alive for the connection to persist.
async fn setup() -> (
    InteropServiceClient<tonic_iroh_transport::IrohChannel>,
    tonic_iroh_transport::TransportGuard,
    Endpoint,
) {
    let address_lookup = MemoryLookup::new();
    let server_ep = local_endpoint().await;
    let client_ep = local_endpoint_with_lookup(address_lookup.clone()).await;

    let guard = TransportBuilder::new(server_ep.clone())
        .add_rpc(InteropServiceServer::new(TestInteropService))
        .spawn()
        .await
        .expect("server should start");

    address_lookup.add_endpoint_info(endpoint_addr(&server_ep));

    let channel = timeout(
        Duration::from_secs(5),
        InteropServiceServer::<TestInteropService>::connect(&client_ep, endpoint_addr(&server_ep)),
    )
    .await
    .expect("connect timed out")
    .expect("connect failed");

    let client = InteropServiceClient::new(channel);
    (client, guard, client_ep)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn unary_echo() {
    let (mut client, _guard, _ep) = setup().await;

    let resp = client
        .unary(Request::new(Msg {
            value: "hello".into(),
        }))
        .await
        .expect("unary should succeed");

    assert_eq!(resp.into_inner().value, "echo:hello");
}

#[tokio::test]
async fn server_streaming() {
    let (mut client, _guard, _ep) = setup().await;

    let resp = client
        .server_stream(Request::new(Msg { value: "5".into() }))
        .await
        .expect("server_stream should succeed");

    let items: Vec<String> = resp
        .into_inner()
        .map(|r| r.expect("stream item should be ok").value)
        .collect()
        .await;

    assert_eq!(
        items,
        vec!["item-0", "item-1", "item-2", "item-3", "item-4"]
    );
}

#[tokio::test]
async fn client_streaming() {
    let (mut client, _guard, _ep) = setup().await;

    let input = tokio_stream::iter(vec![
        Msg { value: "a".into() },
        Msg { value: "b".into() },
        Msg { value: "c".into() },
    ]);

    let resp = client
        .client_stream(Request::new(input))
        .await
        .expect("client_stream should succeed");

    assert_eq!(resp.into_inner().value, "a,b,c");
}

#[tokio::test]
async fn bidi_streaming() {
    let (mut client, _guard, _ep) = setup().await;

    let input = tokio_stream::iter(vec![Msg { value: "x".into() }, Msg { value: "y".into() }]);

    let resp = client
        .bidi_stream(Request::new(input))
        .await
        .expect("bidi_stream should succeed");

    let items: Vec<String> = resp
        .into_inner()
        .map(|r| r.expect("bidi item should be ok").value)
        .collect()
        .await;

    assert_eq!(items, vec!["bidi:x", "bidi:y"]);
}

#[tokio::test]
async fn error_propagation() {
    let (mut client, _guard, _ep) = setup().await;

    // Send a non-numeric value to server_stream which expects a number.
    let result = client
        .server_stream(Request::new(Msg {
            value: "not-a-number".into(),
        }))
        .await;

    let status = result.expect_err("should return an error");
    assert_eq!(status.code(), tonic::Code::InvalidArgument);
}

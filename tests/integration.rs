use std::net::SocketAddr;
use std::time::Duration;
use tokio::time::timeout;
use tonic_iroh_transport::iroh::{
    endpoint::{Connection, RelayMode},
    protocol::{AcceptError, ProtocolHandler, Router},
    Endpoint, EndpointAddr, EndpointId, TransportAddr,
};
use tonic_iroh_transport::{GrpcProtocolHandler, IrohConnect, IrohStream};

/// Create a local-only endpoint with relays and discovery disabled for testing.
async fn local_endpoint() -> Endpoint {
    Endpoint::builder()
        .relay_mode(RelayMode::Disabled)
        .clear_discovery()
        .bind()
        .await
        .unwrap()
}

/// Convert bound socket addresses to localhost addresses for local testing.
/// `0.0.0.0:port` -> `127.0.0.1:port`, `[::]:port` -> `[::1]:port`
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

struct TestService;
impl tonic::server::NamedService for TestService {
    const NAME: &'static str = "test.Service";
}

struct AltService;
impl tonic::server::NamedService for AltService {
    const NAME: &'static str = "alt.Service";
}

#[tokio::test]
async fn test_basic_iroh_connection() {
    let endpoint1 = local_endpoint().await;
    let endpoint2 = local_endpoint().await;

    let handler = TestProtocolHandler;
    let addrs2 = endpoint2.bound_sockets();
    let router2 = Router::builder(endpoint2.clone())
        .accept(b"/test/1.0", handler)
        .spawn();

    let node_addr2 = EndpointAddr::new(endpoint2.id()).with_addrs(to_localhost_addrs(addrs2));
    let conn = timeout(
        Duration::from_secs(5),
        endpoint1.connect(node_addr2, b"/test/1.0"),
    )
    .await
    .expect("connection timed out")
    .expect("connection failed");

    let (send, recv) = conn.open_bi().await.expect("open_bi failed");
    let context = tonic_iroh_transport::stream::IrohContext {
        node_id: endpoint2.id(),
        connection: conn.clone(),
        established_at: std::time::Instant::now(),
        alpn: conn.alpn().to_vec(),
    };
    let _stream = IrohStream::new(send, recv, context);

    router2.shutdown().await.unwrap();
}

#[tokio::test]
async fn test_grpc_protocol_handler() {
    let (_handler, _incoming) = GrpcProtocolHandler::with_service_name("test.MockService");
    let (_handler, _incoming) = GrpcProtocolHandler::new::<TestService>();
    let (_handler, _incoming, alpn) = GrpcProtocolHandler::for_service::<TestService>();
    assert_eq!(alpn, b"/test.Service/1.0");
}

#[tokio::test]
async fn test_iroh_connect_trait() {
    // Test that the IrohConnect trait is implemented for NamedService types
    let endpoint = local_endpoint().await;
    let addr = EndpointAddr::new(endpoint.id());
    // Just verify the builder can be created (don't actually connect)
    let _builder = TestService::connect(&endpoint, addr);
}

#[test_log::test(tokio::test)]
async fn test_typed_client_connections() {
    let server = local_endpoint().await;
    let (handler, _incoming, alpn) = GrpcProtocolHandler::for_service::<TestService>();

    let _router = Router::builder(server.clone())
        .accept(alpn.clone(), handler)
        .spawn();

    let client_endpoint = local_endpoint().await;

    let addr =
        EndpointAddr::new(server.id()).with_addrs(to_localhost_addrs(server.bound_sockets()));

    let result = timeout(
        Duration::from_secs(2),
        TestService::connect(&client_endpoint, addr),
    )
    .await;

    assert!(result.is_ok() && result.unwrap().is_ok());
    assert_eq!(alpn, b"/test.Service/1.0");
}

#[test_log::test(tokio::test)]
async fn test_concurrent_connections() {
    let server = local_endpoint().await;
    let (handler, _incoming, alpn) = GrpcProtocolHandler::for_service::<TestService>();

    let _router = Router::builder(server.clone())
        .accept(alpn, handler)
        .spawn();

    let addr =
        EndpointAddr::new(server.id()).with_addrs(to_localhost_addrs(server.bound_sockets()));

    let tasks: Vec<_> = (0..5)
        .map(|_| {
            let addr = addr.clone();
            tokio::spawn(async move {
                let endpoint = local_endpoint().await;
                timeout(
                    Duration::from_secs(10),
                    TestService::connect(&endpoint, addr),
                )
                .await
                .unwrap()
                .unwrap();
            })
        })
        .collect();

    for task in tasks {
        task.await.unwrap();
    }
}

#[test_log::test(tokio::test)]
async fn test_multiple_services() {
    let server = local_endpoint().await;

    let (handler1, _incoming1, alpn1) = GrpcProtocolHandler::for_service::<TestService>();
    let (handler2, _incoming2, alpn2) = GrpcProtocolHandler::for_service::<AltService>();

    let _router = Router::builder(server.clone())
        .accept(alpn1.clone(), handler1)
        .accept(alpn2.clone(), handler2)
        .spawn();

    let client_endpoint = local_endpoint().await;

    let addr =
        EndpointAddr::new(server.id()).with_addrs(to_localhost_addrs(server.bound_sockets()));

    let channel1 = timeout(
        Duration::from_secs(2),
        TestService::connect(&client_endpoint, addr.clone()),
    )
    .await
    .unwrap()
    .unwrap();

    let channel2 = timeout(
        Duration::from_secs(2),
        AltService::connect(&client_endpoint, addr),
    )
    .await
    .unwrap()
    .unwrap();

    assert_eq!(alpn1, b"/test.Service/1.0");
    assert_eq!(alpn2, b"/alt.Service/1.0");

    drop(channel1);
    drop(channel2);
}

#[test_log::test(tokio::test)]
async fn test_connection_timeout_external() {
    // Test using external tokio::time::timeout
    let client_endpoint = local_endpoint().await;

    let fake_node_id = EndpointId::from_bytes(&[0u8; 32]).unwrap();
    let addr = EndpointAddr::new(fake_node_id);

    let result = timeout(
        Duration::from_millis(500),
        TestService::connect(&client_endpoint, addr),
    )
    .await;

    assert!(result.is_err() || result.unwrap().is_err());
}

#[test_log::test(tokio::test)]
async fn test_connect_timeout_builtin() {
    // Test using the built-in connect_timeout on ConnectBuilder
    let client_endpoint = local_endpoint().await;

    let fake_node_id = EndpointId::from_bytes(&[0u8; 32]).unwrap();
    let addr = EndpointAddr::new(fake_node_id);

    let result = TestService::connect(&client_endpoint, addr)
        .connect_timeout(Duration::from_millis(100))
        .await;

    // Should fail - either with timeout or connection error
    assert!(result.is_err());
}

#[derive(Clone, Debug)]
struct TestProtocolHandler;

impl ProtocolHandler for TestProtocolHandler {
    fn accept(
        &self,
        _connection: Connection,
    ) -> impl futures_util::Future<Output = Result<(), AcceptError>> + std::marker::Send {
        async move { Ok::<(), AcceptError>(()) }
    }
}

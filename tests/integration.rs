use std::time::Duration;
use tokio::time::timeout;
use tonic_iroh_transport::{GrpcProtocolHandler, IrohClient, IrohStream};

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
    let endpoint1 = iroh::Endpoint::builder().bind().await.unwrap();
    let endpoint2 = iroh::Endpoint::builder().bind().await.unwrap();

    let handler = TestProtocolHandler;
    let addrs2 = endpoint2.bound_sockets();
    let router2 = iroh::protocol::Router::builder(endpoint2.clone())
        .accept(b"/test/1.0", handler)
        .spawn();

    let node_addr2 = iroh::NodeAddr::new(endpoint2.node_id()).with_direct_addresses(addrs2);
    let conn = timeout(
        Duration::from_secs(5),
        endpoint1.connect(node_addr2, b"/test/1.0"),
    )
    .await
    .expect("connection timed out")
    .expect("connection failed");

    let (send, recv) = conn.open_bi().await.expect("open_bi failed");
    let context = tonic_iroh_transport::stream::IrohContext {
        node_id: endpoint2.node_id(),
        connection: conn.clone(),
        established_at: std::time::Instant::now(),
        alpn: conn.alpn().unwrap_or_default(),
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
async fn test_iroh_client() {
    let endpoint = iroh::Endpoint::builder().bind().await.unwrap();
    let client = IrohClient::new(endpoint.clone());
    assert_eq!(client.endpoint().node_id(), endpoint.node_id());
}

#[test_log::test(tokio::test)]
async fn test_typed_client_connections() {
    let server = iroh::Endpoint::builder().bind().await.unwrap();
    let (handler, _incoming, alpn) = GrpcProtocolHandler::for_service::<TestService>();

    let _router = iroh::protocol::Router::builder(server.clone())
        .accept(alpn.clone(), handler)
        .spawn();

    let client_endpoint = iroh::Endpoint::builder().bind().await.unwrap();
    let client = IrohClient::new(client_endpoint);

    let addr = iroh::NodeAddr::new(server.node_id()).with_direct_addresses(server.bound_sockets());

    let result = timeout(
        Duration::from_secs(2),
        client.connect_to_service::<TestService>(addr),
    )
    .await;

    assert!(result.is_ok() && result.unwrap().is_ok());
    assert_eq!(alpn, b"/test.Service/1.0");
}

#[test_log::test(tokio::test)]
async fn test_concurrent_connections() {
    let server = iroh::Endpoint::builder().bind().await.unwrap();
    let (handler, _incoming, alpn) = GrpcProtocolHandler::for_service::<TestService>();

    let _router = iroh::protocol::Router::builder(server.clone())
        .accept(alpn, handler)
        .spawn();

    let addr = iroh::NodeAddr::new(server.node_id()).with_direct_addresses(server.bound_sockets());

    let tasks: Vec<_> = (0..5)
        .map(|_| {
            let addr = addr.clone();
            tokio::spawn(async move {
                let endpoint = iroh::Endpoint::builder().bind().await.unwrap();
                let client = IrohClient::new(endpoint);
                timeout(
                    Duration::from_secs(2),
                    client.connect_to_service::<TestService>(addr),
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
    let server = iroh::Endpoint::builder().bind().await.unwrap();

    let (handler1, _incoming1, alpn1) = GrpcProtocolHandler::for_service::<TestService>();
    let (handler2, _incoming2, alpn2) = GrpcProtocolHandler::for_service::<AltService>();

    let _router = iroh::protocol::Router::builder(server.clone())
        .accept(alpn1.clone(), handler1)
        .accept(alpn2.clone(), handler2)
        .spawn();

    let client_endpoint = iroh::Endpoint::builder().bind().await.unwrap();
    let client = IrohClient::new(client_endpoint);

    let addr = iroh::NodeAddr::new(server.node_id()).with_direct_addresses(server.bound_sockets());

    let channel1 = timeout(
        Duration::from_secs(2),
        client.connect_to_service::<TestService>(addr.clone()),
    )
    .await
    .unwrap()
    .unwrap();

    let channel2 = timeout(
        Duration::from_secs(2),
        client.connect_to_service::<AltService>(addr),
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
async fn test_connection_timeout() {
    let client_endpoint = iroh::Endpoint::builder().bind().await.unwrap();
    let client = IrohClient::new(client_endpoint);

    let fake_node_id = iroh::NodeId::from_bytes(&[0u8; 32]).unwrap();
    let addr = iroh::NodeAddr::new(fake_node_id);

    let result = timeout(
        Duration::from_millis(500),
        client.connect_to_service::<TestService>(addr),
    )
    .await;

    assert!(result.is_err() || result.unwrap().is_err());
}

#[derive(Clone, Debug)]
struct TestProtocolHandler;

impl iroh::protocol::ProtocolHandler for TestProtocolHandler {
    fn accept(
        &self,
        _connection: iroh::endpoint::Connection,
    ) -> impl futures_util::Future<Output = Result<(), iroh::protocol::AcceptError>> + std::marker::Send
    {
        async move { Ok::<(), iroh::protocol::AcceptError>(()) }
    }
}

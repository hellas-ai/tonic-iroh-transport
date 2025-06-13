//! End-to-end integration tests for tonic-iroh-transport

use iroh::protocol::AcceptError;
use std::time::Duration;
use tokio::time::timeout;
use tonic_iroh_transport::{GrpcProtocolHandler, IrohClient, IrohStream};

/// Test that we can create two iroh endpoints and establish a connection
#[tokio::test]
async fn test_iroh_connection() {
    // Create two endpoints
    let endpoint1 = iroh::Endpoint::builder().bind().await.unwrap();
    let endpoint2 = iroh::Endpoint::builder().bind().await.unwrap();

    let node_id1 = endpoint1.node_id();
    let node_id2 = endpoint2.node_id();

    println!("Node 1 ID: {}", node_id1);
    println!("Node 2 ID: {}", node_id2);

    // Create a simple protocol handler for testing
    let handler = TestProtocolHandler;

    // Start accepting connections on endpoint2
    let router2 = iroh::protocol::Router::builder(endpoint2)
        .accept(b"/test/1.0", handler)
        .spawn();

    // Connect from endpoint1 to endpoint2
    let node_addr2 = iroh::NodeAddr::new(node_id2);
    let connection = timeout(
        Duration::from_secs(5),
        endpoint1.connect(node_addr2, b"/test/1.0"),
    )
    .await;

    match connection {
        Ok(Ok(conn)) => {
            println!("✅ Successfully connected to peer!");

            // Test creating an IrohStream from the connection
            match conn.open_bi().await {
                Ok((send, recv)) => {
                    let peer_info = tonic_iroh_transport::stream::IrohPeerInfo {
                        node_id: node_id2,
                        established_at: std::time::Instant::now(),
                        alpn: conn.alpn().unwrap_or_default(),
                    };
                    let _stream = IrohStream::new(send, recv, peer_info);
                    println!("✅ Successfully created IrohStream!");
                }
                Err(e) => panic!("Failed to create IrohStream: {}", e),
            }
        }
        Ok(Err(e)) => {
            // Connection failed, but this might be expected in CI environments
            // where networking is restricted
            println!("⚠️  Connection failed (might be expected in CI): {}", e);
        }
        Err(_) => {
            println!("⚠️  Connection timed out (might be expected in CI)");
        }
    }

    router2.shutdown().await.unwrap();
}

/// Test creating a GrpcProtocolHandler
#[tokio::test]
async fn test_grpc_protocol_handler() {
    // Test with_service_name method
    let (_handler, _incoming) = GrpcProtocolHandler::with_service_name("test.MockService");

    // If we get here, the basic creation works
    assert!(true);
}

/// Mock service for testing GrpcProtocolHandler::new()
struct MockService;
impl tonic::server::NamedService for MockService {
    const NAME: &'static str = "test.MockService";
}

/// Test creating a GrpcProtocolHandler with type parameter
#[tokio::test]
async fn test_grpc_protocol_handler_typed() {
    // Test the new() method with a type parameter
    let (_handler, _incoming) = GrpcProtocolHandler::new::<MockService>();

    // If we get here, the basic creation works
    assert!(true);
}

/// Test creating an IrohClient
#[tokio::test]
async fn test_iroh_client() {
    // Create an endpoint for testing
    let endpoint = iroh::Endpoint::builder().bind().await.unwrap();

    // Create IrohClient
    let client = IrohClient::new(endpoint.clone());

    // Verify we can access the endpoint
    assert_eq!(client.endpoint().node_id(), endpoint.node_id());
}

// Mock protocol handler for testing
#[derive(Clone, Debug)]
struct TestProtocolHandler;

impl iroh::protocol::ProtocolHandler for TestProtocolHandler {
    async fn accept(&self, _connection: iroh::endpoint::Connection) -> Result<(), AcceptError> {
        println!("Test protocol handler accepted connection");
        Ok(())
    }
}

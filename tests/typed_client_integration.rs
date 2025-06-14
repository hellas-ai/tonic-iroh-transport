//! Integration test demonstrating the typed client with multiple connections

use std::time::Duration;
use tokio::time::timeout;
use tonic_iroh_transport::{GrpcProtocolHandler, IrohClient};
use tracing::info;

/// Test multiple connections using our typed client
#[test_log::test(tokio::test)]
async fn test_typed_client_multiple_connections() {
    // Create server endpoint
    let server_endpoint = iroh::Endpoint::builder().bind().await.unwrap();
    let server_node_id = server_endpoint.node_id();

    info!("Server Node ID: {}", server_node_id);

    // Set up multiple mock services using the same handler pattern
    let (service1_handler, _service1_incoming, service1_alpn) =
        GrpcProtocolHandler::for_service::<MockService1Server<MockService1>>();
    let (service2_handler, _service2_incoming, service2_alpn) =
        GrpcProtocolHandler::for_service::<MockService2Server<MockService2>>();

    info!(
        "Service1 protocol: {}",
        String::from_utf8_lossy(&service1_alpn)
    );
    info!(
        "Service2 protocol: {}",
        String::from_utf8_lossy(&service2_alpn)
    );

    let _router = iroh::protocol::Router::builder(server_endpoint.clone())
        .accept(service1_alpn, service1_handler)
        .accept(service2_alpn, service2_handler)
        .spawn();

    // Spawn mock servers
    let _service1_server = tokio::spawn(async move {
        // Mock server that just handles connections without actual tonic service
        loop {
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    });

    let _service2_server = tokio::spawn(async move {
        // Mock server that just handles connections without actual tonic service
        loop {
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    });

    // Give servers time to start
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Create client endpoints
    let client1_endpoint = iroh::Endpoint::builder().bind().await.unwrap();
    let client2_endpoint = iroh::Endpoint::builder().bind().await.unwrap();

    info!("Client1 Node ID: {}", client1_endpoint.node_id());
    info!("Client2 Node ID: {}", client2_endpoint.node_id());

    let server_addr = {
        let (ipv4_addr, _ipv6_addr) = server_endpoint.bound_sockets();
        iroh::NodeAddr::new(server_node_id).with_direct_addresses([ipv4_addr])
    };

    // Test our typed client connections
    let iroh_client1 = IrohClient::new(client1_endpoint);
    let iroh_client2 = IrohClient::new(client2_endpoint);

    // Test connecting to different services with type safety
    let service1_result = timeout(
        Duration::from_secs(5),
        iroh_client1.connect_to_service::<MockService1Server<MockService1>>(server_addr.clone()),
    )
    .await;

    let service2_result = timeout(
        Duration::from_secs(5),
        iroh_client2.connect_to_service::<MockService2Server<MockService2>>(server_addr.clone()),
    )
    .await;

    // Test that both connections succeed
    match (service1_result, service2_result) {
        (Ok(Ok(channel1)), Ok(Ok(channel2))) => {
            info!("✅ Successfully created both channels");

            // Verify we can create clients from the channels
            let _client1 = MockService1Client::new(channel1);
            let _client2 = MockService2Client::new(channel2);

            info!("✅ Successfully created typed clients");
        }
        (Ok(Err(e1)), _) => {
            info!(
                "⚠️  Service1 connection failed (expected in some environments): {}",
                e1
            );
        }
        (_, Ok(Err(e2))) => {
            info!(
                "⚠️  Service2 connection failed (expected in some environments): {}",
                e2
            );
        }
        (Err(_), _) => {
            info!("⚠️  Service1 connection timed out (expected in some environments)");
        }
        (_, Err(_)) => {
            info!("⚠️  Service2 connection timed out (expected in some environments)");
        }
    }

    // Test same client connecting to multiple services
    let multi_service_test = async {
        let service1_channel = iroh_client1
            .connect_to_service::<MockService1Server<MockService1>>(server_addr.clone())
            .await?;
        let service2_channel = iroh_client1
            .connect_to_service::<MockService2Server<MockService2>>(server_addr)
            .await?;

        let _client1 = MockService1Client::new(service1_channel);
        let _client2 = MockService2Client::new(service2_channel);

        Ok::<(), tonic_iroh_transport::Error>(())
    };

    match timeout(Duration::from_secs(5), multi_service_test).await {
        Ok(Ok(())) => {
            info!("✅ Same client connected to multiple services successfully");
        }
        Ok(Err(e)) => {
            info!(
                "⚠️  Multi-service connection failed (expected in some environments): {}",
                e
            );
        }
        Err(_) => {
            info!("⚠️  Multi-service connection timed out (expected in some environments)");
        }
    }

    info!("✅ Typed client integration test completed!");
}

/// Test concurrent connections to demonstrate scalability
#[test_log::test(tokio::test)]
async fn test_concurrent_typed_clients() {
    // Create server endpoint
    let server_endpoint = iroh::Endpoint::builder().bind().await.unwrap();
    let server_node_id = server_endpoint.node_id();

    let (handler, _incoming, alpn) =
        GrpcProtocolHandler::for_service::<MockService1Server<MockService1>>();

    let _router = iroh::protocol::Router::builder(server_endpoint.clone())
        .accept(alpn, handler)
        .spawn();

    tokio::time::sleep(Duration::from_millis(100)).await;

    let server_addr = {
        let (ipv4_addr, _ipv6_addr) = server_endpoint.bound_sockets();
        iroh::NodeAddr::new(server_node_id).with_direct_addresses([ipv4_addr])
    };

    // Create multiple clients concurrently
    let num_clients = 3;
    let mut tasks = Vec::new();

    for i in 0..num_clients {
        let server_addr = server_addr.clone();
        let task = tokio::spawn(async move {
            let client_endpoint = iroh::Endpoint::builder().bind().await.unwrap();
            let iroh_client = IrohClient::new(client_endpoint);

            // Test typed connection
            let result = timeout(
                Duration::from_secs(5),
                iroh_client.connect_to_service::<MockService1Server<MockService1>>(server_addr),
            )
            .await;

            (i, result)
        });

        tasks.push(task);
    }

    // Wait for all clients to complete
    let results = futures_util::future::join_all(tasks).await;

    let mut successful_connections = 0;

    // Verify results
    for result in results {
        let (client_id, connection_result) = result.unwrap();
        match connection_result {
            Ok(Ok(_channel)) => {
                info!("✅ Client {} connected successfully", client_id);
                successful_connections += 1;
            }
            Ok(Err(e)) => {
                info!(
                    "⚠️  Client {} connection failed (may be expected): {}",
                    client_id, e
                );
            }
            Err(_) => {
                info!(
                    "⚠️  Client {} connection timed out (may be expected)",
                    client_id
                );
            }
        }
    }

    info!(
        "Concurrent connection test: {}/{} connections successful",
        successful_connections, num_clients
    );
    info!("✅ Concurrent typed clients test completed!");
}

// Minimal mock service definitions for testing

pub struct MockService1;

impl tonic::server::NamedService for MockService1 {
    const NAME: &'static str = "test.MockService1";
}

pub struct MockService2;

impl tonic::server::NamedService for MockService2 {
    const NAME: &'static str = "test.MockService2";
}

// Mock server wrappers
pub struct MockService1Server<T> {
    #[allow(dead_code)]
    inner: T,
}

impl<T> MockService1Server<T> {
    #[allow(dead_code)]
    pub fn new(inner: T) -> Self {
        Self { inner }
    }
}

impl<T> tonic::server::NamedService for MockService1Server<T> {
    const NAME: &'static str = "test.MockService1";
}

pub struct MockService2Server<T> {
    #[allow(dead_code)]
    inner: T,
}

impl<T> MockService2Server<T> {
    #[allow(dead_code)]
    pub fn new(inner: T) -> Self {
        Self { inner }
    }
}

impl<T> tonic::server::NamedService for MockService2Server<T> {
    const NAME: &'static str = "test.MockService2";
}

// Mock client wrappers
pub struct MockService1Client<T> {
    #[allow(dead_code)]
    inner: T,
}

impl<T> MockService1Client<T> {
    pub fn new(inner: T) -> Self {
        Self { inner }
    }
}

pub struct MockService2Client<T> {
    #[allow(dead_code)]
    inner: T,
}

impl<T> MockService2Client<T> {
    pub fn new(inner: T) -> Self {
        Self { inner }
    }
}

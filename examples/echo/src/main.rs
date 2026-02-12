use std::time::Duration;

use anyhow::Result;
use pb::echo::v1::{
    echo_service_client::EchoServiceClient,
    echo_service_server::{EchoService, EchoServiceServer},
    EchoRequest, EchoResponse,
};
use tonic::{Request, Response, Status};
use tonic_iroh_transport::iroh;
use tonic_iroh_transport::swarm::{DhtBackend, DhtPublisherConfig, ServiceRegistry};
use tonic_iroh_transport::{IrohContext, TransportBuilder};
use tracing::info;

// Generated protobuf code
mod pb;

// Simple echo service implementation
#[derive(Clone)]
struct EchoServiceImpl;

#[tonic::async_trait]
impl EchoService for EchoServiceImpl {
    async fn echo(&self, request: Request<EchoRequest>) -> Result<Response<EchoResponse>, Status> {
        let context = request.extensions().get::<IrohContext>().cloned();
        let peer_id = context
            .map(|ctx| ctx.node_id.to_string())
            .unwrap_or_else(|| "unknown".to_string());

        let req = request.into_inner();
        info!("Echo request from {}: {}", peer_id, req.message);

        Ok(Response::new(EchoResponse {
            message: req.message,
            peer_id,
        }))
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();

    // --- Server ---
    let server_endpoint = iroh::Endpoint::builder().bind().await?;
    info!("Server Node ID: {}", server_endpoint.id());

    let dht = DhtBackend::new(&server_endpoint)?;
    let publisher = dht.create_publisher(DhtPublisherConfig {
        tags: vec![],
        publish_interval: Duration::from_secs(5),
    });

    let rpc_guard = TransportBuilder::new(server_endpoint.clone())
        .with_publisher(publisher)
        .add_rpc(EchoServiceServer::new(EchoServiceImpl))
        .spawn()
        .await?;

    info!("Server started, publishing to DHT...");

    // Wait for DHT publish to propagate
    tokio::time::sleep(Duration::from_secs(5)).await;

    // --- Client ---
    let client_endpoint = iroh::Endpoint::builder().bind().await?;
    info!("Client Node ID: {}", client_endpoint.id());

    let client_dht = DhtBackend::new(&client_endpoint)?.poll_interval(Duration::from_secs(5));

    let mut registry = ServiceRegistry::new(&client_endpoint);
    registry.add(client_dht);

    info!("Discovering echo service via DHT...");

    let channel = registry
        .find::<EchoServiceServer<EchoServiceImpl>>()
        .timeout(Duration::from_secs(60))
        .first()
        .await?;

    info!("Discovered and connected to echo service via DHT!");

    let mut client = EchoServiceClient::new(channel);

    let messages = vec![
        "Hello, World!",
        "This is a test message",
        "Echo demo working!",
    ];

    for message in messages {
        info!("Sending: '{}'", message);
        let request = Request::new(EchoRequest {
            message: message.to_string(),
        });

        let response = client.echo(request).await?;
        let resp = response.into_inner();

        info!(
            "Echo response: '{}' from peer: {}",
            resp.message, resp.peer_id
        );
    }

    info!("Echo demo completed successfully!");
    rpc_guard.shutdown().await?;

    Ok(())
}

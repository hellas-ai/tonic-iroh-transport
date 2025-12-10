use anyhow::Result;
use iroh::EndpointAddr;
use pb::echo::{
    echo_client::EchoClient,
    echo_server::{Echo, EchoServer},
    EchoRequest, EchoResponse,
};
use tonic::transport::Server;
use tonic::{Request, Response, Status};
use tonic_iroh_transport::{GrpcProtocolHandler, IrohClient, IrohContext};
use tracing::info;

// Generated protobuf code
mod pb;

// Simple echo service implementation
#[derive(Clone)]
struct EchoService;

#[tonic::async_trait]
impl Echo for EchoService {
    async fn echo(&self, request: Request<EchoRequest>) -> Result<Response<EchoResponse>, Status> {
        // Extract peer info from connection
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

    // Create server endpoint
    let server_endpoint = iroh::Endpoint::builder().bind().await?;
    let server_node_id = server_endpoint.id();

    info!("Server Node ID: {}", server_node_id);
    info!(
        "Server Local addresses: {:?}",
        server_endpoint.bound_sockets()
    );

    // Set up echo service
    let (handler, incoming, alpn) = GrpcProtocolHandler::for_service::<EchoServer<EchoService>>();

    info!(
        "Echo server started on protocol: {}",
        String::from_utf8_lossy(&alpn)
    );

    let _router = iroh::protocol::Router::builder(server_endpoint.clone())
        .accept(alpn, handler)
        .spawn();

    // Spawn server in background
    let server_handle = tokio::spawn(async move {
        let server = Server::builder()
            .add_service(EchoServer::new(EchoService))
            .serve_with_incoming(incoming);
        if let Err(e) = server.await {
            eprintln!("Server error: {e}");
        }
    });

    // Give the server a moment to start up
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    // Create client endpoint
    let client_endpoint = iroh::Endpoint::builder().bind().await?;
    info!("Client Node ID: {}", client_endpoint.id());

    // Connect to the server
    let server_addr = {
        let addrs = server_endpoint.bound_sockets();
        let mut addr = EndpointAddr::new(server_node_id);
        for a in addrs {
            addr = addr.with_ip_addr(a);
        }
        addr
    };

    info!("Connecting to server at: {:?}", server_addr);

    let iroh_client = IrohClient::new(client_endpoint);
    let channel = iroh_client
        .connect_to_service::<EchoServer<EchoService>>(server_addr)
        .await?;
    let mut client = EchoClient::new(channel);

    // Test a few echo calls
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
            "✅ Echo response: '{}' from peer: {}",
            resp.message, resp.peer_id
        );
    }

    info!("Echo demo completed successfully!");

    // Shutdown
    server_handle.abort();

    Ok(())
}

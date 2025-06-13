use anyhow::Result;
use tonic::{Request, Response, Status};
use tonic::transport::Server;
use tracing::info;

// Generated protobuf code
mod pb;

use iroh::{NodeId, NodeAddr};
use pb::echo::{echo_server::{Echo, EchoServer}, echo_client::EchoClient, EchoRequest, EchoResponse};
use tonic_iroh_transport::{connect_with_alpn, GrpcProtocolHandler, IrohPeerInfo, service_to_alpn};
use std::str::FromStr;

// Simple echo service implementation
#[derive(Clone)]
struct EchoService;

#[tonic::async_trait]
impl Echo for EchoService {
    async fn echo(&self, request: Request<EchoRequest>) -> Result<Response<EchoResponse>, Status> {
        // Extract peer info from connection
        let peer_info = request.extensions().get::<IrohPeerInfo>().cloned();
        let peer_id = peer_info
            .map(|info| info.node_id.to_string())
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

    // Create iroh endpoint
    let endpoint = iroh::Endpoint::builder().bind().await?;
    let node_id = endpoint.node_id();
    
    info!("Node ID: {}", node_id);
    info!("Local addresses: {:?}", endpoint.bound_sockets());

    // Set up echo service - the new ergonomic way!
    let (handler, incoming, alpn) = GrpcProtocolHandler::for_service::<EchoServer<EchoService>>();
    
    info!("Echo server started on protocol: {}", String::from_utf8_lossy(&alpn));
    
    let _router = iroh::protocol::Router::builder(endpoint.clone())
        .accept(alpn, handler)
        .spawn();

    // Spawn server in background
    tokio::spawn(async move {
        let server = Server::builder()
            .add_service(EchoServer::new(EchoService))
            .serve_with_incoming(incoming);
        if let Err(e) = server.await {
            eprintln!("Server error: {}", e);
        }
    });

    // Example: connect to yourself (for demo)
    let args: Vec<String> = std::env::args().collect();
    if args.len() > 1 && args[1] == "client" {
        // If run with "client" argument, act as client
        if args.len() < 3 {
            eprintln!("Usage: {} client <node_id> [address] [message]", args[0]);
            std::process::exit(1);
        }
        
        let target_node_id = NodeId::from_str(&args[2])?;
        let target_addr = if args.len() > 3 {
            NodeAddr::new(target_node_id).with_direct_addresses([args[3].parse()?])
        } else {
            NodeAddr::new(target_node_id)
        };
        let message = args.get(4).cloned().unwrap_or_else(|| "Hello, World!".to_string());

        info!("Connecting to: {:?}", target_addr);
        
        let alpn = service_to_alpn::<EchoServer<EchoService>>();
        let channel = connect_with_alpn(endpoint, target_addr, &alpn).await?;
        let mut client = EchoClient::new(channel);

        let request = Request::new(EchoRequest { message });
        let response = client.echo(request).await?;
        let resp = response.into_inner();
        
        info!("Echo response: '{}' from peer: {}", resp.message, resp.peer_id);
        return Ok(());
    }

    // Run server indefinitely
    tokio::signal::ctrl_c().await?;
    info!("Shutting down...");
    
    Ok(())
}
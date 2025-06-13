//! Simple client connector for tonic over iroh.

use crate::{stream::IrohStream, Result};
use http::Uri;
use hyper_util::rt::TokioIo;
use iroh_base::NodeAddr;
use tonic::transport::{Channel, Endpoint};
use tower::service_fn;
use tracing::{debug, info};


/// Connect to a remote iroh peer with a specific ALPN protocol.
pub async fn connect_with_alpn(endpoint: iroh::Endpoint, target: NodeAddr, alpn: &[u8]) -> Result<Channel> {
    debug!("Connecting to peer: {}", target.node_id);
    
    let target_id = target.node_id;
    let alpn = alpn.to_vec();
    
    // Create a dummy endpoint URI (not used by connector)
    let channel = Endpoint::try_from("http://[::]:50051")?
        .connect_with_connector(service_fn(move |_: Uri| {
            let endpoint = endpoint.clone();
            let target = target.clone();
            let alpn = alpn.clone();
            
            async move {
                info!("Establishing iroh connection to {}", target.node_id);
                
                // Connect to the peer using iroh
                let connection = endpoint.connect(target, &alpn).await
                    .map_err(|e| std::io::Error::new(std::io::ErrorKind::ConnectionRefused, e))?;
                
                // Open a bidirectional stream for this gRPC call
                let (send, recv) = connection.open_bi().await
                    .map_err(|e| std::io::Error::new(std::io::ErrorKind::BrokenPipe, e))?;
                
                // Create the stream with peer info
                let peer_info = crate::stream::IrohPeerInfo {
                    node_id: connection.remote_node_id()
                        .map_err(std::io::Error::other)?,
                    established_at: std::time::Instant::now(),
                    alpn,
                };
                
                let stream = IrohStream::new(send, recv, peer_info);
                Ok::<_, std::io::Error>(TokioIo::new(stream))
            }
        }))
        .await?;
    
    info!("Successfully connected to peer: {}", target_id);
    Ok(channel)
}

/// Type alias for a tonic Channel over iroh.
/// This makes the examples more readable.
pub type IrohChannel = Channel;
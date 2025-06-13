use anyhow::Result;
use clap::Parser;
use std::collections::HashMap;
use std::str::FromStr;
use std::sync::Arc;
use tokio::sync::{mpsc, RwLock};
use tokio_stream::wrappers::ReceiverStream;
use tokio_stream::StreamExt;
use tonic::{Request, Response, Status, Streaming};
use tonic::transport::Server;
use tracing::{info, warn, error};

mod pb;

use pb::p2p_chat::{
    node_service_client::NodeServiceClient,
    node_service_server::{NodeService, NodeServiceServer},
    p2p_chat_service_client::P2pChatServiceClient,
    p2p_chat_service_server::{P2pChatService, P2pChatServiceServer},
    *,
};

use tonic_iroh_transport::{GrpcProtocolHandler, IrohChannel, IrohClient, IrohPeerInfo};
use iroh::{NodeAddr, NodeId, SecretKey};

#[derive(Parser)]
#[clap(name = "p2p-chat")]
#[clap(about = "A P2P chat application using tonic over iroh")]
struct Args {
    /// Node secret key (hex encoded), generates random if not provided
    #[clap(long)]
    secret_key: Option<String>,
    
    /// Custom relay URLs to use
    #[clap(long)]
    relay_url: Vec<String>,
    
    /// Bind port for local connections (optional)
    #[clap(long, default_value = "0")]
    port: u16,
    
    /// Enable verbose logging
    #[clap(short, long)]
    verbose: bool,
    
    /// Disable relay servers (direct connections only)
    #[clap(long)]
    no_relay: bool,
    
    /// Connect to another peer by Node ID
    #[clap(long)]
    connect_to: Option<String>,
    
    /// Target node addresses for connection
    #[clap(long)]
    target_addresses: Vec<String>,
    
    /// Command to execute after connecting
    #[clap(subcommand)]
    command: Option<Command>,
}

#[derive(Parser)]
enum Command {
    /// Send a single message
    Send {
        /// Message content
        message: String,
    },
    /// Subscribe to incoming messages
    Subscribe,
    /// Get chat history
    History {
        /// Number of messages to retrieve
        #[clap(long, default_value = "10")]
        limit: i32,
    },
    /// Start an interactive chat session
    Chat,
    /// Ping the target node
    Ping {
        /// Optional data to include in ping
        #[clap(long)]
        data: Option<String>,
    },
    /// Get node information
    NodeInfo,
    /// List connected peers
    ListPeers,
    /// Benchmark message sending
    Benchmark {
        /// Duration in seconds to run benchmark
        #[clap(long, default_value = "5")]
        duration: u64,
        /// Message content to send repeatedly
        #[clap(long, default_value = "benchmark")]
        message: String,
        /// Maximum concurrent messages
        #[clap(long, default_value = "10")]
        concurrent: usize,
    },
}

#[derive(Clone)]
struct ChatState {
    messages: Arc<RwLock<Vec<Message>>>,
    subscribers: Arc<RwLock<HashMap<String, mpsc::Sender<Message>>>>,
    node_id: String,
}

impl std::fmt::Debug for ChatState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChatState")
            .field("node_id", &self.node_id)
            .field("messages", &"<RwLock<Vec<Message>>>")
            .field("subscribers", &"<RwLock<HashMap<String, mpsc::Sender<Message>>>>")
            .finish()
    }
}

impl ChatState {
    fn new(node_id: String) -> Self {
        Self {
            messages: Arc::new(RwLock::new(Vec::new())),
            subscribers: Arc::new(RwLock::new(HashMap::new())),
            node_id,
        }
    }

    async fn add_message(&self, message: Message) {
        self.messages.write().await.push(message.clone());
        
        let subscribers = self.subscribers.read().await;
        for (subscriber_id, sender) in subscribers.iter() {
            if let Err(_) = sender.send(message.clone()).await {
                warn!("Failed to send message to subscriber: {}", subscriber_id);
            }
        }
    }

    async fn add_subscriber(&self, id: String, sender: mpsc::Sender<Message>) {
        self.subscribers.write().await.insert(id, sender);
    }

    async fn remove_subscriber(&self, id: &str) {
        self.subscribers.write().await.remove(id);
    }
}

#[derive(Clone, Debug)]
struct ChatServiceImpl {
    state: ChatState,
}

impl ChatServiceImpl {
    fn new(state: ChatState) -> Self {
        Self { state }
    }
}

#[tonic::async_trait]
impl P2pChatService for ChatServiceImpl {
    async fn send_message(
        &self,
        request: Request<SendMessageRequest>,
    ) -> Result<Response<SendMessageResponse>, Status> {
        // Extract connection information from the request extensions
        let connect_info = request.extensions().get::<IrohPeerInfo>().cloned();
        
        let req = request.into_inner();
        
        // Log connection details if available
        if let Some(peer_info) = connect_info {
            info!("=== Connection Info ===");
            info!("Remote Node ID: {}", peer_info.node_id);
            info!("Connection established: {:?} ago", peer_info.established_at.elapsed());
            info!("ALPN protocol: {}", String::from_utf8_lossy(&peer_info.alpn));
            info!("======================");
        }
        
        let message = Message {
            id: uuid::new_v4().to_string(),
            sender_id: self.state.node_id.clone(),
            recipient_id: req.recipient_id.clone(),
            content: req.content,
            timestamp: req.timestamp,
            r#type: MessageType::Text as i32,
        };

        info!("Received message from {}: {}", message.sender_id, message.content);
        
        self.state.add_message(message.clone()).await;

        Ok(Response::new(SendMessageResponse {
            success: true,
            error: String::new(),
            message_id: message.id,
        }))
    }

    type SubscribeMessagesStream = ReceiverStream<Result<Message, Status>>;

    async fn subscribe_messages(
        &self,
        request: Request<SubscribeRequest>,
    ) -> Result<Response<Self::SubscribeMessagesStream>, Status> {
        // Extract connection information for subscriber
        let connect_info = request.extensions().get::<IrohPeerInfo>().cloned();
        
        if let Some(peer_info) = connect_info {
            info!("=== New Subscriber ===");
            info!("Subscriber Node ID: {}", peer_info.node_id);
            info!("Connection age: {:?}", peer_info.established_at.elapsed());
            info!("ALPN protocol: {}", String::from_utf8_lossy(&peer_info.alpn));
            info!("======================");
        }
        
        let subscriber_id = uuid::new_v4().to_string();
        let subscriber_id_clone = subscriber_id.clone();
        let (tx, rx) = mpsc::channel(100);
        
        let (result_tx, result_rx) = mpsc::channel(100);
        let state = self.state.clone();
        
        tokio::spawn(async move {
            let mut message_rx = rx;
            while let Some(message) = message_rx.recv().await {
                if result_tx.send(Ok(message)).await.is_err() {
                    break;
                }
            }
            state.remove_subscriber(&subscriber_id_clone).await;
        });

        self.state.add_subscriber(subscriber_id, tx).await;
        
        info!("New message subscriber connected");
        
        Ok(Response::new(ReceiverStream::new(result_rx)))
    }

    async fn get_history(
        &self,
        request: Request<GetHistoryRequest>,
    ) -> Result<Response<GetHistoryResponse>, Status> {
        let req = request.into_inner();
        let messages = self.state.messages.read().await;
        
        let filtered_messages: Vec<Message> = messages
            .iter()
            .filter(|msg| {
                (msg.sender_id == req.peer_id || msg.recipient_id == req.peer_id) &&
                (req.before_timestamp == 0 || msg.timestamp < req.before_timestamp)
            })
            .take(req.limit as usize)
            .cloned()
            .collect();

        let has_more = filtered_messages.len() == req.limit as usize;

        Ok(Response::new(GetHistoryResponse {
            messages: filtered_messages,
            has_more,
        }))
    }

    type ChatStreamStream = ReceiverStream<Result<Message, Status>>;

    async fn chat_stream(
        &self,
        request: Request<Streaming<Message>>,
    ) -> Result<Response<Self::ChatStreamStream>, Status> {
        let mut stream = request.into_inner();
        let (tx, rx) = mpsc::channel(100);
        let state = self.state.clone();

        tokio::spawn(async move {
            while let Some(result) = stream.message().await.transpose() {
                match result {
                    Ok(message) => {
                        info!("Received chat stream message: {}", message.content);
                        
                        state.add_message(message.clone()).await;
                        
                        let response = Message {
                            id: uuid::new_v4().to_string(),
                            sender_id: state.node_id.clone(),
                            recipient_id: message.sender_id,
                            content: format!("Echo: {}", message.content),
                            timestamp: chrono::Utc::now().timestamp(),
                            r#type: MessageType::Text as i32,
                        };
                        
                        if tx.send(Ok(response)).await.is_err() {
                            break;
                        }
                    }
                    Err(e) => {
                        warn!("Error in chat stream: {}", e);
                        let _ = tx.send(Err(e)).await;
                        break;
                    }
                }
            }
        });

        Ok(Response::new(ReceiverStream::new(rx)))
    }
}

#[derive(Clone, Debug)]
struct NodeServiceImpl {
    state: ChatState,
    start_time: std::time::Instant,
}

impl NodeServiceImpl {
    fn new(state: ChatState) -> Self {
        Self {
            state,
            start_time: std::time::Instant::now(),
        }
    }
}

#[tonic::async_trait]
impl NodeService for NodeServiceImpl {
    async fn get_node_info(
        &self,
        _request: Request<Empty>,
    ) -> Result<Response<NodeInfo>, Status> {
        Ok(Response::new(NodeInfo {
            node_id: self.state.node_id.clone(),
            addresses: vec![],
            version: env!("CARGO_PKG_VERSION").to_string(),
            uptime_seconds: self.start_time.elapsed().as_secs() as i64,
            connected_peers: 0,
        }))
    }

    async fn ping(
        &self,
        request: Request<PingRequest>,
    ) -> Result<Response<PingResponse>, Status> {
        let req = request.into_inner();
        let now = chrono::Utc::now().timestamp();
        
        Ok(Response::new(PingResponse {
            request_timestamp: req.timestamp,
            response_timestamp: now,
            echo_data: req.data,
        }))
    }

    async fn list_peers(
        &self,
        _request: Request<Empty>,
    ) -> Result<Response<ListPeersResponse>, Status> {
        Ok(Response::new(ListPeersResponse {
            peers: vec![],
        }))
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    tracing_subscriber::fmt()
        .with_max_level(if args.verbose { tracing::Level::DEBUG } else { tracing::Level::INFO })
        .init();

    info!("Starting P2P Chat Node");

    let mut endpoint_builder = iroh::Endpoint::builder();
    
    if let Some(secret_key) = args.secret_key {
        let secret_key = SecretKey::from_str(&secret_key)?;
        endpoint_builder = endpoint_builder.secret_key(secret_key);
    }
    
    if args.port > 0 {
        endpoint_builder = endpoint_builder.bind_addr_v4(std::net::SocketAddrV4::new(std::net::Ipv4Addr::UNSPECIFIED, args.port));
    }
    
    if args.no_relay {
        info!("Relay servers disabled - using direct connections only");
        // Disable all relay URLs for direct-only mode
        endpoint_builder = endpoint_builder.relay_mode(iroh::RelayMode::Disabled);
    }
    
    let endpoint = endpoint_builder.bind().await?;
    let node_id = endpoint.node_id().to_string();
    
    info!("Node ID: {}", node_id);
    
    let local_addrs = endpoint.bound_sockets();
    if let Some(socket_addr) = local_addrs.first() {
        let node_addr = NodeAddr::new(endpoint.node_id()).with_direct_addresses([*socket_addr]);
        info!("Node Address: {:?}", node_addr);
    }

    let chat_state = ChatState::new(node_id.clone());
    let chat_service = ChatServiceImpl::new(chat_state.clone());
    let node_service = NodeServiceImpl::new(chat_state);

    let (chat_handler, chat_incoming, chat_alpn) = GrpcProtocolHandler::for_service::<P2pChatServiceServer<ChatServiceImpl>>();
    let (node_handler, node_incoming, node_alpn) = GrpcProtocolHandler::for_service::<NodeServiceServer<NodeServiceImpl>>();

    let endpoint_for_client = endpoint.clone();
    
    // Log the protocols before moving
    info!("P2P Chat Node started successfully");
    info!("Available protocols:");
    info!("  - Chat Service: {}", String::from_utf8_lossy(&chat_alpn));
    info!("  - Node Service: {}", String::from_utf8_lossy(&node_alpn));

    let router = iroh::protocol::Router::builder(endpoint)
        .accept(chat_alpn, chat_handler)
        .accept(node_alpn, node_handler)
        .spawn();

    // Spawn chat server
    let chat_server = Server::builder()
        .add_service(P2pChatServiceServer::new(chat_service))
        .serve_with_incoming(chat_incoming);
        
    // Spawn node server
    let node_server = Server::builder()
        .add_service(NodeServiceServer::new(node_service))
        .serve_with_incoming(node_incoming);

    if let Some(target_node_id) = args.connect_to {
        let endpoint_clone = endpoint_for_client.clone();
        let target_addresses = args.target_addresses.clone();
        let command = args.command;
        
        tokio::spawn(async move {
            if let Err(e) = connect_and_execute_command(endpoint_clone, target_node_id, target_addresses, command).await {
                error!("Failed to connect and execute command: {}", e);
            }
        });
    }

    tokio::select! {
        result = chat_server => {
            if let Err(e) = result {
                error!("Chat server error: {}", e);
            }
        }
        result = node_server => {
            if let Err(e) = result {
                error!("Node server error: {}", e);
            }
        }
        _ = tokio::signal::ctrl_c() => {
            info!("Shutting down node...");
        }
    }

    router.shutdown().await?;
    
    Ok(())
}

async fn connect_and_execute_command(
    endpoint: iroh::Endpoint,
    target_node_id: String,
    target_addresses: Vec<String>,
    command: Option<Command>,
) -> Result<()> {
    let target_node_id = NodeId::from_str(&target_node_id)?;
    let target_addr = if target_addresses.is_empty() {
        NodeAddr::new(target_node_id)
    } else {
        let mut addr = NodeAddr::new(target_node_id);
        for address_str in target_addresses {
            addr = addr.with_direct_addresses([address_str.parse()?]);
        }
        addr
    };

    info!("Connecting to: {:?}", target_addr);

    let local_node_id = endpoint.node_id().to_string();
    
    // Create typed clients using IrohClient
    let iroh_client = IrohClient::new(endpoint);
    let chat_channel = iroh_client.connect_to_service::<P2pChatServiceServer<ChatServiceImpl>>(target_addr.clone()).await?;
    let node_channel = iroh_client.connect_to_service::<NodeServiceServer<NodeServiceImpl>>(target_addr).await?;
    let mut chat_client = P2pChatServiceClient::new(chat_channel);
    let mut node_client = NodeServiceClient::new(node_channel);

    match command {
        Some(Command::Send { message }) => {
            send_message(&mut chat_client, &target_node_id.to_string(), message).await?;
            std::process::exit(0);
        }
        Some(Command::Subscribe) => {
            subscribe_messages(&mut chat_client).await?;
            std::process::exit(0);
        }
        Some(Command::History { limit }) => {
            get_history(&mut chat_client, &target_node_id.to_string(), limit).await?;
            std::process::exit(0);
        }
        Some(Command::Chat) => {
            interactive_chat(&mut chat_client, &local_node_id).await?;
            std::process::exit(0);
        }
        Some(Command::Ping { data }) => {
            ping_node(&mut node_client, data).await?;
            std::process::exit(0);
        }
        Some(Command::NodeInfo) => {
            get_node_info(&mut node_client).await?;
            std::process::exit(0);
        }
        Some(Command::ListPeers) => {
            list_peers(&mut node_client).await?;
            std::process::exit(0);
        }
        Some(Command::Benchmark { duration, message, concurrent }) => {
            benchmark_messages(&mut chat_client, &target_node_id.to_string(), message, duration, concurrent).await?;
            std::process::exit(0);
        }
        None => {
            info!("Connected to peer. No command specified.");
            ping_node(&mut node_client, Some("Hello from P2P node!".to_string())).await?;
            std::process::exit(0);
        }
    }
}

async fn send_message(
    client: &mut P2pChatServiceClient<IrohChannel>,
    recipient_id: &str,
    content: String,
) -> Result<()> {
    info!("Sending message: {}", content);
    
    let request = Request::new(SendMessageRequest {
        recipient_id: recipient_id.to_string(),
        content,
        timestamp: chrono::Utc::now().timestamp(),
    });

    let response = client.send_message(request).await?;
    let response = response.into_inner();

    if response.success {
        info!("Message sent successfully (ID: {})", response.message_id);
    } else {
        error!("Failed to send message: {}", response.error);
    }

    Ok(())
}

async fn subscribe_messages(
    client: &mut P2pChatServiceClient<IrohChannel>,
) -> Result<()> {
    info!("Subscribing to messages...");
    
    let request = Request::new(SubscribeRequest {});
    let mut stream = client.subscribe_messages(request).await?.into_inner();

    info!("Connected to message stream. Waiting for messages...");
    
    while let Some(message_result) = stream.next().await {
        match message_result {
            Ok(message) => {
                info!(
                    "[{}] {}: {}",
                    format_timestamp(message.timestamp),
                    message.sender_id,
                    message.content
                );
            }
            Err(e) => {
                error!("Error receiving message: {}", e);
                break;
            }
        }
    }

    Ok(())
}

async fn get_history(
    client: &mut P2pChatServiceClient<IrohChannel>,
    peer_id: &str,
    limit: i32,
) -> Result<()> {
    info!("Getting chat history...");
    
    let request = Request::new(GetHistoryRequest {
        peer_id: peer_id.to_string(),
        limit,
        before_timestamp: 0,
    });

    let response = client.get_history(request).await?;
    let response = response.into_inner();

    info!("Retrieved {} messages:", response.messages.len());
    
    for message in response.messages {
        info!(
            "[{}] {} -> {}: {}",
            format_timestamp(message.timestamp),
            message.sender_id,
            message.recipient_id,
            message.content
        );
    }

    if response.has_more {
        info!("(More messages available)");
    }

    Ok(())
}

async fn interactive_chat(
    client: &mut P2pChatServiceClient<IrohChannel>,
    local_node_id: &str,
) -> Result<()> {
    info!("Starting interactive chat session. Type 'quit' to exit.");
    
    let (tx, rx) = tokio::sync::mpsc::channel(100);
    let rx_stream = tokio_stream::wrappers::ReceiverStream::new(rx);
    
    let request = Request::new(rx_stream);
    let mut response_stream = client.chat_stream(request).await?.into_inner();

    let response_handle = tokio::spawn(async move {
        while let Some(message_result) = response_stream.next().await {
            match message_result {
                Ok(message) => {
                    println!("< {}", message.content);
                }
                Err(e) => {
                    error!("Error receiving message: {}", e);
                    break;
                }
            }
        }
    });

    let mut stdin = tokio::io::BufReader::new(tokio::io::stdin());
    let mut line = String::new();
    
    loop {
        line.clear();
        use tokio::io::AsyncBufReadExt;
        
        print!("> ");
        if stdin.read_line(&mut line).await? == 0 {
            break;
        }
        
        let content = line.trim().to_string();
        if content == "quit" {
            break;
        }
        
        if !content.is_empty() {
            let message = Message {
                id: uuid::new_v4().to_string(),
                sender_id: local_node_id.to_string(),
                recipient_id: String::new(),
                content,
                timestamp: chrono::Utc::now().timestamp(),
                r#type: MessageType::Text as i32,
            };
            
            if tx.send(message).await.is_err() {
                error!("Failed to send message");
                break;
            }
        }
    }

    drop(tx);
    response_handle.abort();
    
    Ok(())
}

async fn ping_node(
    client: &mut NodeServiceClient<IrohChannel>,
    data: Option<String>,
) -> Result<()> {
    let now = chrono::Utc::now().timestamp();
    let data = data.unwrap_or_else(|| "ping".to_string());
    
    info!("Pinging node with data: {}", data);
    
    let request = Request::new(PingRequest {
        timestamp: now,
        data: data.clone(),
    });

    let response = client.ping(request).await?;
    let response = response.into_inner();

    let rtt = response.response_timestamp - response.request_timestamp;
    
    info!(
        "Pong received! RTT: {}ms, Echo: {}",
        rtt,
        response.echo_data
    );

    Ok(())
}

async fn get_node_info(
    client: &mut NodeServiceClient<IrohChannel>,
) -> Result<()> {
    info!("Getting node information...");
    
    let request = Request::new(Empty {});
    let response = client.get_node_info(request).await?;
    let response = response.into_inner();

    info!("Node Information:");
    info!("  Node ID: {}", response.node_id);
    info!("  Version: {}", response.version);
    info!("  Uptime: {}s", response.uptime_seconds);
    info!("  Connected Peers: {}", response.connected_peers);
    info!("  Addresses:");
    for addr in response.addresses {
        info!("    {}", addr);
    }

    Ok(())
}

async fn list_peers(
    client: &mut NodeServiceClient<IrohChannel>,
) -> Result<()> {
    info!("Listing connected peers...");
    
    let request = Request::new(Empty {});
    let response = client.list_peers(request).await?;
    let response = response.into_inner();

    info!("Connected Peers ({}):", response.peers.len());
    for peer in response.peers {
        let connection_type = match ConnectionType::try_from(peer.connection_type) {
            Ok(ConnectionType::Direct) => "Direct",
            Ok(ConnectionType::Relay) => "Relay",
            _ => "Unknown",
        };
        
        info!("  {}", peer.node_id);
        info!("    Connection: {}", connection_type);
        info!("    Since: {}", format_timestamp(peer.connected_since));
        for addr in peer.addresses {
            info!("    Address: {}", addr);
        }
    }

    Ok(())
}

async fn benchmark_messages(
    client: &mut P2pChatServiceClient<IrohChannel>,
    recipient_id: &str,
    message_content: String,
    duration_secs: u64,
    max_concurrent: usize,
) -> Result<()> {
    info!("Starting benchmark: {} seconds, max {} concurrent messages", duration_secs, max_concurrent);
    
    let start_time = std::time::Instant::now();
    let duration = std::time::Duration::from_secs(duration_secs);
    let mut message_count = 0u64;
    let mut error_count = 0u64;
    let mut tasks = tokio::task::JoinSet::new();
    
    while start_time.elapsed() < duration {
        // Keep max_concurrent tasks running
        while tasks.len() < max_concurrent && start_time.elapsed() < duration {
            let mut client_clone = client.clone();
            let recipient_id = recipient_id.to_string();
            let content = format!("{} #{}", message_content, message_count);
            message_count += 1;
            
            tasks.spawn(async move {
                let request = Request::new(SendMessageRequest {
                    recipient_id,
                    content,
                    timestamp: chrono::Utc::now().timestamp(),
                });
                
                client_clone.send_message(request).await
            });
        }
        
        // Wait for at least one task to complete
        if let Some(result) = tasks.join_next().await {
            match result {
                Ok(Ok(response)) => {
                    let response = response.into_inner();
                    if !response.success {
                        error_count += 1;
                        if error_count % 10 == 1 {
                            warn!("Message failed: {}", response.error);
                        }
                    }
                }
                Ok(Err(e)) => {
                    error_count += 1;
                    if error_count % 10 == 1 {
                        warn!("gRPC error: {}", e);
                    }
                }
                Err(e) => {
                    error_count += 1;
                    if error_count % 10 == 1 {
                        warn!("Task error: {}", e);
                    }
                }
            }
        }
    }
    
    // Wait for remaining tasks to complete
    while let Some(result) = tasks.join_next().await {
        match result {
            Ok(Ok(response)) => {
                let response = response.into_inner();
                if !response.success {
                    error_count += 1;
                }
            }
            Ok(Err(_)) | Err(_) => {
                error_count += 1;
            }
        }
    }
    
    let elapsed = start_time.elapsed();
    let messages_per_sec = message_count as f64 / elapsed.as_secs_f64();
    let success_count = message_count - error_count;
    let success_rate = (success_count as f64 / message_count as f64) * 100.0;
    
    info!("=== Benchmark Results ===");
    info!("Duration: {:.2}s", elapsed.as_secs_f64());
    info!("Total messages: {}", message_count);
    info!("Successful: {} ({:.1}%)", success_count, success_rate);
    info!("Errors: {}", error_count);
    info!("Messages/sec: {:.2}", messages_per_sec);
    info!("Avg latency: {:.2}ms", elapsed.as_millis() as f64 / message_count as f64);
    
    Ok(())
}

fn format_timestamp(timestamp: i64) -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    
    let duration = std::time::Duration::from_secs(timestamp as u64);
    let datetime = UNIX_EPOCH + duration;
    
    match datetime.duration_since(SystemTime::now()) {
        Ok(_) => format!("future+{}s", timestamp),
        Err(e) => format!("{}s ago", e.duration().as_secs()),
    }
}

mod uuid {
    pub fn new_v4() -> String {
        format!("{:032x}", crate::rand::random::<u128>())
    }
}

mod chrono {
    pub struct Utc;
    
    impl Utc {
        pub fn now() -> DateTime {
            DateTime(std::time::SystemTime::now())
        }
    }
    
    pub struct DateTime(std::time::SystemTime);
    
    impl DateTime {
        pub fn timestamp(&self) -> i64 {
            self.0.duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs() as i64
        }
    }
}

mod rand {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    use std::time::SystemTime;
    
    pub fn random<T: From<u128>>() -> T {
        let mut hasher = DefaultHasher::new();
        SystemTime::now().hash(&mut hasher);
        std::thread::current().id().hash(&mut hasher);
        T::from(hasher.finish() as u128)
    }
}
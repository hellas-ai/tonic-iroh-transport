use anyhow::Result;
use clap::Parser;
use std::collections::HashMap;
use std::str::FromStr;
use std::sync::Arc;
use tokio::sync::{mpsc, RwLock};
use tokio_stream::wrappers::ReceiverStream;
use tokio_stream::StreamExt;
use tonic::transport::Channel;
use tonic::{Request, Response, Status, Streaming};
use tracing::{error, info, warn};
use tracing_subscriber::EnvFilter;

mod pb;

use pb::p2p_chat::v1::{
    node_service_client::NodeServiceClient,
    node_service_server::{NodeService, NodeServiceServer},
    p2p_chat_service_client::P2pChatServiceClient,
    p2p_chat_service_server::{P2pChatService, P2pChatServiceServer},
    *,
};

use tonic_iroh_transport::iroh::{self, EndpointAddr, EndpointId, SecretKey};
use tonic_iroh_transport::{IrohConnect, IrohContext, TransportBuilder};

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

    /// Exit after processing this many messages (server mode)
    #[clap(long)]
    max_requests: Option<u64>,

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
    /// Shutdown the target node gracefully
    Shutdown,
}

#[derive(Clone)]
struct ChatState {
    messages: Arc<RwLock<Vec<ChatMessage>>>,
    subscribers: Arc<RwLock<HashMap<String, mpsc::Sender<ChatMessage>>>>,
    node_id: String,
    request_count: Arc<std::sync::atomic::AtomicU64>,
    max_requests: Option<u64>,
    shutdown_tx: Arc<tokio::sync::broadcast::Sender<()>>,
}

impl std::fmt::Debug for ChatState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChatState")
            .field("node_id", &self.node_id)
            .field("messages", &"<RwLock<Vec<ChatMessage>>>")
            .field(
                "subscribers",
                &"<RwLock<HashMap<String, mpsc::Sender<ChatMessage>>>>",
            )
            .finish()
    }
}

impl ChatState {
    fn new(
        node_id: String,
        max_requests: Option<u64>,
    ) -> (Self, tokio::sync::broadcast::Receiver<()>) {
        let (shutdown_tx, shutdown_rx) = tokio::sync::broadcast::channel(1);
        let state = Self {
            messages: Arc::new(RwLock::new(Vec::new())),
            subscribers: Arc::new(RwLock::new(HashMap::new())),
            node_id,
            request_count: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            max_requests,
            shutdown_tx: Arc::new(shutdown_tx),
        };
        (state, shutdown_rx)
    }

    async fn add_message(&self, message: ChatMessage) {
        self.messages.write().await.push(message.clone());

        let subscribers = self.subscribers.read().await;
        for (subscriber_id, sender) in subscribers.iter() {
            if sender.send(message.clone()).await.is_err() {
                warn!("Failed to send message to subscriber: {}", subscriber_id);
            }
        }
    }

    async fn add_subscriber(&self, id: String, sender: mpsc::Sender<ChatMessage>) {
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
        let connect_info = request.extensions().get::<IrohContext>().cloned();

        let req = request.into_inner();

        // Log connection details if available
        if let Some(context) = connect_info {
            info!("=== Connection Info ===");
            info!("Remote Node ID: {}", context.node_id);
            info!(
                "Connection established: {:?} ago",
                context.established_at.elapsed()
            );
            info!("ALPN protocol: {}", String::from_utf8_lossy(&context.alpn));
            info!("======================");
        }

        let message_id = uuid::new_v4().to_string();
        let message = ChatMessage {
            id: message_id.clone(),
            sender_id: self.state.node_id.clone(),
            recipient_id: req.recipient_id.clone(),
            content: req.content,
            timestamp: req.timestamp,
            r#type: MessageType::Text.into(),
        };

        info!(
            "Received message from {}: {}",
            message.sender_id, message.content
        );

        self.state.add_message(message).await;

        // Increment request counter and check if we should exit
        let count = self
            .state
            .request_count
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
            + 1;
        if let Some(max) = self.state.max_requests {
            if count >= max {
                println!("Processed {count} requests, triggering graceful shutdown");
                let _ = self.state.shutdown_tx.send(());
            }
        }

        Ok(Response::new(SendMessageResponse {
            success: true,
            error: String::new(),
            message_id,
        }))
    }

    type SubscribeMessagesStream = ReceiverStream<Result<SubscribeMessagesResponse, Status>>;

    async fn subscribe_messages(
        &self,
        request: Request<SubscribeMessagesRequest>,
    ) -> Result<Response<Self::SubscribeMessagesStream>, Status> {
        // Extract connection information for subscriber
        let connect_info = request.extensions().get::<IrohContext>().cloned();

        if let Some(context) = connect_info {
            info!("=== New Subscriber ===");
            info!("Subscriber Node ID: {}", context.node_id);
            info!("Connection age: {:?}", context.established_at.elapsed());
            info!("ALPN protocol: {}", String::from_utf8_lossy(&context.alpn));
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
                let response = SubscribeMessagesResponse {
                    message: Some(message),
                };
                if result_tx.send(Ok(response)).await.is_err() {
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

        let filtered_messages: Vec<ChatMessage> = messages
            .iter()
            .filter(|msg| {
                (msg.sender_id == req.peer_id || msg.recipient_id == req.peer_id)
                    && (req.before_timestamp == 0 || msg.timestamp < req.before_timestamp)
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

    type ChatStreamStream = ReceiverStream<Result<ChatStreamResponse, Status>>;

    async fn chat_stream(
        &self,
        request: Request<Streaming<ChatStreamRequest>>,
    ) -> Result<Response<Self::ChatStreamStream>, Status> {
        let mut stream = request.into_inner();
        let (tx, rx) = mpsc::channel(100);
        let state = self.state.clone();

        tokio::spawn(async move {
            while let Some(result) = stream.message().await.transpose() {
                match result {
                    Ok(request) => {
                        let Some(message) = request.message else {
                            warn!("Received chat stream request without message");
                            continue;
                        };

                        info!("Received chat stream message: {}", message.content);

                        // Store the message directly
                        state.add_message(message.clone()).await;

                        let response_message = ChatMessage {
                            id: uuid::new_v4().to_string(),
                            sender_id: state.node_id.clone(),
                            recipient_id: message.sender_id,
                            content: format!("Echo: {}", message.content),
                            timestamp: chrono::Utc::now().timestamp(),
                            r#type: MessageType::Text.into(),
                        };

                        let response = ChatStreamResponse {
                            message: Some(response_message),
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
        _request: Request<GetNodeInfoRequest>,
    ) -> Result<Response<GetNodeInfoResponse>, Status> {
        Ok(Response::new(GetNodeInfoResponse {
            node_id: self.state.node_id.clone(),
            addresses: vec![],
            version: env!("CARGO_PKG_VERSION").to_string(),
            uptime_seconds: self.start_time.elapsed().as_secs() as i64,
            connected_peers: 0,
        }))
    }

    async fn ping(&self, request: Request<PingRequest>) -> Result<Response<PingResponse>, Status> {
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
        _request: Request<ListPeersRequest>,
    ) -> Result<Response<ListPeersResponse>, Status> {
        Ok(Response::new(ListPeersResponse { peers: vec![] }))
    }

    async fn shutdown(
        &self,
        _request: Request<ShutdownRequest>,
    ) -> Result<Response<ShutdownResponse>, Status> {
        println!("Shutdown requested via RPC, triggering graceful shutdown");
        let _ = self.state.shutdown_tx.send(());
        Ok(Response::new(ShutdownResponse {}))
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    // Initialize tracing with environment variable support
    let env_filter = if args.verbose {
        // If verbose flag is set, override to DEBUG level
        EnvFilter::try_from_default_env()
            .unwrap_or_else(|_| EnvFilter::new("debug"))
            .add_directive("debug".parse().unwrap())
    } else {
        // Use RUST_LOG if set, otherwise default to INFO
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"))
    };

    tracing_subscriber::fmt().with_env_filter(env_filter).init();

    info!("Starting P2P Chat Node");

    let mut endpoint_builder = iroh::Endpoint::builder();

    if let Some(secret_key) = args.secret_key {
        let secret_key = SecretKey::from_str(&secret_key)?;
        endpoint_builder = endpoint_builder.secret_key(secret_key);
    }

    if args.port > 0 {
        endpoint_builder = endpoint_builder.bind_addr_v4(std::net::SocketAddrV4::new(
            std::net::Ipv4Addr::UNSPECIFIED,
            args.port,
        ));
    }

    if args.no_relay {
        info!("Relay servers disabled - using direct connections only");
        // Disable all relay URLs for direct-only mode
        endpoint_builder = endpoint_builder.relay_mode(iroh::RelayMode::Disabled);
    }

    let endpoint = endpoint_builder.bind().await?;
    let node_id = endpoint.id().to_string();

    println!("Node ID: {node_id}");

    let addrs = endpoint.bound_sockets();
    let mut node_addr = EndpointAddr::new(endpoint.id());
    for addr in addrs {
        node_addr = node_addr.with_ip_addr(addr);
    }
    info!("Node Address: {:?}", node_addr);

    let (chat_state, mut shutdown_rx) = ChatState::new(node_id.clone(), args.max_requests);
    let chat_service = ChatServiceImpl::new(chat_state.clone());
    let node_service = NodeServiceImpl::new(chat_state.clone());

    let transport_guard = TransportBuilder::new(endpoint.clone())
        .add_rpc(P2pChatServiceServer::new(chat_service))
        .add_rpc(NodeServiceServer::new(node_service))
        .spawn()
        .await?;

    let endpoint_for_client = endpoint.clone();

    // Log the protocols before moving
    println!("P2P Chat Node started successfully");
    info!("Available protocols:");
    info!("  - Chat Service: /p2p_chat.P2pChatService/1.0");
    info!("  - Node Service: /p2p_chat.NodeService/1.0");

    if let Some(target_node_id) = args.connect_to {
        let endpoint_clone = endpoint_for_client.clone();
        let target_addresses = args.target_addresses.clone();
        let command = args.command;

        tokio::spawn(async move {
            if let Err(e) = connect_and_execute_command(
                endpoint_clone,
                target_node_id,
                target_addresses,
                command,
            )
            .await
            {
                error!("Failed to connect and execute command: {}", e);
            }
        });
    }

    tokio::select! {
        _ = tokio::signal::ctrl_c() => {
            info!("Shutting down node...");
        }
        result = shutdown_rx.recv() => {
            match result {
                Ok(_) => info!("Graceful shutdown triggered by max requests"),
                Err(_) => info!("Shutdown channel closed"),
            }
        }
    }

    transport_guard.shutdown().await?;

    Ok(())
}

async fn connect_and_execute_command(
    endpoint: iroh::Endpoint,
    target_node_id: String,
    target_addresses: Vec<String>,
    command: Option<Command>,
) -> Result<()> {
    let target_node_id = EndpointId::from_str(&target_node_id)?;
    let target_addr = if target_addresses.is_empty() {
        EndpointAddr::new(target_node_id)
    } else {
        let mut addr = EndpointAddr::new(target_node_id);
        for address_str in target_addresses {
            addr = addr.with_ip_addr(address_str.parse()?);
        }
        addr
    };

    info!("Connecting to: {:?}", target_addr);

    let local_node_id = endpoint.id().to_string();

    // Connect to services using IrohConnect trait
    let chat_channel =
        P2pChatServiceServer::<ChatServiceImpl>::connect(&endpoint, target_addr.clone()).await?;
    let node_channel =
        NodeServiceServer::<NodeServiceImpl>::connect(&endpoint, target_addr).await?;
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
        Some(Command::Benchmark {
            duration,
            message,
            concurrent,
        }) => {
            benchmark_messages(
                &mut chat_client,
                &target_node_id.to_string(),
                message,
                duration,
                concurrent,
            )
            .await?;
            std::process::exit(0);
        }
        Some(Command::Shutdown) => {
            shutdown_node(&mut node_client).await?;
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
    client: &mut P2pChatServiceClient<Channel>,
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

async fn subscribe_messages(client: &mut P2pChatServiceClient<Channel>) -> Result<()> {
    info!("Subscribing to messages...");

    let request = Request::new(SubscribeMessagesRequest {});
    let mut stream = client.subscribe_messages(request).await?.into_inner();

    info!("Connected to message stream. Waiting for messages...");

    while let Some(message_result) = stream.next().await {
        match message_result {
            Ok(response) => {
                if let Some(message) = response.message {
                    info!(
                        "[{}] {}: {}",
                        format_timestamp(message.timestamp),
                        message.sender_id,
                        message.content
                    );
                }
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
    client: &mut P2pChatServiceClient<Channel>,
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
    client: &mut P2pChatServiceClient<Channel>,
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
                Ok(response) => {
                    if let Some(message) = response.message {
                        println!("< {}", message.content);
                    }
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
            let chat_message = ChatMessage {
                id: uuid::new_v4().to_string(),
                sender_id: local_node_id.to_string(),
                recipient_id: String::new(),
                content,
                timestamp: chrono::Utc::now().timestamp(),
                r#type: MessageType::Text.into(),
            };
            let request = ChatStreamRequest {
                message: Some(chat_message),
            };

            if tx.send(request).await.is_err() {
                error!("Failed to send message");
                break;
            }
        }
    }

    drop(tx);
    response_handle.abort();

    Ok(())
}

async fn ping_node(client: &mut NodeServiceClient<Channel>, data: Option<String>) -> Result<()> {
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
        rtt, response.echo_data
    );

    Ok(())
}

async fn get_node_info(client: &mut NodeServiceClient<Channel>) -> Result<()> {
    info!("Getting node information...");

    let request = Request::new(GetNodeInfoRequest {});
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

async fn list_peers(client: &mut NodeServiceClient<Channel>) -> Result<()> {
    info!("Listing connected peers...");

    let request = Request::new(ListPeersRequest {});
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

async fn shutdown_node(client: &mut NodeServiceClient<Channel>) -> Result<()> {
    println!("Requesting graceful shutdown of target node...");

    let request = Request::new(ShutdownRequest {});
    let _response = client.shutdown(request).await?;

    println!("Shutdown request sent successfully");
    Ok(())
}

async fn benchmark_messages(
    client: &mut P2pChatServiceClient<Channel>,
    recipient_id: &str,
    message_content: String,
    duration_secs: u64,
    max_concurrent: usize,
) -> Result<()> {
    println!(
        "Starting benchmark: {duration_secs} seconds, max {max_concurrent} concurrent messages"
    );

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
            let content = format!("{message_content} #{message_count}");
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

    println!("=== Benchmark Results ===");
    println!("Duration: {:.2}s", elapsed.as_secs_f64());
    println!("Total messages: {message_count}");
    println!("Successful: {success_count} ({success_rate:.1}%)");
    println!("Errors: {error_count}");
    println!("Messages/sec: {messages_per_sec:.2}");
    println!(
        "Avg latency: {:.2}ms",
        elapsed.as_millis() as f64 / message_count as f64
    );

    Ok(())
}

fn format_timestamp(timestamp: i64) -> String {
    use std::time::{SystemTime, UNIX_EPOCH};

    let duration = std::time::Duration::from_secs(timestamp as u64);
    let datetime = UNIX_EPOCH + duration;

    match datetime.duration_since(SystemTime::now()) {
        Ok(_) => format!("future+{timestamp}s"),
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
            self.0
                .duration_since(std::time::UNIX_EPOCH)
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

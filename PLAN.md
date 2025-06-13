# tonic-iroh Implementation Plan (Revised)

## Project Goals

Build the **best** tonic+iroh integration library using the **correct tonic transport pattern**:
- Provide AsyncRead/AsyncWrite QUIC streams to tonic (like UDS example)
- Let tonic handle ALL gRPC framing, HTTP/2, serialization
- Focus on efficient P2P transport with rich middleware
- Multi-transport compatibility for maximum flexibility

## Key Architectural Insight

**Wrong Approach** (V1): Custom HTTP serialization over QUIC streams
```rust
// BAD: We were doing this
let request_metadata = serialize_request_metadata(&parts)?;
send_stream.write_all(&request_metadata).await?;
```

**Correct Approach**: Follow tonic's transport pattern from UDS example
```rust
// GOOD: Just provide AsyncRead + AsyncWrite, tonic does the rest
let channel = Endpoint::try_from("http://[::]:50051")?
    .connect_with_connector(service_fn(|_: Uri| async {
        Ok::<_, std::io::Error>(TokioIo::new(IrohStream::connect(addr).await?))
    }))
    .await?;
```

## Core Architecture

### Transport Components
```rust
// Core transport abstraction
pub struct IrohStream {
    send: iroh::endpoint::SendStream,
    recv: iroh::endpoint::RecvStream,
}

impl AsyncRead for IrohStream { /* delegate to recv */ }
impl AsyncWrite for IrohStream { /* delegate to send */ }
impl Connected for IrohStream { /* provide peer info */ }

// Client connector
pub struct IrohConnector {
    transport: IrohTransport,
}

impl Service<Uri> for IrohConnector {
    type Response = TokioIo<IrohStream>;
    // Returns wrapped QUIC stream
}

// Server incoming stream
pub struct IrohIncoming {
    transport: IrohTransport,
}

impl Stream for IrohIncoming {
    type Item = Result<TokioIo<IrohStream>, Error>;
    // Yields wrapped QUIC streams
}
```

### Usage Pattern (Same as UDS example)
```rust
// Client
let channel = Endpoint::try_from("http://iroh://node-id")?
    .connect_with_connector(IrohConnector::new(transport))
    .await?;
let mut client = MyServiceClient::new(channel);

// Server  
let incoming = IrohIncoming::new(transport);
Server::builder()
    .add_service(MyServiceServer::new(service))
    .serve_with_incoming(incoming)
    .await?;
```

## Implementation Phases

### Phase 1: Core Transport Layer (2-3 days)
**Goal**: Basic QUIC ↔ AsyncRead/AsyncWrite bridge working

#### 1.1 IrohStream Implementation
```rust
pub struct IrohStream {
    send: iroh::endpoint::SendStream,
    recv: iroh::endpoint::RecvStream,
    peer_info: IrohPeerInfo,
}

pub struct IrohPeerInfo {
    node_id: NodeId,
    connection_type: ConnectionType, // Direct/Relay
    established_at: Instant,
    alpn: Vec<u8>,
}

impl AsyncRead for IrohStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        // Delegate to recv stream
        Pin::new(&mut self.recv).poll_read(cx, buf)
    }
}

impl AsyncWrite for IrohStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<Result<usize, io::Error>> {
        // Delegate to send stream
        Pin::new(&mut self.send).poll_write(cx, buf)
    }
    
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), io::Error>> {
        Pin::new(&mut self.send).poll_flush(cx)
    }
    
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), io::Error>> {
        Pin::new(&mut self.send).poll_shutdown(cx)
    }
}

impl Connected for IrohStream {
    type ConnectInfo = IrohPeerInfo;
    
    fn connect_info(&self) -> Self::ConnectInfo {
        self.peer_info.clone()
    }
}
```

#### 1.2 IrohConnector for Clients
```rust
pub struct IrohConnector {
    transport: IrohTransport,
}

impl IrohConnector {
    pub fn new(transport: IrohTransport) -> Self {
        Self { transport }
    }
}

impl Service<Uri> for IrohConnector {
    type Response = TokioIo<IrohStream>;
    type Error = Error;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, uri: Uri) -> Self::Future {
        let transport = self.transport.clone();
        
        Box::pin(async move {
            // Parse iroh URI: iroh://node-id or iroh://node-id@relay-url
            let target_addr = parse_iroh_uri(uri)?;
            
            // Connect to peer
            let connection = transport.connect_to_peer(target_addr).await?;
            let (send, recv) = connection.open_bi().await?;
            
            let peer_info = IrohPeerInfo {
                node_id: connection.remote_node_id(),
                connection_type: determine_connection_type(&connection),
                established_at: Instant::now(),
                alpn: GRPC_ALPN.to_vec(),
            };
            
            let stream = IrohStream { send, recv, peer_info };
            Ok(TokioIo::new(stream))
        })
    }
}
```

#### 1.3 IrohIncoming for Servers
```rust
pub struct IrohIncoming {
    transport: IrohTransport,
    #[pin]
    connection_stream: IncomingConnectionStream,
}

impl Stream for IrohIncoming {
    type Item = Result<TokioIo<IrohStream>, Error>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.project();
        
        match ready!(this.connection_stream.poll_next(cx)) {
            Some(Ok(connection)) => {
                // Accept a bi-directional stream for this gRPC call
                let peer_info = IrohPeerInfo {
                    node_id: connection.remote_node_id(),
                    connection_type: determine_connection_type(&connection),
                    established_at: Instant::now(),
                    alpn: GRPC_ALPN.to_vec(),
                };
                
                // Spawn task to handle this connection's streams
                let stream_future = async move {
                    let (send, recv) = connection.accept_bi().await?;
                    let stream = IrohStream { send, recv, peer_info };
                    Ok(TokioIo::new(stream))
                };
                
                // For simplicity, we'll handle one stream per connection for now
                // In Phase 3, we'll add proper stream multiplexing
                Poll::Ready(Some(Ok(/* stream from future */)))
            }
            Some(Err(e)) => Poll::Ready(Some(Err(e))),
            None => Poll::Ready(None),
        }
    }
}
```

#### 1.4 URI Parsing
```rust
// Support URIs like: iroh://node-id or iroh://node-id@relay-url
fn parse_iroh_uri(uri: Uri) -> Result<NodeAddr> {
    if uri.scheme_str() != Some("iroh") {
        return Err(Error::invalid_uri("Expected iroh:// scheme"));
    }
    
    let host = uri.host().ok_or_else(|| Error::invalid_uri("Missing node ID"))?;
    let node_id = NodeId::from_str(host)?;
    
    // TODO: Parse relay URLs from URI fragments/query params
    Ok(NodeAddr::new(node_id))
}
```

#### 1.5 Tests
- [ ] Basic client connection test
- [ ] Basic server accept test  
- [ ] Round-trip gRPC call test
- [ ] Connection metadata propagation test
- [ ] Error handling tests

### Phase 2: Enhanced Transport Features (3-5 days)
**Goal**: Production-ready transport with connection management

#### 2.1 Connection Pooling
```rust
pub struct ConnectionPool {
    connections: Arc<RwLock<HashMap<NodeId, PooledConnection>>>,
    max_connections_per_peer: usize,
    idle_timeout: Duration,
}

struct PooledConnection {
    connection: iroh::endpoint::Connection,
    last_used: Instant,
    active_streams: AtomicUsize,
}

impl ConnectionPool {
    async fn get_or_connect(&self, target: NodeAddr) -> Result<iroh::endpoint::Connection> {
        // Try to reuse existing connection
        if let Some(conn) = self.get_existing(&target.node_id).await {
            return Ok(conn);
        }
        
        // Create new connection
        let conn = self.transport.connect_to_peer(target).await?;
        self.store_connection(conn.clone()).await;
        Ok(conn)
    }
}
```

#### 2.2 Stream Multiplexing
Handle multiple concurrent gRPC calls over single QUIC connection:

```rust
// Enhanced server incoming that handles stream multiplexing
impl IrohIncoming {
    fn handle_connection(&self, connection: iroh::endpoint::Connection) {
        tokio::spawn(async move {
            loop {
                match connection.accept_bi().await {
                    Ok((send, recv)) => {
                        let stream = IrohStream { send, recv, peer_info };
                        // Send stream to main incoming queue
                        self.stream_sender.send(Ok(TokioIo::new(stream))).await;
                    }
                    Err(_) => break, // Connection closed
                }
            }
        });
    }
}
```

#### 2.3 Connection Health & Recovery
```rust
pub struct ConnectionMonitor {
    health_check_interval: Duration,
    max_failures: usize,
}

impl ConnectionMonitor {
    async fn monitor_connection(&self, connection: &iroh::endpoint::Connection) {
        // Periodic ping/health checks
        // Automatic reconnection on failure
        // Circuit breaker pattern
    }
}
```

#### 2.4 Enhanced Error Handling
```rust
#[derive(Debug, thiserror::Error)]
pub enum TransportError {
    #[error("Connection failed: {0}")]
    ConnectionFailed(#[from] iroh::endpoint::ConnectionError),
    
    #[error("Peer unreachable: {node_id}")]
    PeerUnreachable { node_id: NodeId },
    
    #[error("Stream error: {0}")]
    StreamError(String),
    
    #[error("URI parse error: {0}")]
    InvalidUri(String),
}
```

### Phase 3: Multi-Transport Architecture (2-3 days)
**Goal**: Same services work on HTTP/2, iroh, WebSocket, etc.

#### 3.1 Transport-Agnostic Server
```rust
// Standard tonic server with multiple transports
async fn main() -> Result<()> {
    let service = MyServiceServer::new(MyService::new());
    
    // HTTP/2 transport for local clients
    let http_server = Server::builder()
        .add_service(service.clone())
        .serve("[::1]:8080".parse()?);
    
    // Iroh P2P transport
    let iroh_transport = IrohTransport::builder().build().await?;
    let iroh_incoming = IrohIncoming::new(iroh_transport);
    let p2p_server = Server::builder()
        .add_service(service)
        .serve_with_incoming(iroh_incoming);
    
    // Run both transports concurrently
    tokio::try_join!(http_server, p2p_server)?;
    Ok(())
}
```

#### 3.2 Enhanced Connection Info
```rust
#[derive(Debug, Clone)]
pub enum ConnectionInfo {
    Iroh(IrohPeerInfo),
    Http(SocketAddr),
    Unix(PathBuf),
    WebSocket(WsConnectionInfo),
}

// Available in service handlers via:
let conn_info = request.extensions().get::<ConnectionInfo>();
```

#### 3.3 Cross-Transport Examples
- [ ] Multi-transport chat server
- [ ] HTTP client → iroh server calls
- [ ] iroh client → HTTP server calls
- [ ] Service discovery across transports

### Phase 4: Middleware Architecture (1-2 weeks)
**Goal**: Rich, composable middleware system using tower layers

#### 4.1 Core Middleware Traits
```rust
// Use standard tower patterns
pub use tower::{Layer, Service, ServiceBuilder};

// Peer-aware middleware using connection info
pub struct PeerAwareLayer<L> {
    inner: L,
}

impl<L, S> Layer<S> for PeerAwareLayer<L> 
where 
    L: Layer<S>,
{
    type Service = PeerAwareService<L::Service>;
    
    fn layer(&self, service: S) -> Self::Service {
        PeerAwareService {
            inner: self.inner.layer(service),
        }
    }
}
```

#### 4.2 Essential Middlewares

**Rate Limiting**
```rust
pub struct RateLimitLayer {
    per_peer_limit: u64,
    per_connection_limit: u64,
    window: Duration,
}

impl RateLimitLayer {
    pub fn per_peer(requests_per_second: u64) -> Self { /* ... */ }
    pub fn per_connection(requests_per_second: u64) -> Self { /* ... */ }
}

impl<S> Service<Request<Body>> for RateLimitService<S> {
    async fn call(&mut self, req: Request<Body>) -> Result<Response<Body>, Error> {
        // Extract peer info from connection
        let peer_info = req.extensions().get::<IrohPeerInfo>();
        
        // Apply rate limiting based on peer
        if let Some(peer) = peer_info {
            self.limiter.check_rate_limit(peer.node_id).await?;
        }
        
        self.inner.call(req).await
    }
}
```

**Load Shedding**
```rust
pub struct LoadSheddingLayer {
    max_cpu_percent: f32,
    max_memory_bytes: usize,
    max_concurrent_requests: usize,
}

impl<S> Service<Request<Body>> for LoadSheddingService<S> {
    async fn call(&mut self, req: Request<Body>) -> Result<Response<Body>, Error> {
        // Check system resources
        let system_load = self.monitor.current_load().await;
        
        if system_load.cpu_percent > self.max_cpu_percent {
            return Err(Status::resource_exhausted("High CPU load").into());
        }
        
        self.inner.call(req).await
    }
}
```

**Compression**
```rust
pub struct CompressionLayer {
    algorithms: Vec<CompressionAlgorithm>,
    min_size: usize,
}

// Automatically compresses responses based on client capabilities
```

**Authentication**
```rust
pub trait AuthProvider: Send + Sync + 'static {
    async fn authenticate(&self, peer_info: &IrohPeerInfo) -> Result<AuthContext>;
}

pub struct AuthLayer<P> {
    provider: P,
}

impl<P: AuthProvider, S> Service<Request<Body>> for AuthService<P, S> {
    async fn call(&mut self, req: Request<Body>) -> Result<Response<Body>, Error> {
        let peer_info = req.extensions().get::<IrohPeerInfo>()
            .ok_or_else(|| Status::unauthenticated("No peer info"))?;
            
        let auth_ctx = self.provider.authenticate(peer_info).await?;
        req.extensions_mut().insert(auth_ctx);
        
        self.inner.call(req).await
    }
}
```

**Metrics & Tracing**
```rust
pub struct MetricsLayer {
    registry: Arc<MetricsRegistry>,
}

impl<S> Service<Request<Body>> for MetricsService<S> {
    async fn call(&mut self, req: Request<Body>) -> Result<Response<Body>, Error> {
        let start = Instant::now();
        let peer_info = req.extensions().get::<IrohPeerInfo>();
        
        // Create tracing span with peer info
        let span = tracing::info_span!(
            "grpc_request",
            peer_id = ?peer_info.map(|p| p.node_id),
            method = ?req.uri().path()
        );
        
        let result = self.inner.call(req).instrument(span).await;
        
        // Record metrics
        let duration = start.elapsed();
        self.registry.record_request_duration(duration, &result);
        
        result
    }
}
```

#### 4.3 Middleware Composition
```rust
// Easy composition using ServiceBuilder
let service = MyServiceServer::new(MyService::new());

let layered_service = ServiceBuilder::new()
    .layer(MetricsLayer::new(registry))
    .layer(AuthLayer::new(auth_provider))
    .layer(RateLimitLayer::per_peer(100))
    .layer(LoadSheddingLayer::new(95.0))
    .layer(CompressionLayer::gzip())
    .service(service);

let server = Server::builder()
    .add_service(layered_service)
    .serve_with_incoming(iroh_incoming)
    .await?;
```

#### 4.4 Anti-Byzantine Features
```rust
pub struct PeerReputationLayer {
    reputation_tracker: Arc<ReputationTracker>,
    min_reputation: f64,
}

pub struct ResourceLimitLayer {
    max_memory_per_peer: usize,
    max_connections_per_peer: usize,
    max_requests_per_second: u64,
}

// Automatic peer banning for misbehavior
pub struct BehaviorMonitorLayer {
    violation_tracker: Arc<ViolationTracker>,
    ban_threshold: usize,
}
```

### Phase 5: Multi-Protocol ALPN Support (3-5 days)
**Goal**: Multiple protocols over same QUIC connection

#### 5.1 Protocol Router
```rust
pub struct ProtocolRouter {
    protocols: HashMap<Vec<u8>, Box<dyn ProtocolHandler>>,
    default_handler: Option<Box<dyn ProtocolHandler>>,
}

pub trait ProtocolHandler: Send + Sync {
    async fn handle_connection(
        &self, 
        connection: iroh::endpoint::Connection
    ) -> Result<()>;
}

// Built-in gRPC handler
pub struct GrpcProtocolHandler {
    service: BoxCloneService<Request<Body>, Response<Body>, Error>,
}

impl ProtocolHandler for GrpcProtocolHandler {
    async fn handle_connection(&self, connection: iroh::endpoint::Connection) -> Result<()> {
        // Handle gRPC calls over this connection
        while let Ok((send, recv)) = connection.accept_bi().await {
            let stream = IrohStream { send, recv, peer_info };
            // Process gRPC call
        }
        Ok(())
    }
}
```

#### 5.2 Enhanced Transport Builder
```rust
impl IrohTransportBuilder {
    pub fn register_protocol<H>(
        mut self, 
        alpn: &[u8], 
        handler: H
    ) -> Self 
    where 
        H: ProtocolHandler + 'static,
    {
        self.protocol_router.register(alpn.to_vec(), Box::new(handler));
        self.alpns.push(alpn.to_vec());
        self
    }
}

// Usage
let transport = IrohTransport::builder()
    .register_protocol(b"/grpc/1.0", GrpcHandler::new(grpc_services))
    .register_protocol(b"/custom/sync/1.0", SyncHandler::new())
    .register_protocol(b"/metrics/1.0", MetricsHandler::new())
    .build()
    .await?;
```

#### 5.3 Protocol-Specific Middleware
```rust
// Different middleware stacks per protocol
let grpc_service = ServiceBuilder::new()
    .layer(AuthLayer::new(grpc_auth))
    .layer(RateLimitLayer::per_peer(100))
    .service(chat_service);

let metrics_service = ServiceBuilder::new()
    .layer(RateLimitLayer::per_peer(10)) // Lower limit for metrics
    .service(metrics_service);
```

### Phase 6: OpenTelemetry Integration (2-3 days)
**Goal**: Full observability for distributed debugging

#### 6.1 Distributed Tracing
```rust
pub struct TracingLayer {
    tracer: Arc<dyn Tracer + Send + Sync>,
}

impl<S> Service<Request<Body>> for TracingService<S> {
    async fn call(&mut self, req: Request<Body>) -> Result<Response<Body>, Error> {
        // Extract trace context from metadata or create new
        let span_context = extract_trace_context(&req);
        
        let span = self.tracer
            .span_builder(format!("grpc_{}", req.uri().path()))
            .with_parent_context(span_context)
            .with_attributes([
                ("peer.id", peer_info.node_id.to_string()),
                ("connection.type", format!("{:?}", peer_info.connection_type)),
            ])
            .start(&self.tracer);
            
        let result = self.inner.call(req).with_context(span.context()).await;
        
        // Record span status based on result
        match &result {
            Ok(_) => span.set_status(StatusCode::Ok),
            Err(e) => span.set_status(StatusCode::Error, e.to_string()),
        }
        
        result
    }
}
```

#### 6.2 Metrics Collection
```rust
pub struct MetricsCollector {
    // Connection metrics
    active_connections: IntGaugeVec,
    connection_duration: HistogramVec,
    
    // Request metrics  
    request_duration: HistogramVec,
    request_count: IntCounterVec,
    
    // Peer metrics
    peer_reputation: GaugeVec,
    bytes_sent_per_peer: IntCounterVec,
    bytes_received_per_peer: IntCounterVec,
}
```

#### 6.3 Health Check Integration
```rust
// Standard gRPC health checking with peer info
impl Health for HealthService {
    async fn check(&self, req: Request<HealthCheckRequest>) -> Result<Response<HealthCheckResponse>, Status> {
        let peer_info = req.extensions().get::<IrohPeerInfo>();
        
        // Check health including peer-specific factors
        let status = if self.is_healthy_for_peer(peer_info).await {
            health_check_response::ServingStatus::Serving
        } else {
            health_check_response::ServingStatus::NotServing
        };
        
        Ok(Response::new(HealthCheckResponse { status: status as i32 }))
    }
}
```

### Phase 7: iroh-gossip Integration (1-2 weeks)
**Goal**: Optional gossip capabilities alongside gRPC

#### 7.1 Unified Protocol Support
```rust
// Enhanced transport with gossip support
[features]
default = []
gossip = ["iroh-gossip"]

impl IrohTransportBuilder {
    #[cfg(feature = "gossip")]
    pub fn enable_gossip(mut self) -> Self {
        self.register_protocol(
            iroh_gossip::GOSSIP_ALPN, 
            GossipProtocolHandler::new()
        )
    }
}
```

#### 7.2 gRPC-Gossip Bridge
```rust
pub struct GrpcGossipBridge<S> {
    inner: S,
    gossip: Gossip,
    event_mapper: Box<dyn Fn(&Request<Body>) -> Option<Bytes> + Send + Sync>,
}

impl<S> Service<Request<Body>> for GrpcGossipBridge<S> 
where 
    S: Service<Request<Body>, Response = Response<Body>>,
{
    async fn call(&mut self, req: Request<Body>) -> Result<Response<Body>, S::Error> {
        // Process gRPC call
        let response = self.inner.call(req).await?;
        
        // Optionally broadcast event via gossip
        if let Some(event_data) = (self.event_mapper)(&req) {
            let _ = self.gossip.broadcast(event_data).await;
        }
        
        Ok(response)
    }
}
```

#### 7.3 Enhanced Service Discovery
```rust
#[cfg(feature = "gossip")]
pub struct GossipServiceDiscovery {
    gossip: Gossip,
    service_topic: TopicId,
    known_services: Arc<RwLock<HashMap<NodeId, Vec<ServiceInfo>>>>,
}

impl GossipServiceDiscovery {
    pub async fn advertise_grpc_service(&self, service_name: &str) -> Result<()> {
        let announcement = ServiceAnnouncement {
            node_id: self.node_id,
            service_name: service_name.to_string(),
            protocol: "grpc".to_string(),
            timestamp: Instant::now(),
        };
        
        self.gossip.broadcast(announcement.encode()).await
    }
    
    pub fn discover_services(&self) -> impl Stream<Item = ServiceAnnouncement> {
        // Stream of service announcements from gossip
    }
}
```

### Phase 8: Production Hardening (1-2 weeks)
**Goal**: Production-ready deployment features

#### 8.1 Configuration Management
```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IrohTransportConfig {
    pub secret_key: Option<String>,
    pub relay_urls: Vec<String>,
    pub bind_port: Option<u16>,
    pub connection_limits: ConnectionLimits,
    pub rate_limits: RateLimits,
    pub health_check: HealthCheckConfig,
}

impl IrohTransportConfig {
    pub fn from_file<P: AsRef<Path>>(path: P) -> Result<Self> {
        let content = std::fs::read_to_string(path)?;
        Ok(toml::from_str(&content)?)
    }
}
```

#### 8.2 Deployment Helpers
```rust
// Easy deployment patterns
pub struct IrohServerDeployment {
    config: IrohTransportConfig,
    services: Vec<BoxCloneService<Request<Body>, Response<Body>, Error>>,
    middleware_stack: ServiceBuilder<BoxLayer>,
}

impl IrohServerDeployment {
    pub async fn deploy(self) -> Result<()> {
        // Set up transport, middleware, services automatically
        // Handle graceful shutdown
        // Metrics export
        // Health checking
    }
}
```

#### 8.3 CLI Tools
```rust
// cargo install tonic-iroh-tools
// iroh-grpc-server --config server.toml
// iroh-grpc-client call MyService/MyMethod --data '{"field": "value"}'
```

## Testing Strategy

### Unit Tests
- [ ] IrohStream AsyncRead/AsyncWrite implementation
- [ ] Connection pooling and reuse
- [ ] Middleware composition and zero-cost verification
- [ ] Error handling and retry mechanisms
- [ ] ALPN protocol negotiation

### Integration Tests
- [ ] End-to-end gRPC calls over iroh
- [ ] Multi-transport server with concurrent clients
- [ ] Middleware behavior under load
- [ ] Connection metadata propagation
- [ ] Cross-transport service calls

### Performance Tests
- [ ] Latency benchmarks vs native tonic HTTP/2
- [ ] Throughput comparison with/without middleware
- [ ] Connection establishment time
- [ ] Memory usage under load
- [ ] CPU usage profiling

### Hostile Environment Tests
- [ ] Connection flooding attacks
- [ ] Large message DoS attacks
- [ ] Malformed data handling
- [ ] Peer reputation effectiveness
- [ ] Resource exhaustion recovery

## Success Criteria

### Functional Requirements
- ✅ Drop-in replacement for tonic HTTP/2 transport
- ✅ Same gRPC service runs on multiple transports
- ✅ Rich peer metadata in service handlers
- ✅ Composable middleware with zero-cost when unused
- ✅ Multi-protocol ALPN support

### Performance Requirements
- ✅ <5% latency overhead vs tonic HTTP/2
- ✅ >95% throughput vs raw iroh QUIC
- ✅ <500KB memory overhead for basic operation
- ✅ 10,000+ concurrent connections on modest hardware

### Usability Requirements
- ✅ Same patterns as tonic UDS example
- ✅ Clear migration path from HTTP-only services
- ✅ Rich middleware ecosystem
- ✅ Good error messages and debugging

## Migration from V1

### What to Keep
- [x] Error type definitions (mostly good)
- [x] Basic ALPN constants
- [x] Project structure and documentation

### What to Replace
- [ ] All HTTP serialization code → AsyncRead/AsyncWrite wrappers
- [ ] Custom service traits → Standard tonic Service pattern  
- [ ] Custom server builder → Standard tonic Server + serve_with_incoming
- [ ] Complex request handling → Simple stream delegation

### Breaking Changes
```rust
// V1 (wrong)
let server = IrohServerBuilder::new(transport)
    .add_service(service)
    .serve().await?;

// V2 (correct)
let incoming = IrohIncoming::new(transport);
let server = Server::builder()
    .add_service(service)
    .serve_with_incoming(incoming)
    .await?;
```

This revised plan leverages tonic's existing optimizations and follows established patterns while providing rich P2P capabilities and middleware support.
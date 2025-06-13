# tonic-iroh-example

This example demonstrates how to use `tonic-iroh-transport` to build peer-to-peer gRPC applications.

## Overview

The example implements a simple P2P chat application with the following features:

- **P2P Chat Service**: Send messages, subscribe to incoming messages, get chat history, and participate in bidirectional chat streams
- **Node Service**: Get node information, ping peers, and list connected peers
- **Full P2P networking**: Direct connections with relay fallback via iroh
- **gRPC compatibility**: Standard protobuf definitions and tonic-generated code

## Building

```bash
cargo build
```

## Usage

### Start a Server

```bash
# Start with a random node ID
cargo run --bin server

# Start with a specific secret key (hex-encoded)
cargo run --bin server -- --secret-key 0123456789abcdef...

# Start with custom relay URLs
cargo run --bin server -- --relay-url https://relay1.example.com --relay-url https://relay2.example.com

# Enable verbose logging
cargo run --bin server -- --verbose
```

The server will print its node ID and address when it starts. You'll need this information to connect clients.

### Connect a Client

```bash
# Ping a server
cargo run --bin client -- --target-node-id <NODE_ID> ping

# Send a message
cargo run --bin client -- --target-node-id <NODE_ID> send "Hello, world!"

# Subscribe to incoming messages
cargo run --bin client -- --target-node-id <NODE_ID> subscribe

# Get chat history
cargo run --bin client -- --target-node-id <NODE_ID> history --limit 20

# Start interactive chat
cargo run --bin client -- --target-node-id <NODE_ID> chat

# Get node information
cargo run --bin client -- --target-node-id <NODE_ID> node-info

# List connected peers
cargo run --bin client -- --target-node-id <NODE_ID> list-peers
```

### Connection with Direct Addresses

If you know the direct address of a peer (e.g., they're on the same local network), you can specify it:

```bash
cargo run --bin client -- \
  --target-node-id <NODE_ID> \
  --target-addresses 192.168.1.100:8080 \
  ping
```

## Architecture

The example showcases the ideal API design for `tonic-iroh-transport`:

### Server Side

```rust
// Create iroh transport
let transport = IrohTransport::builder()
    .secret_key_from_hex(&secret_key)?
    .add_relay_url(relay_url)
    .bind_port(port)
    .build()
    .await?;

// Build gRPC server with iroh transport
let server = IrohServerBuilder::new(transport)
    .add_service(P2pChatServiceServer::new(chat_service))
    .add_service(NodeServiceServer::new(node_service))
    .serve()
    .await?;
```

### Client Side

```rust
// Create iroh transport
let transport = IrohTransport::builder()
    .build()
    .await?;

// Connect to peer
let channel = IrohChannel::connect(transport, target_addr).await?;

// Use standard tonic clients
let mut chat_client = P2pChatServiceClient::new(channel.clone());
let mut node_client = NodeServiceClient::new(channel);
```

## Protocol Details

- **Transport**: QUIC over iroh p2p connections
- **gRPC**: Standard protobuf + gRPC semantics
- **Discovery**: Automatic peer discovery via iroh's DHT
- **NAT Traversal**: Hole-punching with relay fallback
- **Security**: TLS 1.3 encryption for all connections

## Testing Locally

1. Start a server in one terminal:
   ```bash
   RUST_LOG=debug cargo run --bin server -- --verbose
   ```

2. Note the node ID printed by the server

3. In another terminal, connect a client:
   ```bash
   RUST_LOG=debug cargo run --bin client -- --target-node-id <NODE_ID> ping --verbose
   ```

The client should successfully connect and receive a pong response, demonstrating the p2p gRPC connection.
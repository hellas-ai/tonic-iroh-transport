# P2P Chat Example

A peer-to-peer chat application demonstrating `tonic-iroh-transport` with multiple gRPC services over a single P2P connection.

## Features

- **Chat Service**: Send messages, subscribe to streams, view history
- **Node Service**: Ping peers, get node info, list connections
- **Multi-service**: Both services over the same P2P connection
- **Interactive CLI**: Real-time chat interface

## Usage

### Start Server

```bash
cargo run --bin server
```

Copy the printed node ID.

### Connect Client

```bash
# Ping the server
cargo run --bin client -- --target-node-id <NODE_ID> ping

# Send a message
cargo run --bin client -- --target-node-id <NODE_ID> send "Hello!"

# Subscribe to messages
cargo run --bin client -- --target-node-id <NODE_ID> subscribe

# Interactive chat
cargo run --bin client -- --target-node-id <NODE_ID> chat

# View chat history
cargo run --bin client -- --target-node-id <NODE_ID> history --limit 10
```

### Options

```bash
# Custom server options
cargo run --bin server -- --secret-key <HEX> --relay-url <URL> --verbose

# Direct address connection
cargo run --bin client -- \
  --target-node-id <NODE_ID> \
  --target-addresses 192.168.1.100:8080 \
  ping
```

## Architecture

```rust
// Server: Multiple services registered with TransportBuilder
let endpoint = iroh::Endpoint::builder().bind().await?;

let _guard = TransportBuilder::new(endpoint.clone())
    .add_rpc(P2pChatServiceServer::new(chat_service))
    .add_rpc(NodeServiceServer::new(node_service))
    .spawn()
    .await?;

// Client: Connect to each service using IrohConnect
use tonic_iroh_transport::IrohConnect;

let chat_channel =
    P2pChatServiceServer::<ChatServiceImpl>::connect(&endpoint, target.clone()).await?;
let node_channel =
    NodeServiceServer::<NodeServiceImpl>::connect(&endpoint, target).await?;

let mut chat_client = P2pChatServiceClient::new(chat_channel);
let mut node_client = NodeServiceClient::new(node_channel);
```

This demonstrates how multiple gRPC services can be hosted on the same endpoint, with automatic service routing via ALPN protocols.
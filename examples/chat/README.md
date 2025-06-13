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
// Server: Multiple services on one transport
let transport = IrohTransport::builder().build().await?;

IrohServerBuilder::new(transport)
    .add_service(P2pChatServiceServer::new(chat_service))
    .add_service(NodeServiceServer::new(node_service))
    .serve()
    .await?;

// Client: Multiple clients on one channel
let channel = IrohChannel::connect(transport, target).await?;
let mut chat_client = P2pChatServiceClient::new(channel.clone());
let mut node_client = NodeServiceClient::new(channel);
```

This demonstrates how multiple gRPC services can share a single P2P connection, with automatic service routing via ALPN protocols.
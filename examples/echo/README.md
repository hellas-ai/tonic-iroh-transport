# Echo Example

A simple echo service demonstrating basic `tonic-iroh-transport` usage with unary gRPC calls over P2P connections.

## Features

- **Unary RPC**: Simple request-response echo
- **P2P networking**: Direct peer connections with NAT traversal
- **Minimal setup**: Basic transport usage example

## Usage

### Start Server

```bash
cargo run --bin server
```

Copy the printed node ID.

### Connect Client

```bash
cargo run --bin client <NODE_ID>
```

## Code Structure

```rust
// Protocol definition (proto/echo.proto)
service Echo {
  rpc Echo(EchoRequest) returns (EchoResponse);
}

// Server setup
let endpoint = iroh::Endpoint::builder().bind().await?;
let (handler, incoming, alpn) = GrpcProtocolHandler::for_service::<EchoServer<EchoService>>();
let _router = iroh::protocol::Router::builder(endpoint)
    .accept(alpn, handler)
    .spawn();
Server::builder()
    .add_service(EchoServer::new(EchoService))
    .serve_with_incoming(incoming)
    .await?;

// Client usage
use tonic_iroh_transport::IrohConnect;

let endpoint = iroh::Endpoint::builder().bind().await?;
let channel = EchoServer::<EchoService>::connect(&endpoint, server_addr).await?;
let mut client = EchoClient::new(channel);
let response = client.echo(request).await?;
```

This example provides a minimal demonstration of tonic-iroh-transport, showing how standard gRPC patterns work seamlessly over P2P connections.
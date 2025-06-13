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

// Server implementation
let transport = IrohTransport::builder().build().await?;
IrohServerBuilder::new(transport)
    .add_service(EchoServer::new(echo_service))
    .serve()
    .await?;

// Client usage
let channel = IrohChannel::connect(transport, target).await?;
let mut client = EchoClient::new(channel);
let response = client.echo(request).await?;
```

This example provides a minimal demonstration of tonic-iroh-transport, showing how standard gRPC patterns work seamlessly over P2P connections.
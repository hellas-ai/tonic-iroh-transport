<div style="text-align: center;" align="center">

[![Crate][crate_img]][crate_link]
[![License][license_img]][license_file]
[![Documentation][docs_img]][docs_link]

</div>

# tonic-iroh-transport

A transport layer that enables [tonic](https://github.com/hyperium/tonic) gRPC services to run over [iroh](https://github.com/n0-computer/iroh) peer-to-peer connections.

## Features

- **P2P gRPC**: Run standard gRPC services over encrypted QUIC connections
- **NAT traversal**: Automatic hole-punching with relay fallback
- **Service multiplexing**: Multiple gRPC services over the same P2P connection
- **Type safety**: Full integration with tonic's generated clients and servers

## How it Works

```
gRPC Client ←→ tonic-iroh-transport ←→ gRPC Server
                       ↓
               iroh P2P Network
            (QUIC + NAT traversal)
```

The transport layer bridges HTTP/2 gRPC streams with QUIC streams, enabling standard tonic applications to communicate directly peer-to-peer without requiring a centralized server.

## Quick Start

This guide shows how to build a complete P2P gRPC service using the echo example.

### 1. Dependencies

Add to your `Cargo.toml`:

```toml
[dependencies]
tonic-iroh-transport = "0.0.3" # or path = ".." in a workspace
tonic = { version = "0.13", features = ["prost"] }
prost = "0.13"
tokio = { version = "1.0", features = ["macros", "rt-multi-thread"] }
iroh = { version = "0.91" }
anyhow = "1.0"
tracing = "0.1"
tracing-subscriber = "0.3"

[build-dependencies]
tonic-build = "0.13"
```

### 2. Protocol Definition

Create `proto/echo/v1/echo.proto` to define your gRPC service:

```protobuf
syntax = "proto3";
package echo.v1;

service EchoService {
  rpc Echo(EchoRequest) returns (EchoResponse);
}

message EchoRequest {
  string message = 1;
}

message EchoResponse {
  string message = 1;
  string peer_id = 2;
}
```

This defines a simple service with one RPC method that echoes messages back with the peer ID.

### 3. Build Configuration

Create `build.rs` to generate Rust code from your protobuf:

```rust
fn main() -> Result<(), Box<dyn std::error::Error>> {
    tonic_build::configure()
        .out_dir("src/pb")
        .include_file("mod.rs")
        .build_transport(false)  // We provide our own transport
        .build_client(true)
        .build_server(true)
        .compile_protos(&["proto/echo/v1/echo.proto"], &["proto"])?;

    println!("cargo:rerun-if-changed=proto/echo/v1/echo.proto");
    Ok(())
}
```

This generates client and server code while disabling tonic's built-in transport.

### 4. Service Implementation

Implement your gRPC service handler:

```rust
use pb::echo::v1::{
    echo_service_server::EchoService,
    EchoRequest, EchoResponse,
};
use tonic_iroh_transport::IrohContext;

#[derive(Clone)]
struct EchoServiceImpl;

#[tonic::async_trait]
impl EchoService for EchoServiceImpl {
    async fn echo(&self, request: Request<EchoRequest>) -> Result<Response<EchoResponse>, Status> {
        // Extract peer info from the P2P connection
        let context = request.extensions().get::<IrohContext>().cloned();
        let peer_id = context
            .map(|ctx| ctx.node_id.to_string())
            .unwrap_or_else(|| "unknown".to_string());

        let req = request.into_inner();

        Ok(Response::new(EchoResponse {
            message: req.message,
            peer_id,
        }))
    }
}
```

The `IrohContext` extension provides details about the P2P connection.

### 5. Server Setup

Set up the P2P server with iroh transport:

```rust
use tonic_iroh_transport::TransportBuilder;
use pb::echo::v1::echo_service_server::EchoServiceServer;

#[tokio::main]
async fn main() -> Result<()> {
    // Create iroh endpoint for P2P networking
    let endpoint = iroh::Endpoint::builder().bind().await?;
    println!("Server Node ID: {}", endpoint.id());

    // Start transport with the Echo service
    let _guard = TransportBuilder::new(endpoint)
        .add_rpc(EchoServiceServer::new(EchoServiceImpl))
        .spawn()
        .await?;

    Ok(())
}
```

This creates an iroh endpoint, registers the service with the transport, and starts the gRPC server over iroh.

### 6. Client Usage

Connect to the P2P service and make calls:

```rust
use tonic_iroh_transport::IrohConnect;
use pb::echo::v1::{
    echo_service_client::EchoServiceClient,
    echo_service_server::EchoServiceServer,
    EchoRequest,
};

// Create client endpoint
let client_endpoint = iroh::Endpoint::builder().bind().await?;
let server_addr = EndpointAddr::new(server_node_id);

// Connect using the IrohConnect trait
let channel = EchoServiceServer::<EchoServiceImpl>::connect(&client_endpoint, server_addr).await?;
let mut client = EchoServiceClient::new(channel);

// Make RPC calls
let request = Request::new(EchoRequest {
    message: "Hello, P2P gRPC!".to_string()
});

let response = client.echo(request).await?;
println!("Response: {}", response.into_inner().message);
```

The `IrohConnect` trait is automatically implemented for all tonic server types and handles protocol negotiation.

You can also configure connection options:

```rust
use std::time::Duration;

// With connection timeout
let channel = EchoServiceServer::<EchoServiceImpl>::connect(&client_endpoint, server_addr)
    .connect_timeout(Duration::from_secs(10))
    .await?;
```

## Examples

- [`echo`](examples/echo/) - Simple echo service
- [`chat`](examples/chat/) - P2P chat application with multiple services

## Performance

```
❯ ./benchmark.sh 5 32
=== P2P Chat Benchmark Test ===
Duration: 5s, Concurrency: 32

Building... OK
Starting receiver... OK
Running benchmark... OK
Shutting down receiver... OK

=== Results ===
  Duration: 5.00s
  Total messages: 204471
  Messages/sec: 40889.64
  Avg latency: 0.02ms

=== Resource Usage ===
  Receiver:         7.80 real         4.34 user        12.86 sys

Benchmark complete.
```

## Development

```bash
# Check all crates compile
make check-all

# Run all tests
make test-all

# Format and lint
make fmt
make lint

# Build examples
make examples
```

## License

MIT OR Apache-2.0

[crate_link]: https://crates.io/crates/tonic-iroh-transport "Crate listing"
[crate_img]: https://img.shields.io/crates/v/tonic-iroh-transport.svg?style=for-the-badge&color=f46623 "Crate badge"
[docs_link]: https://docs.rs/tonic-iroh-transport/latest/tonic-iroh-transport "Crate documentation"
[docs_img]: https://img.shields.io/docsrs/tonic-iroh-transport/latest.svg?style=for-the-badge "Documentation badge"
[license_file]: https://github.com/hellas-ai/tonic-iroh-transport/blob/master/LICENSE "Project license"
[license_img]: https://img.shields.io/crates/l/tonic-iroh-transport.svg?style=for-the-badge "License badge"

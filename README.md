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

Add to your `Cargo.toml`:

```toml
[dependencies]
tonic-iroh-transport = "1.0"
tonic = "0.13"
tokio = { version = "1.0", features = ["macros", "rt-multi-thread"] }
```

### Server

```rust
use tonic_iroh_transport::{IrohServerBuilder, IrohTransport};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let transport = IrohTransport::builder().build().await?;
    
    println!("Server Node ID: {}", transport.node_id());
    
    IrohServerBuilder::new(transport)
        .add_service(YourServiceServer::new(service_impl))
        .serve()
        .await?;
        
    Ok(())
}
```

### Client

```rust
use tonic_iroh_transport::{IrohChannel, IrohTransport};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let transport = IrohTransport::builder().build().await?;
    let target = iroh::NodeAddr::new(server_node_id);
    
    let channel = IrohChannel::connect(transport, target).await?;
    let mut client = YourServiceClient::new(channel);
    
    let response = client.your_method(request).await?;
    Ok(())
}
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

## How it Works

```
gRPC Client ←→ tonic-iroh-transport ←→ gRPC Server
                       ↓
               iroh P2P Network
            (QUIC + NAT traversal)
```

The transport layer bridges HTTP/2 gRPC streams with QUIC streams, enabling standard tonic applications to communicate directly peer-to-peer without requiring a centralized server.

## License

MIT OR Apache-2.0
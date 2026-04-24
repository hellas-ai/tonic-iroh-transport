use std::net::SocketAddr;
use std::time::Duration;
use tokio::time::{sleep, timeout, Instant};
use tonic_iroh_transport::iroh::{
    address_lookup::memory::MemoryLookup,
    endpoint::{presets, Connection},
    protocol::{AcceptError, ProtocolHandler, Router},
    Endpoint, EndpointAddr, EndpointId, TransportAddr,
};
#[cfg(feature = "discovery")]
use tonic_iroh_transport::swarm::{peers::Scope, ServiceRegistry, StaticBackend};
use tonic_iroh_transport::{
    ConnectionPool, ConnectionRef, IrohConnect, IrohStream, PoolOptions, TransportBuilder,
};

/// Create a local-only endpoint with relays and discovery disabled for testing.
async fn local_endpoint() -> Endpoint {
    Endpoint::builder(presets::Minimal).bind().await.unwrap()
}

async fn local_endpoint_with_lookup(address_lookup: MemoryLookup) -> Endpoint {
    Endpoint::builder(presets::Minimal)
        .address_lookup(address_lookup)
        .bind()
        .await
        .unwrap()
}

/// Convert bound socket addresses to localhost addresses for local testing.
/// `0.0.0.0:port` -> `127.0.0.1:port`, `[::]:port` -> `[::1]:port`
fn to_localhost_addrs(addrs: Vec<SocketAddr>) -> impl Iterator<Item = TransportAddr> {
    addrs.into_iter().map(|addr| {
        let local_addr = match addr {
            SocketAddr::V4(v4) if v4.ip().is_unspecified() => SocketAddr::new(
                std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
                v4.port(),
            ),
            SocketAddr::V6(v6) if v6.ip().is_unspecified() => SocketAddr::new(
                std::net::IpAddr::V6(std::net::Ipv6Addr::LOCALHOST),
                v6.port(),
            ),
            other => other,
        };
        TransportAddr::Ip(local_addr)
    })
}

fn endpoint_addr(endpoint: &Endpoint) -> EndpointAddr {
    EndpointAddr::new(endpoint.id()).with_addrs(to_localhost_addrs(endpoint.bound_sockets()))
}

async fn wait_for_replacement_connection(
    pool: &ConnectionPool,
    peer_id: EndpointId,
    previous_stable_id: usize,
    idle_timeout: Duration,
) -> ConnectionRef {
    let deadline = Instant::now() + Duration::from_secs(1);
    sleep(idle_timeout).await;

    loop {
        let conn = pool
            .get_or_connect(peer_id)
            .await
            .expect("reconnect after idle timeout failed");
        if conn.stable_id() != previous_stable_id {
            return conn;
        }

        assert!(
            Instant::now() < deadline,
            "idle timeout should retire the old connection within 1s"
        );
        drop(conn);
        sleep(Duration::from_millis(10)).await;
    }
}

#[cfg(feature = "discovery")]
async fn wait_for_shared_connection(
    pool: &ConnectionPool,
    peer_id: EndpointId,
    expected_stable_id: usize,
    idle_timeout: Duration,
) -> ConnectionRef {
    let deadline = Instant::now() + Duration::from_secs(1);
    sleep(idle_timeout).await;

    loop {
        let conn = pool
            .get_or_connect(peer_id)
            .await
            .expect("shared pool connection should remain alive");
        if conn.stable_id() == expected_stable_id {
            return conn;
        }

        assert!(
            Instant::now() < deadline,
            "locator should keep the shared pooled connection alive"
        );
        drop(conn);
        sleep(Duration::from_millis(10)).await;
    }
}

#[cfg(feature = "discovery")]
#[derive(Clone)]
struct TestRpc;

#[cfg(feature = "discovery")]
impl tonic::server::NamedService for TestRpc {
    const NAME: &'static str = "test.Service";
}

#[cfg(feature = "discovery")]
impl tower::Service<http::Request<tonic::body::Body>> for TestRpc {
    type Response = axum::response::Response<axum::body::Body>;
    type Error = std::convert::Infallible;
    type Future = std::future::Ready<Result<Self::Response, Self::Error>>;

    fn poll_ready(
        &mut self,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        std::task::Poll::Ready(Ok(()))
    }

    fn call(&mut self, _req: http::Request<tonic::body::Body>) -> Self::Future {
        std::future::ready(Ok(axum::response::Response::new(axum::body::Body::empty())))
    }
}

#[tokio::test]
async fn test_basic_iroh_connection() {
    let endpoint1 = local_endpoint().await;
    let endpoint2 = local_endpoint().await;

    let handler = TestProtocolHandler;
    let addrs2 = endpoint2.bound_sockets();
    let router2 = Router::builder(endpoint2.clone())
        .accept(b"/test/1.0", handler)
        .spawn();

    let node_addr2 = EndpointAddr::new(endpoint2.id()).with_addrs(to_localhost_addrs(addrs2));
    let conn = timeout(
        Duration::from_secs(5),
        endpoint1.connect(node_addr2, b"/test/1.0"),
    )
    .await
    .expect("connection timed out")
    .expect("connection failed");

    let (send, recv) = conn.open_bi().await.expect("open_bi failed");
    let context = tonic_iroh_transport::stream::IrohContext {
        node_id: endpoint2.id(),
        connection: conn.clone(),
        established_at: n0_future::time::Instant::now(),
        alpn: conn.alpn().to_vec(),
    };
    let _stream = IrohStream::new(send, recv, context);

    router2.shutdown().await.unwrap();
}

struct DummyService;
impl tonic::server::NamedService for DummyService {
    const NAME: &'static str = "test.Service";
}

#[tokio::test]
async fn test_iroh_connect_trait() {
    // Test that the IrohConnect trait is implemented for NamedService types
    let endpoint = local_endpoint().await;
    let addr = EndpointAddr::new(endpoint.id());
    // Just verify the builder can be created (don't actually connect)
    let _builder = DummyService::connect(&endpoint, addr);
}

#[test_log::test(tokio::test)]
async fn test_connection_timeout_external() {
    // Test using external tokio::time::timeout
    let client_endpoint = local_endpoint().await;

    let fake_node_id = EndpointId::from_bytes(&[0u8; 32]).unwrap();
    let addr = EndpointAddr::new(fake_node_id);

    let result = timeout(
        Duration::from_millis(500),
        DummyService::connect(&client_endpoint, addr),
    )
    .await;

    assert!(result.is_err() || result.unwrap().is_err());
}

#[tokio::test]
async fn test_rpc_server_lifecycle() {
    use axum::body::Body as AxumBody;
    use axum::response::Response;
    use std::convert::Infallible;
    use std::{future::Ready, task::Poll};
    use tonic::body::Body;

    #[derive(Clone)]
    struct DummyRpc;
    impl tonic::server::NamedService for DummyRpc {
        const NAME: &'static str = "test.Service";
    }
    impl tower::Service<http::Request<Body>> for DummyRpc {
        type Response = Response<AxumBody>;
        type Error = Infallible;
        type Future = Ready<Result<Self::Response, Self::Error>>;

        fn poll_ready(
            &mut self,
            _cx: &mut std::task::Context<'_>,
        ) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }

        fn call(&mut self, _req: http::Request<Body>) -> Self::Future {
            std::future::ready(Ok(Response::new(AxumBody::empty())))
        }
    }

    let endpoint = local_endpoint().await;
    let guard = TransportBuilder::new(endpoint.clone())
        .add_rpc(DummyRpc)
        .spawn()
        .await
        .unwrap();

    guard.shutdown().await.unwrap();
}

#[tokio::test]
async fn test_rpc_shutdown_closes_live_service_connection() {
    use axum::body::Body as AxumBody;
    use axum::response::Response;
    use std::convert::Infallible;
    use std::{future::Ready, task::Poll};
    use tonic::body::Body;

    #[derive(Clone)]
    struct DummyRpc;
    impl tonic::server::NamedService for DummyRpc {
        const NAME: &'static str = "test.Service";
    }
    impl tower::Service<http::Request<Body>> for DummyRpc {
        type Response = Response<AxumBody>;
        type Error = Infallible;
        type Future = Ready<Result<Self::Response, Self::Error>>;

        fn poll_ready(
            &mut self,
            _cx: &mut std::task::Context<'_>,
        ) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }

        fn call(&mut self, _req: http::Request<Body>) -> Self::Future {
            std::future::ready(Ok(Response::new(AxumBody::empty())))
        }
    }

    let server_endpoint = local_endpoint().await;
    let guard = TransportBuilder::new(server_endpoint.clone())
        .add_rpc(DummyRpc)
        .spawn()
        .await
        .unwrap();

    let client_endpoint = local_endpoint().await;
    let server_addr = EndpointAddr::new(server_endpoint.id())
        .with_addrs(to_localhost_addrs(server_endpoint.bound_sockets()));
    let conn = timeout(
        Duration::from_secs(5),
        client_endpoint.connect(server_addr, b"/test.Service/1.0"),
    )
    .await
    .expect("connection timed out")
    .expect("connection failed");

    timeout(Duration::from_secs(5), guard.shutdown())
        .await
        .expect("shutdown timed out")
        .expect("shutdown failed");

    let open_result = timeout(Duration::from_secs(1), conn.open_bi()).await;
    assert!(
        matches!(open_result, Ok(Err(_)) | Err(_)),
        "connection remained usable after server shutdown"
    );
}

#[test_log::test(tokio::test)]
async fn test_connect_timeout_builtin() {
    // Test using the built-in connect_timeout on ConnectBuilder
    let client_endpoint = local_endpoint().await;

    let fake_node_id = EndpointId::from_bytes(&[0u8; 32]).unwrap();
    let addr = EndpointAddr::new(fake_node_id);

    let result = DummyService::connect(&client_endpoint, addr)
        .connect_timeout(Duration::from_millis(100))
        .await;

    // Should fail - either with timeout or connection error
    assert!(result.is_err());
}

#[tokio::test]
async fn test_connection_pool_reuses_and_reconnects_after_idle() {
    let idle_timeout = Duration::from_millis(50);
    let address_lookup = MemoryLookup::new();
    let client_endpoint = local_endpoint_with_lookup(address_lookup.clone()).await;
    let server_endpoint = local_endpoint().await;
    let router = Router::builder(server_endpoint.clone())
        .accept(b"/test/1.0", TestProtocolHandler)
        .spawn();

    address_lookup.add_endpoint_info(endpoint_addr(&server_endpoint));

    let pool = ConnectionPool::new(
        client_endpoint,
        b"/test/1.0",
        PoolOptions {
            idle_timeout,
            connect_timeout: Duration::from_secs(5),
            max_connections: 8,
        },
    );

    let conn1 = timeout(
        Duration::from_secs(5),
        pool.get_or_connect(server_endpoint.id()),
    )
    .await
    .expect("pooled connect timed out")
    .expect("pooled connect failed");
    let stable_id = conn1.stable_id();

    let conn2 = pool
        .get_or_connect(server_endpoint.id())
        .await
        .expect("second pooled connect failed");
    assert_eq!(
        stable_id,
        conn2.stable_id(),
        "pool should reuse the existing connection while it is active"
    );

    drop(conn1);
    drop(conn2);

    let conn3 =
        wait_for_replacement_connection(&pool, server_endpoint.id(), stable_id, idle_timeout).await;
    assert_ne!(
        stable_id,
        conn3.stable_id(),
        "idle timeout should retire the old connection"
    );

    drop(conn3);
    router.shutdown().await.unwrap();
}

#[tokio::test]
async fn test_connection_ref_survives_pool_drop() {
    let address_lookup = MemoryLookup::new();
    let client_endpoint = local_endpoint_with_lookup(address_lookup.clone()).await;
    let server_endpoint = local_endpoint().await;
    let router = Router::builder(server_endpoint.clone())
        .accept(b"/test/1.0", TestProtocolHandler)
        .spawn();

    address_lookup.add_endpoint_info(endpoint_addr(&server_endpoint));

    let pool = ConnectionPool::new(
        client_endpoint,
        b"/test/1.0",
        PoolOptions {
            idle_timeout: Duration::from_secs(5),
            connect_timeout: Duration::from_secs(5),
            max_connections: 8,
        },
    );

    let conn = pool
        .get_or_connect(server_endpoint.id())
        .await
        .expect("initial pooled connection failed");
    let stable_id = conn.stable_id();

    drop(pool);

    let (_send, _recv) = timeout(Duration::from_secs(5), conn.open_bi())
        .await
        .expect("opening stream after pool drop timed out")
        .expect("leased connection should remain usable after pool drop");
    assert_eq!(
        stable_id,
        conn.stable_id(),
        "dropping the pool should not replace the leased connection"
    );

    drop(conn);
    router.shutdown().await.unwrap();
}

#[cfg(feature = "discovery")]
#[tokio::test]
async fn test_service_registry_locator_uses_shared_pool() {
    let idle_timeout = Duration::from_millis(50);
    let address_lookup = MemoryLookup::new();
    let client_endpoint = local_endpoint_with_lookup(address_lookup.clone()).await;
    let server_endpoint = local_endpoint().await;
    let guard = TransportBuilder::new(server_endpoint.clone())
        .add_rpc(TestRpc)
        .spawn()
        .await
        .unwrap();

    address_lookup.add_endpoint_info(endpoint_addr(&server_endpoint));

    let mut registry = ServiceRegistry::new(&client_endpoint);
    registry.with_pool_options(PoolOptions {
        idle_timeout,
        connect_timeout: Duration::from_secs(2),
        max_connections: 8,
    });
    registry.add(StaticBackend::new().add_peers(Scope::Any, 10, 255, [server_endpoint.id()]));

    let pool = registry.pool::<TestRpc>();
    let conn1 = pool
        .get_or_connect(server_endpoint.id())
        .await
        .expect("initial registry pool connect failed");
    let stable_id = conn1.stable_id();
    drop(conn1);

    let _channel = registry
        .find::<TestRpc>()
        .first()
        .await
        .expect("locator should connect through the shared pool");

    let conn2 =
        wait_for_shared_connection(&pool, server_endpoint.id(), stable_id, idle_timeout).await;
    assert_eq!(
        stable_id,
        conn2.stable_id(),
        "locator should keep the shared pooled connection alive"
    );

    drop(conn2);
    guard.shutdown().await.unwrap();
}

#[derive(Clone, Debug)]
struct TestProtocolHandler;

impl ProtocolHandler for TestProtocolHandler {
    async fn accept(&self, _connection: Connection) -> Result<(), AcceptError> {
        Ok(())
    }
}

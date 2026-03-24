//! Optional OpenTelemetry trace context propagation for tonic gRPC.
//!
//! Provides a client-side interceptor and a server-side tower layer for
//! propagating W3C `traceparent`/`tracestate` headers across iroh P2P calls.
//!
//! # Client-side injection
//!
//! ```rust,no_run
//! use tonic_iroh_transport::otel::TraceContextInjector;
//!
//! // Wrap a client with the injector so outgoing calls carry the current trace context
//! // let client = MyServiceClient::with_interceptor(channel, TraceContextInjector);
//! ```
//!
//! # Server-side span parenting
//!
//! ```rust,no_run
//! use tonic_iroh_transport::otel::TraceContextLayer;
//!
//! // Wrap a server so incoming requests run inside a span parented to the caller's trace
//! // let service = TraceContextLayer.layer(my_server);
//! ```

use opentelemetry::propagation::Injector;
use tonic::{Request, Status};

/// Client-side interceptor that injects the current OpenTelemetry trace context
/// into outgoing gRPC metadata as W3C `traceparent`/`tracestate` headers.
///
/// Requires a [`TextMapPropagator`](opentelemetry::propagation::TextMapPropagator)
/// to be registered via [`opentelemetry::global::set_text_map_propagator`].
#[derive(Clone, Copy, Debug, Default)]
pub struct TraceContextInjector;

impl tonic::service::Interceptor for TraceContextInjector {
    fn call(&mut self, mut request: Request<()>) -> Result<Request<()>, Status> {
        let cx = opentelemetry::Context::current();
        opentelemetry::global::get_text_map_propagator(|propagator| {
            propagator.inject_context(&cx, &mut MetadataInjector(request.metadata_mut()));
        });
        Ok(request)
    }
}

/// [`Injector`] adapter for [`tonic::metadata::MetadataMap`].
struct MetadataInjector<'a>(&'a mut tonic::metadata::MetadataMap);

impl Injector for MetadataInjector<'_> {
    fn set(&mut self, key: &str, value: String) {
        if let Ok(key) = tonic::metadata::MetadataKey::from_bytes(key.as_bytes()) {
            if let Ok(val) = tonic::metadata::MetadataValue::try_from(&value) {
                self.0.insert(key, val);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Tower layer for automatic server-side span parenting
// ---------------------------------------------------------------------------

#[cfg(feature = "server")]
mod layer {
    use std::future::Future;
    use std::pin::Pin;
    use std::task::{Context, Poll};

    use opentelemetry::propagation::Extractor;
    use tracing::Instrument;
    use tracing_opentelemetry::OpenTelemetrySpanExt;

    /// A tower [`Layer`](tower::Layer) that extracts W3C trace context from
    /// incoming gRPC requests and runs the inner service inside a tracing span
    /// parented to the caller's trace.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// use tonic_iroh_transport::otel::TraceContextLayer;
    ///
    /// let transport = TransportBuilder::new(endpoint)
    ///     .add_rpc(TraceContextLayer.layer(MyServer::new(my_impl)))
    ///     .spawn()
    ///     .await?;
    /// ```
    #[derive(Clone, Copy, Debug, Default)]
    pub struct TraceContextLayer;

    impl TraceContextLayer {
        /// Wrap a service so incoming requests run inside a span parented to the
        /// caller's trace context.
        pub fn layer<S>(&self, inner: S) -> TraceContextService<S> {
            TraceContextService { inner }
        }
    }

    impl<S> tower::Layer<S> for TraceContextLayer {
        type Service = TraceContextService<S>;

        fn layer(&self, inner: S) -> Self::Service {
            TraceContextService { inner }
        }
    }

    /// Tower service created by [`TraceContextLayer`].
    ///
    /// Extracts W3C trace context from HTTP headers and instruments the inner
    /// service call with a span parented to the extracted context.
    #[derive(Clone, Debug)]
    pub struct TraceContextService<S> {
        inner: S,
    }

    impl<S: tonic::server::NamedService> tonic::server::NamedService for TraceContextService<S> {
        const NAME: &'static str = S::NAME;
    }

    impl<S, B> tower::Service<http::Request<B>> for TraceContextService<S>
    where
        S: tower::Service<http::Request<B>> + Clone + Send + 'static,
        S::Future: Send + 'static,
        B: Send + 'static,
    {
        type Response = S::Response;
        type Error = S::Error;
        type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

        fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
            self.inner.poll_ready(cx)
        }

        fn call(&mut self, request: http::Request<B>) -> Self::Future {
            let parent_cx = opentelemetry::global::get_text_map_propagator(|propagator| {
                propagator.extract(&HeaderExtractor(request.headers()))
            });

            let span = tracing::info_span!(
                "grpc.server",
                otel.kind = "server",
                rpc.method = request.uri().path(),
            );
            let _ = span.set_parent(parent_cx);

            let clone = self.inner.clone();
            let mut inner = std::mem::replace(&mut self.inner, clone);

            Box::pin(inner.call(request).instrument(span))
        }
    }

    /// [`Extractor`] adapter for [`http::HeaderMap`].
    struct HeaderExtractor<'a>(&'a http::HeaderMap);

    impl Extractor for HeaderExtractor<'_> {
        fn get(&self, key: &str) -> Option<&str> {
            self.0.get(key).and_then(|v| v.to_str().ok())
        }

        fn keys(&self) -> Vec<&str> {
            self.0.keys().map(http::HeaderName::as_str).collect()
        }
    }
}

#[cfg(feature = "server")]
pub use layer::{TraceContextLayer, TraceContextService};

#[cfg(test)]
mod tests {
    use super::*;
    use opentelemetry::propagation::Injector;
    use tonic::metadata::MetadataMap;

    #[test]
    fn injector_sets_traceparent_header() {
        let mut metadata = MetadataMap::new();
        let mut injector = MetadataInjector(&mut metadata);
        injector.set(
            "traceparent",
            "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01".into(),
        );

        assert_eq!(
            metadata.get("traceparent").unwrap().to_str().unwrap(),
            "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01"
        );
    }

    #[test]
    fn injector_round_trip_via_interceptor() {
        use opentelemetry::propagation::{Extractor, TextMapPropagator};
        use opentelemetry_sdk::propagation::TraceContextPropagator;

        let propagator = TraceContextPropagator::new();
        let traceparent = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";

        // Simulate: traceparent → metadata → extract context → re-inject → verify
        let mut src = MetadataMap::new();
        src.insert("traceparent", traceparent.parse().unwrap());

        struct MapExtractor<'a>(&'a MetadataMap);
        impl Extractor for MapExtractor<'_> {
            fn get(&self, key: &str) -> Option<&str> {
                self.0.get(key).and_then(|v| v.to_str().ok())
            }
            fn keys(&self) -> Vec<&str> {
                self.0
                    .keys()
                    .filter_map(|k| match k {
                        tonic::metadata::KeyRef::Ascii(k) => Some(k.as_str()),
                        tonic::metadata::KeyRef::Binary(_) => None,
                    })
                    .collect()
            }
        }

        let cx = propagator.extract(&MapExtractor(&src));

        let mut dst = MetadataMap::new();
        propagator.inject_context(&cx, &mut MetadataInjector(&mut dst));

        assert_eq!(
            dst.get("traceparent").unwrap().to_str().unwrap(),
            traceparent
        );
    }
}

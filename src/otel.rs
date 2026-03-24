//! Optional OpenTelemetry trace context propagation for tonic gRPC.
//!
//! Provides tonic interceptors that inject/extract W3C `traceparent`/`tracestate`
//! headers into gRPC metadata, enabling distributed tracing across iroh P2P calls.
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
//! # Server-side extraction
//!
//! ```rust,no_run
//! use tonic::service::interceptor::InterceptedService;
//! use tonic_iroh_transport::otel::TraceContextExtractor;
//!
//! // Wrap a server so incoming trace context is attached to request extensions
//! // let service = InterceptedService::new(my_server, TraceContextExtractor);
//! ```

use opentelemetry::propagation::{Extractor, Injector};
use tonic::metadata::MetadataMap;
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

/// Server-side interceptor that extracts W3C trace context from incoming gRPC
/// metadata and stores it in request extensions as an [`opentelemetry::Context`].
///
/// Downstream handlers can retrieve the context with:
/// ```rust,ignore
/// let parent_cx = request.extensions().get::<opentelemetry::Context>();
/// ```
///
/// Requires a [`TextMapPropagator`](opentelemetry::propagation::TextMapPropagator)
/// to be registered via [`opentelemetry::global::set_text_map_propagator`].
#[derive(Clone, Copy, Debug, Default)]
pub struct TraceContextExtractor;

impl tonic::service::Interceptor for TraceContextExtractor {
    fn call(&mut self, mut request: Request<()>) -> Result<Request<()>, Status> {
        let cx = opentelemetry::global::get_text_map_propagator(|propagator| {
            propagator.extract(&MetadataExtractorRef(request.metadata()))
        });
        request.extensions_mut().insert(cx);
        Ok(request)
    }
}

/// [`Injector`] adapter for [`tonic::metadata::MetadataMap`].
///
/// Use this directly if you need custom propagation logic beyond the
/// provided interceptors.
pub struct MetadataInjector<'a>(
    /// The metadata map to inject headers into.
    pub &'a mut MetadataMap,
);

impl Injector for MetadataInjector<'_> {
    fn set(&mut self, key: &str, value: String) {
        if let Ok(key) = tonic::metadata::MetadataKey::from_bytes(key.as_bytes()) {
            if let Ok(val) = tonic::metadata::MetadataValue::try_from(&value) {
                self.0.insert(key, val);
            }
        }
    }
}

/// [`Extractor`] adapter for [`tonic::metadata::MetadataMap`].
///
/// Use this directly if you need custom propagation logic beyond the
/// provided interceptors.
pub struct MetadataExtractorRef<'a>(
    /// The metadata map to extract headers from.
    pub &'a MetadataMap,
);

impl Extractor for MetadataExtractorRef<'_> {
    fn get(&self, key: &str) -> Option<&str> {
        self.0.get(key).and_then(|v| v.to_str().ok())
    }

    fn keys(&self) -> Vec<&str> {
        self.0
            .keys()
            .filter_map(|k| match k {
                tonic::metadata::KeyRef::Ascii(key) => Some(key.as_str()),
                tonic::metadata::KeyRef::Binary(_) => None,
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use opentelemetry::propagation::TextMapPropagator;
    use opentelemetry_sdk::propagation::TraceContextPropagator;
    use tonic::service::Interceptor;

    #[test]
    fn injector_sets_traceparent_header() {
        let mut metadata = MetadataMap::new();

        // Inject a known context with a valid traceparent
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
    fn extractor_reads_traceparent_header() {
        let mut metadata = MetadataMap::new();
        metadata.insert(
            "traceparent",
            "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01"
                .parse()
                .unwrap(),
        );

        let extractor = MetadataExtractorRef(&metadata);
        assert_eq!(
            extractor.get("traceparent").unwrap(),
            "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01"
        );
        assert!(extractor.keys().contains(&"traceparent"));
    }

    #[test]
    fn round_trip_inject_extract() {
        let propagator = TraceContextPropagator::new();

        // Inject a context into metadata
        let mut metadata = MetadataMap::new();
        let traceparent = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";
        let mut inject_map = MetadataMap::new();
        inject_map.insert("traceparent", traceparent.parse().unwrap());

        // Extract a context from the injected metadata
        let extracted_cx = propagator.extract(&MetadataExtractorRef(&inject_map));

        // Re-inject the extracted context into fresh metadata
        propagator.inject_context(&extracted_cx, &mut MetadataInjector(&mut metadata));

        // The traceparent should survive the round trip
        assert_eq!(
            metadata.get("traceparent").unwrap().to_str().unwrap(),
            traceparent
        );
    }

    #[test]
    fn interceptor_inject_extract_round_trip() {
        let propagator = TraceContextPropagator::new();
        opentelemetry::global::set_text_map_propagator(propagator);

        // Simulate client-side injection
        let mut client_request = Request::new(());
        let traceparent = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";
        client_request
            .metadata_mut()
            .insert("traceparent", traceparent.parse().unwrap());

        // Extract on server side
        let mut extractor = TraceContextExtractor;
        let server_request = extractor.call(client_request).unwrap();

        // Verify the OTel context was stored in extensions
        let cx = server_request
            .extensions()
            .get::<opentelemetry::Context>()
            .expect("context should be in extensions");

        // Re-inject from the extracted context
        let mut metadata = MetadataMap::new();
        opentelemetry::global::get_text_map_propagator(|p| {
            p.inject_context(cx, &mut MetadataInjector(&mut metadata));
        });

        assert_eq!(
            metadata.get("traceparent").unwrap().to_str().unwrap(),
            traceparent
        );
    }
}

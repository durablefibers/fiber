//! Optional OpenTelemetry via OTLP. Enabled when `OTEL_EXPORTER_OTLP_ENDPOINT` is set
//! (or `FIBER_OTEL_ENDPOINT` as an alias). Uses HTTP/protobuf to the collector.

use anyhow::{Context, Result};
use opentelemetry::trace::TracerProvider as _;
use opentelemetry::{KeyValue, global};
use opentelemetry_otlp::{MetricExporter, Protocol, SpanExporter, WithExportConfig};
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::metrics::SdkMeterProvider;
use opentelemetry_sdk::propagation::TraceContextPropagator;
use opentelemetry_sdk::trace::SdkTracerProvider;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

pub struct OtelGuard {
    tracer_provider: Option<SdkTracerProvider>,
    meter_provider: Option<SdkMeterProvider>,
}

impl Drop for OtelGuard {
    fn drop(&mut self) {
        if let Some(p) = self.tracer_provider.take() {
            let _ = p.shutdown();
        }
        if let Some(p) = self.meter_provider.take() {
            let _ = p.shutdown();
        }
    }
}

fn otlp_endpoint() -> Option<String> {
    std::env::var("OTEL_EXPORTER_OTLP_ENDPOINT")
        .or_else(|_| std::env::var("FIBER_OTEL_ENDPOINT"))
        .ok()
        .filter(|s| !s.trim().is_empty())
}

/// OTLP over HTTP wants a per-signal URL, while `OTEL_EXPORTER_OTLP_ENDPOINT` is defined as
/// a base — the SDK's `with_endpoint` takes the former, so the path has to be joined here.
/// Without it every export lands on `/` and a collector answers 404.
///
/// A base that already names the signal is left alone, so a full URL also works.
fn signal_endpoint(base: &str, signal: &str) -> String {
    let base = base.trim_end_matches('/');
    if base.ends_with(signal) {
        base.to_string()
    } else {
        format!("{base}/{signal}")
    }
}

fn resource() -> Resource {
    Resource::builder()
        .with_service_name("fiber-api")
        .with_attribute(KeyValue::new("service.version", env!("CARGO_PKG_VERSION")))
        .build()
}

/// Install fmt tracing always; add OTLP traces + metrics when an endpoint env var is set.
pub fn init() -> Result<OtelGuard> {
    // Only a default: `add_directive` on top of the env filter would override what
    // RUST_LOG says about this crate, so `RUST_LOG=fiber_api=debug` did nothing.
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info,fiber_api=info"));

    let Some(endpoint) = otlp_endpoint() else {
        tracing_subscriber::fmt().with_env_filter(filter).init();
        return Ok(OtelGuard {
            tracer_provider: None,
            meter_provider: None,
        });
    };

    global::set_text_map_propagator(TraceContextPropagator::new());
    let resource = resource();

    let span_exporter = SpanExporter::builder()
        .with_http()
        .with_protocol(Protocol::HttpBinary)
        .with_endpoint(signal_endpoint(&endpoint, "v1/traces"))
        .build()
        .context("OTLP span exporter")?;

    let tracer_provider = SdkTracerProvider::builder()
        .with_batch_exporter(span_exporter)
        .with_resource(resource.clone())
        .build();

    let metric_exporter = MetricExporter::builder()
        .with_http()
        .with_protocol(Protocol::HttpBinary)
        .with_endpoint(signal_endpoint(&endpoint, "v1/metrics"))
        .build()
        .context("OTLP metric exporter")?;

    let meter_provider = SdkMeterProvider::builder()
        .with_periodic_exporter(metric_exporter)
        .with_resource(resource)
        .build();

    global::set_tracer_provider(tracer_provider.clone());
    global::set_meter_provider(meter_provider.clone());

    let tracer = tracer_provider.tracer("fiber-api");
    let telemetry = tracing_opentelemetry::layer().with_tracer(tracer);

    tracing_subscriber::registry()
        .with(filter)
        .with(tracing_subscriber::fmt::layer())
        .with(telemetry)
        .init();

    // Warm a counter so the meter is visible in backends even before first traffic.
    let meter = global::meter("fiber-api");
    let _ = meter.u64_counter("fiber.api.boot").build();

    tracing::info!(%endpoint, "OpenTelemetry OTLP export enabled");
    Ok(OtelGuard {
        tracer_provider: Some(tracer_provider),
        meter_provider: Some(meter_provider),
    })
}

#[cfg(test)]
mod tests {
    use super::signal_endpoint;

    #[test]
    fn a_base_endpoint_gains_the_signal_path() {
        assert_eq!(
            signal_endpoint("http://collector:4318", "v1/traces"),
            "http://collector:4318/v1/traces"
        );
        // A trailing slash must not produce a doubled one.
        assert_eq!(
            signal_endpoint("http://collector:4318/", "v1/metrics"),
            "http://collector:4318/v1/metrics"
        );
        // Someone who already gave the full signal URL keeps it.
        assert_eq!(
            signal_endpoint("http://collector:4318/v1/traces", "v1/traces"),
            "http://collector:4318/v1/traces"
        );
        // A collector behind a path prefix keeps the prefix.
        assert_eq!(
            signal_endpoint("http://gw/otlp", "v1/traces"),
            "http://gw/otlp/v1/traces"
        );
    }
}

//! Optional OpenTelemetry via OTLP, matching `fiber-api`: enabled when
//! `OTEL_EXPORTER_OTLP_ENDPOINT` is set (or `FIBER_OTEL_ENDPOINT` as an alias), over
//! HTTP/protobuf.
//!
//! The agent is where steps actually run, so this is where step duration and outcome come
//! from. Traces are not yet joined to the API's: the offer carries no trace context, so an
//! agent span is a root rather than a child of the run that produced it.

use anyhow::{Context, Result};
use opentelemetry::metrics::{Counter, Histogram};
use opentelemetry::trace::TracerProvider as _;
use opentelemetry::{KeyValue, global};
use opentelemetry_otlp::{MetricExporter, Protocol, SpanExporter, WithExportConfig};
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::metrics::SdkMeterProvider;
use opentelemetry_sdk::propagation::TraceContextPropagator;
use opentelemetry_sdk::trace::SdkTracerProvider;
use std::sync::OnceLock;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

pub struct OtelGuard {
    tracer_provider: Option<SdkTracerProvider>,
    meter_provider: Option<SdkMeterProvider>,
}

impl Drop for OtelGuard {
    fn drop(&mut self) {
        // Flush on the way out: a step that finished during shutdown is exactly the one
        // worth having in the backend.
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

/// Install fmt logging always; add OTLP traces + metrics when an endpoint is configured.
pub fn init(agent_name: &str) -> Result<OtelGuard> {
    let filter = EnvFilter::from_default_env().add_directive("fiber_agent=info".parse()?);

    let Some(endpoint) = otlp_endpoint() else {
        tracing_subscriber::fmt().with_env_filter(filter).init();
        return Ok(OtelGuard {
            tracer_provider: None,
            meter_provider: None,
        });
    };

    global::set_text_map_propagator(TraceContextPropagator::new());
    // `service.instance.id` separates a fleet: every agent reports the same service name,
    // and "which worker is slow" is the question this data gets asked.
    let resource = Resource::builder()
        .with_service_name("fiber-agent")
        .with_attribute(KeyValue::new("service.version", env!("CARGO_PKG_VERSION")))
        .with_attribute(KeyValue::new("service.instance.id", agent_name.to_string()))
        .build();

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

    let tracer = tracer_provider.tracer("fiber-agent");
    tracing_subscriber::registry()
        .with(filter)
        .with(tracing_subscriber::fmt::layer())
        .with(tracing_opentelemetry::layer().with_tracer(tracer))
        .init();

    tracing::info!(%endpoint, "OpenTelemetry OTLP export enabled");
    Ok(OtelGuard {
        tracer_provider: Some(tracer_provider),
        meter_provider: Some(meter_provider),
    })
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

struct StepInstruments {
    steps: Counter<u64>,
    duration: Histogram<f64>,
}

/// Built once. Creating an instrument per step would allocate on every execution and give
/// the SDK a new stream to reconcile each time.
fn instruments() -> &'static StepInstruments {
    static I: OnceLock<StepInstruments> = OnceLock::new();
    I.get_or_init(|| {
        let meter = global::meter("fiber-agent");
        StepInstruments {
            steps: meter
                .u64_counter("fiber.agent.steps")
                .with_description("Steps this agent finished, by outcome.")
                .build(),
            duration: meter
                .f64_histogram("fiber.agent.step.duration")
                .with_description(
                    "Wall-clock seconds from offer to completion, workspace preparation and \
                     artifact transfer included.",
                )
                .with_unit("s")
                .build(),
        }
    })
}

/// Outcome label for a finished step. Mirrors how the session reports `StepComplete`, so a
/// dashboard and the run page agree on what happened.
pub fn outcome_label(result: &Result<i32>) -> &'static str {
    match result {
        Ok(0) => "succeeded",
        Ok(_) => "failed",
        Err(e) if e.to_string().contains("cancelled") => "cancelled",
        Err(e) if e.to_string().starts_with("timed out") => "timed_out",
        Err(_) => "error",
    }
}

/// Record one finished step. A no-op when OTLP is off: the global meter is then a noop
/// provider, so this costs an atomic load and nothing else.
pub fn record_step(result: &Result<i32>, seconds: f64, containerised: bool) {
    let attrs = [
        KeyValue::new("outcome", outcome_label(result)),
        KeyValue::new("kind", if containerised { "docker" } else { "shell" }),
    ];
    let i = instruments();
    i.steps.add(1, &attrs);
    i.duration.record(seconds, &attrs);
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

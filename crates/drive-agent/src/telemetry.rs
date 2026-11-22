//! Where the agent's spans go: stderr always, and an OTLP collector when
//! one is named (`OTEL_EXPORTER_OTLP_ENDPOINT`, or
//! `OTEL_EXPORTER_OTLP_TRACES_ENDPOINT` for traces alone). Without one,
//! nothing leaves the machine.
//!
//! The jobs that read whole indexes and the fetches are spans, each
//! recording the process's resident memory when it ends (`process.rss_mb`),
//! so a collector shows which of them grows the agent and by how much.

use opentelemetry::trace::TracerProvider as _;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

/// Keeps the exporter flushing; dropped at exit.
pub struct Guard(Option<opentelemetry_sdk::trace::SdkTracerProvider>);

impl Drop for Guard {
    fn drop(&mut self) {
        if let Some(p) = self.0.take() {
            let _ = p.shutdown();
        }
    }
}

fn named(var: &str) -> bool {
    std::env::var(var).is_ok_and(|v| !v.trim().is_empty())
}

/// Install the subscriber (once, at start).
pub fn init(service: &str) -> Guard {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| "drive=info".into());
    let fmt = tracing_subscriber::fmt::layer().with_writer(std::io::stderr);
    let provider = (named("OTEL_EXPORTER_OTLP_ENDPOINT")
        || named("OTEL_EXPORTER_OTLP_TRACES_ENDPOINT"))
    .then(|| {
        let exporter = opentelemetry_otlp::SpanExporter::builder()
            .with_http()
            .build()
            .ok()?;
        Some(
            opentelemetry_sdk::trace::SdkTracerProvider::builder()
                .with_batch_exporter(exporter)
                .with_resource(
                    opentelemetry_sdk::Resource::builder()
                        .with_service_name(service.to_string())
                        .build(),
                )
                .build(),
        )
    })
    .flatten();
    let otel = provider
        .as_ref()
        .map(|p| tracing_opentelemetry::layer().with_tracer(p.tracer("drive")));
    tracing_subscriber::registry()
        .with(filter)
        .with(fmt)
        .with(otel)
        .init();
    Guard(provider)
}

/// This process's resident memory in MB (Linux; 0 elsewhere).
pub fn rss_mb() -> u64 {
    std::fs::read_to_string("/proc/self/statm")
        .ok()
        .and_then(|s| s.split_whitespace().nth(1)?.parse::<u64>().ok())
        .map_or(0, |pages| pages * 4096 / (1024 * 1024))
}

/// Record the memory on the current span (call at a job's end).
pub fn mark_rss() {
    tracing::Span::current().record("process.rss_mb", rss_mb());
}

/// Records the memory on the current span when the job it was made in
/// ends, however it returns.
pub struct RssOnDrop;

impl Drop for RssOnDrop {
    fn drop(&mut self) {
        mark_rss();
    }
}

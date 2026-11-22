//! Where the node's spans go: stderr always, and an OTLP collector when
//! one is named (`OTEL_EXPORTER_OTLP_ENDPOINT`, or
//! `OTEL_EXPORTER_OTLP_TRACES_ENDPOINT` for traces alone). Without one,
//! nothing leaves the machine.
//!
//! Every HTTP request is a span (`http.request`), a child of the caller's
//! trace when the request carries a W3C `traceparent` (a Studio page's
//! fetch): one trace then runs from the page through the node.

use axum::extract::Request;
use axum::middleware::Next;
use axum::response::Response;
use opentelemetry::propagation::TextMapPropagator;
use opentelemetry::trace::TracerProvider as _;
use opentelemetry_sdk::propagation::TraceContextPropagator;
use tracing::Instrument;
use tracing_opentelemetry::OpenTelemetrySpanExt;
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
pub fn init(service: &str, default_filter: &str) -> Guard {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| default_filter.into());
    let fmt = tracing_subscriber::fmt::layer();
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
        .map(|p| tracing_opentelemetry::layer().with_tracer(p.tracer("drive-node")));
    tracing_subscriber::registry()
        .with(filter)
        .with(fmt)
        .with(otel)
        .init();
    Guard(provider)
}

/// Whether spans are going anywhere but stderr.
pub fn exporting() -> bool {
    named("OTEL_EXPORTER_OTLP_ENDPOINT") || named("OTEL_EXPORTER_OTLP_TRACES_ENDPOINT")
}

/// A page's spans (OTLP over HTTP, JSON or protobuf), passed on to the
/// collector this node exports to: the page needs no address of its own,
/// and the collector no browser rules. 404 when the node exports nothing.
pub async fn relay(
    headers: axum::http::HeaderMap,
    body: axum::body::Bytes,
) -> axum::http::StatusCode {
    use axum::http::StatusCode;
    let url = match (
        std::env::var("OTEL_EXPORTER_OTLP_TRACES_ENDPOINT"),
        std::env::var("OTEL_EXPORTER_OTLP_ENDPOINT"),
    ) {
        (Ok(t), _) if !t.trim().is_empty() => t,
        (_, Ok(e)) if !e.trim().is_empty() => format!("{}/v1/traces", e.trim_end_matches('/')),
        _ => return StatusCode::NOT_FOUND,
    };
    static CLIENT: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    let client = CLIENT.get_or_init(reqwest::Client::new);
    let kind = headers
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("application/json")
        .to_string();
    match client
        .post(url)
        .header("content-type", kind)
        .body(body)
        .timeout(std::time::Duration::from_secs(5))
        .send()
        .await
    {
        Ok(r) if r.status().is_success() => StatusCode::NO_CONTENT,
        Ok(r) => {
            tracing::warn!(
                otlp.status = r.status().as_u16(),
                "the collector refused a page's spans"
            );
            StatusCode::BAD_GATEWAY
        }
        Err(e) => {
            tracing::warn!(error = %e, "the collector did not answer");
            StatusCode::BAD_GATEWAY
        }
    }
}

/// Each request as a span, under the caller's trace when it sent one.
pub async fn requests(req: Request, next: Next) -> Response {
    let path = req.uri().path().to_string();
    let span = tracing::info_span!(
        "http.request",
        otel.name = %format!("{} {}", req.method(), route_of(&path)),
        http.request.method = %req.method(),
        url.path = %path,
        http.response.status_code = tracing::field::Empty,
    );
    let parent = TraceContextPropagator::new().extract(&HeaderMap(req.headers()));
    let _ = span.set_parent(parent);
    let res = next.run(req).instrument(span.clone()).await;
    span.record("http.response.status_code", res.status().as_u16());
    res
}

/// A path with its ids folded (`/fetch/{space}/{location}`), so the
/// traces of one route group together.
fn route_of(path: &str) -> String {
    let parts: Vec<&str> = path.split('/').collect();
    let keep = |p: &str| {
        matches!(
            p,
            "" | "api"
                | "studio"
                | "search"
                | "list"
                | "fetch"
                | "orgs"
                | "shared"
                | "blobs"
                | "browse"
                | "sidecar"
                | "sidecars"
                | "copies"
                | "sync"
                | "health"
                | "members"
                | "index"
                | "s"
                | "sites"
                | "otlp"
                | "v1"
                | "traces"
        )
    };
    parts
        .iter()
        .map(|p| {
            if keep(p) {
                (*p).to_string()
            } else {
                "{}".to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("/")
}

struct HeaderMap<'a>(&'a axum::http::HeaderMap);

impl opentelemetry::propagation::Extractor for HeaderMap<'_> {
    fn get(&self, key: &str) -> Option<&str> {
        self.0.get(key).and_then(|v| v.to_str().ok())
    }
    fn keys(&self) -> Vec<&str> {
        self.0.keys().map(|k| k.as_str()).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::route_of;

    #[test]
    fn routes_fold_their_ids() {
        assert_eq!(route_of("/studio/api/search"), "/studio/api/search");
        assert_eq!(
            route_of("/studio/api/fetch/codywright/fa7e5105"),
            "/studio/api/fetch/{}/{}"
        );
        assert_eq!(route_of("/blobs/abc123"), "/blobs/{}");
    }
}

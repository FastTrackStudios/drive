//! Built sites, served: `/sites/{space}/{id}/…` reads the site's output
//! document (path → blob) and sends the blob. The agent that builds a site
//! uploads its files as blobs and writes that document; the node needs
//! nothing else (no `ddc`, no copy of the pages on disk).
//!
//! Like share links, a site is readable without signing in: publishing it
//! is the choice to show it.

use axum::extract::{Path as UrlPath, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Redirect, Response};
use drive_files::sites;

use crate::catalog::Catalog;

/// `/sites/{space}/{id}`: the same with its slash, so the site's own
/// links resolve.
pub async fn root(UrlPath((space, id)): UrlPath<(String, String)>) -> Redirect {
    Redirect::permanent(&sites::node_path(&space, &id))
}

pub async fn index(
    UrlPath((space, id)): UrlPath<(String, String)>,
    State(c): State<Catalog>,
) -> Response {
    serve(&c, &space, &id, "").await
}

pub async fn file(
    UrlPath((space, id, path)): UrlPath<(String, String, String)>,
    State(c): State<Catalog>,
) -> Response {
    serve(&c, &space, &id, &path).await
}

/// Every site response: a sandbox without `allow-same-origin`, so a
/// published site's scripts run in an opaque origin and cannot read the
/// node's `localStorage` (`drive:auth`) or its `drive_token` cookie, nor
/// call the node's API as the visitor; and never sniffed.
pub async fn isolate(mut resp: Response) -> Response {
    let h = resp.headers_mut();
    h.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static("sandbox allow-scripts allow-forms allow-popups allow-downloads"),
    );
    h.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    resp
}

/// Long-lived for files whose name carries their hash (`main.5c21d1ce01ec.css`,
/// `search/asset/<v>/…`); pages are checked every time.
fn cache_control(name: &str) -> &'static str {
    let file = name.rsplit('/').next().unwrap_or(name);
    let hashed = name.starts_with("search/asset/")
        || file.split('.').any(|part| {
            part.len() >= 10
                && part.chars().all(|c| c.is_ascii_alphanumeric())
                && part.chars().any(|c| c.is_ascii_digit())
        });
    if name.ends_with(".html") || !hashed {
        "no-cache"
    } else {
        "public, max-age=31536000, immutable"
    }
}

async fn serve(c: &Catalog, space: &str, id: &str, path: &str) -> Response {
    let files = c
        .rooms
        .peek(&sites::output_doc(space, id), sites::output)
        .await;
    if files.is_empty() {
        return (StatusCode::NOT_FOUND, "This site has not been built yet.").into_response();
    }
    let (name, hash, status) = match sites::resolve(&files, path) {
        Some((name, hash)) => {
            // A folder asked for without its slash.
            if name.ends_with("index.html")
                && !path.is_empty()
                && !path.ends_with('/')
                && !path.ends_with("index.html")
            {
                return Redirect::permanent(&format!("{}{path}/", sites::node_path(space, id)))
                    .into_response();
            }
            (name, hash.to_string(), StatusCode::OK)
        }
        None => match files.get("404.html") {
            Some(h) => ("404.html".to_string(), h.clone(), StatusCode::NOT_FOUND),
            None => return (StatusCode::NOT_FOUND, "No such page.").into_response(),
        },
    };
    let bytes = match c.blobs.get(&hash).await {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!(site.space = %space, site.id = %id, site.path = %name, error = %e, "a site file's blob is missing");
            return (
                StatusCode::BAD_GATEWAY,
                "This file of the site is missing on the node.",
            )
                .into_response();
        }
    };
    let mut resp = (status, bytes).into_response();
    let h = resp.headers_mut();
    h.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static(sites::mime(&name)),
    );
    h.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static(cache_control(&name)),
    );
    resp
}

#[cfg(test)]
mod tests {
    use super::cache_control;

    #[test]
    fn hashed_files_are_cached_for_good() {
        assert_eq!(cache_control("index.html"), "no-cache");
        assert_eq!(
            cache_control("main.5c21d1ce01ec.css"),
            "public, max-age=31536000, immutable"
        );
        assert_eq!(
            cache_control("search/asset/d4703dd6b60f1dfd/search.js"),
            "public, max-age=31536000, immutable"
        );
        assert_eq!(cache_control("search/meta"), "no-cache");
        assert_eq!(cache_control("robots.txt"), "no-cache");
    }

    /// A published site runs in an opaque origin: its scripts cannot read
    /// the node's `localStorage` or cookies (drive#57).
    #[tokio::test]
    async fn every_site_response_is_sandboxed() {
        use tower::ServiceExt;
        let data = tempfile::tempdir().unwrap();
        let opts = crate::Options {
            no_iroh: true,
            no_backfill: true,
            ..Default::default()
        };
        let app = crate::router(data.path(), Vec::new(), opts).await;
        for url in [
            "/sites/home/wiki",
            "/sites/home/wiki/",
            "/sites/home/wiki/a.js",
        ] {
            let r = app
                .clone()
                .oneshot(
                    axum::http::Request::get(url)
                        .body(axum::body::Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            let h = r.headers();
            assert_eq!(
                h.get("content-security-policy")
                    .map(|v| v.to_str().unwrap()),
                Some("sandbox allow-scripts allow-forms allow-popups allow-downloads"),
                "{url}"
            );
            assert_eq!(
                h.get("x-content-type-options").map(|v| v.to_str().unwrap()),
                Some("nosniff"),
                "{url}"
            );
        }
    }
}

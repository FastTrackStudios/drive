//! The S3 subset kache's remote speaks (OpenDAL's S3 service, path-style
//! addressing; read from kache 2.0.0-rc.3 through 2.3.0, docs/kache.md):
//!
//! | Request | What kache uses it for |
//! |---|---|
//! | `HEAD /kache/<key>` | does an entry exist (its v3 manifest) |
//! | `GET /kache/<key>` | a manifest, a pack, a merged build manifest |
//! | `PUT /kache/<key>` | a pack or manifest, `If-None-Match: *` (create only); a merged build manifest, `If-Match: "<etag>"` |
//! | `GET /kache?list-type=2&prefix=…` | ListObjectsV2, recursive, paginated |
//!
//! Nothing else is answered (`501 NotImplemented`): no DELETE, no multipart
//! upload, no copy. Requests are not signature-checked (any credentials
//! do), and only loopback clients are served: everyone else gets `403`.
//! Absence is a quick `404 NoSuchKey`, so kache compiles at once.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, UNIX_EPOCH};

use axum::Router;
use axum::body::Body;
use axum::extract::{ConnectInfo, Path, Query, Request, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use futures::StreamExt;

use crate::{BUCKET, Cond, Kache, etag};

/// Listen on `addr` (a loopback address; anything else is refused) and
/// serve until the process stops. Returns the bound address.
pub async fn serve(
    k: Arc<Kache>,
    addr: SocketAddr,
) -> std::io::Result<(SocketAddr, tokio::task::JoinHandle<()>)> {
    if !addr.ip().is_loopback() {
        return Err(std::io::Error::other(
            "the kache endpoint listens on loopback only",
        ));
    }
    let listener = tokio::net::TcpListener::bind(addr).await?;
    let bound = listener.local_addr()?;
    let app = router(k).into_make_service_with_connect_info::<SocketAddr>();
    let task = tokio::spawn(async move {
        if let Err(e) = axum::serve(listener, app).await {
            tracing::warn!(error = %e, "the kache endpoint stopped");
        }
    });
    Ok((bound, task))
}

pub fn router(k: Arc<Kache>) -> Router {
    Router::new()
        .route("/{bucket}", axum::routing::any(bucket))
        .route("/{bucket}/", axum::routing::any(bucket))
        .route("/{bucket}/{*key}", axum::routing::any(object))
        .fallback(not_implemented)
        .layer(axum::extract::DefaultBodyLimit::disable())
        .with_state(k)
}

fn xml_error(status: StatusCode, code: &str, message: &str) -> Response {
    let body = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<Error><Code>{code}</Code><Message>{}</Message></Error>",
        escape(message)
    );
    (status, [(header::CONTENT_TYPE, "application/xml")], body).into_response()
}

async fn not_implemented() -> Response {
    xml_error(
        StatusCode::NOT_IMPLEMENTED,
        "NotImplemented",
        "Drive's kache endpoint answers HEAD, GET, PUT and ListObjectsV2 only",
    )
}

fn refused(peer: SocketAddr) -> Option<Response> {
    if peer.ip().is_loopback() {
        return None;
    }
    tracing::warn!(
        kache.outcome = "refused",
        "a client that is not on this computer asked the kache endpoint"
    );
    Some(xml_error(
        StatusCode::FORBIDDEN,
        "AccessDenied",
        "loopback clients only",
    ))
}

fn no_bucket() -> Response {
    xml_error(
        StatusCode::NOT_FOUND,
        "NoSuchBucket",
        "the bucket is `kache`",
    )
}

async fn bucket(
    State(k): State<Arc<Kache>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    Path(b): Path<String>,
    method: Method,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    if let Some(r) = refused(peer) {
        return r;
    }
    if b != BUCKET {
        return no_bucket();
    }
    match method {
        Method::HEAD => StatusCode::OK.into_response(),
        Method::GET if q.get("list-type").map(String::as_str) == Some("2") => list(&k, &q).await,
        _ => not_implemented().await,
    }
}

async fn list(k: &Kache, q: &HashMap<String, String>) -> Response {
    let prefix = q.get("prefix").cloned().unwrap_or_default();
    let max = q
        .get("max-keys")
        .and_then(|m| m.parse::<usize>().ok())
        .unwrap_or(1000)
        .clamp(1, 1000);
    let after = q
        .get("continuation-token")
        .or_else(|| q.get("start-after"))
        .cloned()
        .unwrap_or_default();
    let delimiter = q.get("delimiter").cloned().unwrap_or_default();
    let keys = k.list(&prefix).await;
    let mut contents = String::new();
    let mut prefixes = std::collections::BTreeSet::new();
    let mut count = 0;
    let mut last = String::new();
    let mut truncated = false;
    let now = iso8601(crate::store::now_ms());
    for (key, size, hash) in keys
        .iter()
        .filter(|(key, _, _)| key.as_str() > after.as_str())
    {
        // Advance past every key in the last prefix before cutting the page.
        let common_prefix = if delimiter.is_empty() {
            None
        } else {
            key[prefix.len()..]
                .find(&delimiter)
                .map(|i| &key[..prefix.len() + i + delimiter.len()])
        };
        if common_prefix.is_some_and(|p| prefixes.contains(p)) {
            last.clone_from(key);
            continue;
        }
        if count == max {
            truncated = true;
            break;
        }
        if let Some(p) = common_prefix {
            prefixes.insert(p.to_string());
        } else {
            contents.push_str(&format!(
                "<Contents><Key>{}</Key><LastModified>{now}</LastModified><ETag>{}</ETag><Size>{size}</Size><StorageClass>STANDARD</StorageClass></Contents>",
                escape(key),
                escape(&etag(hash))
            ));
        }
        count += 1;
        last.clone_from(key);
    }
    let common: String = prefixes
        .iter()
        .map(|p| {
            format!(
                "<CommonPrefixes><Prefix>{}</Prefix></CommonPrefixes>",
                escape(p)
            )
        })
        .collect();
    let next = if truncated {
        format!(
            "<NextContinuationToken>{}</NextContinuationToken>",
            escape(&last)
        )
    } else {
        String::new()
    };
    let body = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<ListBucketResult xmlns=\"http://s3.amazonaws.com/doc/2006-03-01/\"><Name>{BUCKET}</Name><Prefix>{}</Prefix><KeyCount>{count}</KeyCount><MaxKeys>{max}</MaxKeys><IsTruncated>{truncated}</IsTruncated>{next}{contents}{common}</ListBucketResult>",
        escape(&prefix)
    );
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "application/xml")],
        body,
    )
        .into_response()
}

fn object_headers(size: u64, hash: &str, modified: u64) -> HeaderMap {
    let mut h = HeaderMap::new();
    h.insert(header::CONTENT_LENGTH, HeaderValue::from(size));
    if let Ok(v) = HeaderValue::from_str(&etag(hash)) {
        h.insert(header::ETAG, v);
    }
    let when = UNIX_EPOCH + Duration::from_millis(modified);
    if let Ok(v) = HeaderValue::from_str(&httpdate::fmt_http_date(when)) {
        h.insert(header::LAST_MODIFIED, v);
    }
    h.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/octet-stream"),
    );
    h
}

fn no_key() -> Response {
    xml_error(
        StatusCode::NOT_FOUND,
        "NoSuchKey",
        "not kept here or by this device's build group",
    )
}

async fn object(
    State(k): State<Arc<Kache>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    Path((b, key)): Path<(String, String)>,
    req: Request,
) -> Response {
    if let Some(r) = refused(peer) {
        return r;
    }
    if b != BUCKET {
        return no_bucket();
    }
    if !valid_key(&key) {
        return xml_error(StatusCode::BAD_REQUEST, "InvalidArgument", "a bad key");
    }
    let started = std::time::Instant::now();
    let method = req.method().clone();
    let r = match method {
        Method::HEAD => match k.head(&key).await {
            Some((size, hash, modified)) => {
                (StatusCode::OK, object_headers(size, &hash, modified)).into_response()
            }
            None => StatusCode::NOT_FOUND.into_response(),
        },
        Method::GET => match k.open(&key).await {
            // The file and the headers from one look, under the store's
            // lock: a replacement after the lookup is never sent with the
            // old object's size and ETag.
            Ok(Some(_)) => match k.store.open_file(&key) {
                Ok(Some((f, o))) => (
                    StatusCode::OK,
                    object_headers(o.size, &o.hash, o.modified),
                    Body::from_stream(tokio_util::io::ReaderStream::new(
                        tokio::fs::File::from_std(f),
                    )),
                )
                    .into_response(),
                // Let go between the lookup and the open.
                _ => no_key(),
            },
            Ok(None) => no_key(),
            Err(e) => xml_error(StatusCode::SERVICE_UNAVAILABLE, "SlowDown", &e),
        },
        Method::PUT => put(&k, &key, req).await,
        _ => not_implemented().await,
    };
    tracing::debug!(
        kache.method = %method,
        kache.status = r.status().as_u16(),
        kache.ms = started.elapsed().as_millis() as u64,
        "kache endpoint"
    );
    r
}

/// The write's condition; `None` for one kache never sends.
fn condition(headers: &HeaderMap) -> Option<Cond> {
    if let Some(v) = headers.get(header::IF_NONE_MATCH) {
        return (v.as_bytes() == b"*").then_some(Cond::IfAbsent);
    }
    if let Some(v) = headers.get(header::IF_MATCH) {
        let tag = v.to_str().unwrap_or_default().trim();
        let tag = tag.strip_prefix("W/").unwrap_or(tag);
        return Some(Cond::IfMatch(tag.trim_matches('"').to_string()));
    }
    Some(Cond::Any)
}

fn not_implemented_sync() -> Response {
    xml_error(
        StatusCode::NOT_IMPLEMENTED,
        "NotImplemented",
        "If-None-Match takes `*` only",
    )
}

async fn put(k: &Arc<Kache>, key: &str, req: Request) -> Response {
    use std::io::Write;
    let Some(cond) = condition(req.headers()) else {
        return not_implemented_sync();
    };
    // A create-only write that would find the key: answered before the
    // body is read.
    if cond == Cond::IfAbsent && k.store.get(key).is_some() {
        return xml_error(
            StatusCode::PRECONDITION_FAILED,
            "PreconditionFailed",
            "the object exists",
        );
    }
    let expected: Option<u64> = req
        .headers()
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok()?.parse().ok());
    let (tmp, mut file) = match k.store.temp() {
        Ok(t) => t,
        Err(e) => {
            return xml_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "InternalError",
                &e.to_string(),
            );
        }
    };
    let mut hasher = blake3::Hasher::new();
    let mut n = 0u64;
    let mut body = req.into_body().into_data_stream();
    let mut failed = None;
    while let Some(chunk) = body.next().await {
        match chunk {
            Ok(c) => {
                hasher.update(&c);
                n += c.len() as u64;
                if let Err(e) = file.write_all(&c) {
                    failed = Some(e.to_string());
                    break;
                }
            }
            Err(e) => {
                failed = Some(e.to_string());
                break;
            }
        }
    }
    drop(file);
    if failed.is_none() && expected.is_some_and(|e| e != n) {
        failed = Some("the body is shorter than its Content-Length".into());
    }
    if let Some(e) = failed {
        k.store.discard(&tmp);
        return xml_error(StatusCode::BAD_REQUEST, "IncompleteBody", &e);
    }
    let hash = hasher.finalize().to_hex().to_string();
    match k.store.commit(key, &tmp, n, &hash, &cond) {
        Ok((crate::Commit::Stored, gone)) => {
            k.forget(gone).await;
            let mut h = HeaderMap::new();
            if let Ok(v) = HeaderValue::from_str(&etag(&hash)) {
                h.insert(header::ETAG, v);
            }
            (StatusCode::OK, h).into_response()
        }
        Ok((crate::Commit::Precondition, _)) => xml_error(
            StatusCode::PRECONDITION_FAILED,
            "PreconditionFailed",
            "the object changed or exists",
        ),
        Err(e) => xml_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "InternalError",
            &e.to_string(),
        ),
    }
}

/// Keys as kache writes them: relative, `/`-separated, no empty, `.` or
/// `..` segment, no control characters.
fn valid_key(key: &str) -> bool {
    !key.is_empty()
        && key.len() <= 1024
        && !key.starts_with('/')
        && !key.contains('\\')
        && !key.chars().any(char::is_control)
        && key
            .split('/')
            .all(|s| !s.is_empty() && s != "." && s != "..")
}

fn escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

/// `2026-11-21T12:00:00.000Z`.
fn iso8601(ms: u64) -> String {
    let secs = ms / 1000;
    let (days, rem) = (secs / 86_400, secs % 86_400);
    let (y, m, d) = civil(days as i64);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}.{:03}Z",
        rem / 3600,
        rem / 60 % 60,
        rem % 60,
        ms % 1000
    )
}

/// Days since 1970-01-01 to (year, month, day) (Howard Hinnant's algorithm).
fn civil(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = yoe + era * 400 + i64::from(m <= 2);
    (y, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn listing_reflects_local_writes_and_removals_immediately() {
        let dir = tempfile::tempdir().unwrap();
        let store = crate::Store::open(dir.path(), 1024).unwrap();
        let cache = Kache::new(store, Arc::new(Vec::new), Default::default());
        let prefix = "artifacts/";
        let key = "artifacts/new";
        assert!(cache.list(prefix).await.is_empty());

        let (path, mut file) = cache.store.temp().unwrap();
        use std::io::Write;
        file.write_all(b"new").unwrap();
        drop(file);
        let hash = blake3::hash(b"new").to_hex().to_string();
        cache
            .store
            .commit(key, &path, 3, &hash, &Cond::Any)
            .unwrap();
        let after_put = cache.list(prefix).await;
        cache.store.remove(key).unwrap();
        let after_remove = cache.list(prefix).await;

        assert_eq!(after_put, [(key.to_string(), 3, hash)]);
        assert!(after_remove.is_empty());
    }

    #[tokio::test]
    async fn delimiter_pages_emit_each_prefix_once() {
        let dir = tempfile::tempdir().unwrap();
        let store = crate::Store::open(dir.path(), 1024).unwrap();
        let hash = blake3::hash(b"").to_hex();
        for key in [
            "cache/a/first",
            "cache/a/last",
            "cache/b/first",
            "cache/b/last",
        ] {
            let (path, file) = store.temp().unwrap();
            drop(file);
            store
                .commit(key, &path, 0, hash.as_str(), &Cond::Any)
                .unwrap();
        }
        let kache = Kache::new(store, Arc::new(Vec::new), Default::default());
        let mut query = HashMap::from([
            ("delimiter".to_string(), "/".to_string()),
            ("prefix".to_string(), "cache/".to_string()),
            ("max-keys".to_string(), "1".to_string()),
        ]);

        let page = list(&kache, &query).await;
        let bytes = axum::body::to_bytes(page.into_body(), usize::MAX)
            .await
            .unwrap();
        let first = std::str::from_utf8(&bytes).unwrap();
        assert!(
            first.contains("<CommonPrefixes><Prefix>cache/a/</Prefix></CommonPrefixes>"),
            "{first}"
        );
        let (_, remainder) = first.split_once("<NextContinuationToken>").unwrap();
        let (token, _) = remainder.split_once("</NextContinuationToken>").unwrap();
        query.insert("continuation-token".to_string(), token.to_string());

        let page = list(&kache, &query).await;
        let bytes = axum::body::to_bytes(page.into_body(), usize::MAX)
            .await
            .unwrap();
        let second = std::str::from_utf8(&bytes).unwrap();
        assert!(
            !second.contains("<CommonPrefixes><Prefix>cache/a/</Prefix>"),
            "{second}"
        );
        assert!(
            second.contains("<CommonPrefixes><Prefix>cache/b/</Prefix></CommonPrefixes>"),
            "{second}"
        );
        assert!(
            second.contains("<IsTruncated>false</IsTruncated>"),
            "{second}"
        );
    }

    #[test]
    fn dates() {
        assert_eq!(super::iso8601(0), "1970-01-01T00:00:00.000Z");
        assert_eq!(
            super::iso8601(1_795_262_400_123),
            "2026-11-21T12:00:00.123Z"
        );
    }

    #[test]
    fn keys() {
        assert!(super::valid_key("artifacts/v3/packs/serde/ab12.tar.zst"));
        for bad in ["", "/abs", "a/../b", "a//b", "a/./b", "a\\b", "a\nb"] {
            assert!(!super::valid_key(bad), "{bad:?}");
        }
    }
}

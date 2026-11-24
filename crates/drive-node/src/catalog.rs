//! The node's view of every device's files: location indexes come in
//! (`PUT /index/{space}/{location}`), and anyone in the space can search
//! them, browse them, see each file's copies and the protection report,
//! whether or not the devices are on. `GET /fetch/…` brings one file's
//! bytes from its device, through that device's agent connection.

use std::collections::HashMap;
use std::sync::Arc;

use axum::Json;
use axum::body::Bytes;
use axum::extract::{Path as UrlPath, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use drive_files::blob::BlobStore;
use drive_files::space;
use serde::Serialize;

use crate::rooms::{FetchError, Rooms};
use crate::store::Found;

#[derive(Clone)]
pub struct Catalog {
    pub rooms: Arc<Rooms>,
    pub blobs: BlobStore,
    /// Each space's names, as of a version of its document.
    pub names: NameCache,
    /// Each space's protection reports, as of its document and its rows.
    pub reports: ReportCache,
}

/// A space's names (locations, devices, storages, what the node keeps) by
/// space, with the version of the space document they were read from.
pub type NameCache = Arc<std::sync::Mutex<HashMap<String, (loro::Frontiers, Arc<DocNames>)>>>;

/// A space's protection reports (by space and how many short files each
/// lists), with the version of the space document and the generation of
/// its index rows they were worked out from: checking every file against
/// every policy is a scan of the whole space, done again only when one of
/// the two has moved. The lock lets one request work them out while the
/// others wait for its answer instead of scanning too.
#[derive(Clone, Default)]
pub struct ReportCache {
    done: Arc<std::sync::Mutex<HashMap<(String, usize), Reports>>>,
    working: Arc<tokio::sync::Mutex<()>>,
}

#[derive(Clone)]
struct Reports {
    version: loro::Frontiers,
    generation: u64,
    reports: Arc<Vec<drive_files::protect::Report>>,
}

pub(crate) type Q = Query<HashMap<String, String>>;

pub(crate) fn q<'a>(q: &'a HashMap<String, String>, k: &str) -> &'a str {
    q.get(k).map_or("", String::as_str)
}

pub(crate) fn fail(e: impl std::fmt::Display) -> StatusCode {
    tracing::warn!(error = %e, "a catalog query failed");
    StatusCode::INTERNAL_SERVER_ERROR
}

/// Forget a location's index (the location was removed).
pub async fn drop_index(
    UrlPath((space_id, location)): UrlPath<(String, String)>,
    State(c): State<Catalog>,
) -> Result<StatusCode, StatusCode> {
    c.rooms
        .store
        .drop_index(&space_id, &location)
        .await
        .map_err(fail)?;
    tracing::info!(index.space = %space_id, index.location = %location, "an index was dropped");
    Ok(StatusCode::NO_CONTENT)
}

/// Take a location's new index: kept as a blob (so devices can browse it
/// offline) and as rows (for search).
pub async fn put_index(
    UrlPath((space_id, location)): UrlPath<(String, String)>,
    State(c): State<Catalog>,
    body: Bytes,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let index = drive_files::index::decode(&body).ok_or(StatusCode::BAD_REQUEST)?;
    if index.location != location {
        return Err(StatusCode::BAD_REQUEST);
    }
    let hash = c.blobs.put(body.to_vec()).await.map_err(fail)?;
    let n = c
        .rooms
        .store
        .put_index(&space_id, &index)
        .await
        .map_err(fail)?;
    tracing::info!(index.space = %space_id, index.location = %location, index.entries = n, "an index came in");
    Ok(Json(serde_json::json!({ "hash": hash, "entries": n })))
}

/// A hit, with where it is and whether that device is on.
#[derive(Serialize)]
pub(crate) struct Hit {
    #[serde(flatten)]
    pub(crate) found: Found,
    location_name: String,
    device: String,
    device_name: String,
    storage: String,
    online: bool,
    /// Locations holding the same content (itself included).
    copies: usize,
    /// Its bytes on the node (their hash), when it is kept available: it
    /// can be downloaded while its computer is off.
    available: Option<String>,
}

/// What the space document says of its locations, read once per version
/// of the document: every request names its hits from it.
pub struct DocNames {
    pub(crate) locations: HashMap<String, space::Location>,
    pub(crate) devices: HashMap<String, String>,
    storages: HashMap<String, String>,
    pub(crate) available: HashMap<String, HashMap<String, drive_files::projects::Available>>,
    /// The folders adopted into the Drive tree (docs/adopt.md).
    pub(crate) adoptions: Vec<drive_files::adopt::Adoption>,
}

pub(crate) struct Names {
    pub(crate) doc: Arc<DocNames>,
    pub(crate) online: Vec<String>,
}

fn doc_names(d: &loro::LoroDoc) -> DocNames {
    let locations: HashMap<String, space::Location> = space::locations(d)
        .into_iter()
        .map(|l| (l.id.clone(), l))
        .collect();
    let available = locations
        .keys()
        .map(|l| (l.clone(), drive_files::projects::available(d, l)))
        .collect();
    DocNames {
        locations,
        devices: space::devices(d)
            .into_iter()
            .map(|x| (x.id, x.name))
            .collect(),
        storages: space::storages(d)
            .into_iter()
            .map(|x| (x.id, x.name))
            .collect(),
        available,
        adoptions: drive_files::adopt::adoptions(d),
    }
}

/// The space's names: read from its document only when the document has
/// changed since the last read (under the rooms' lock, which every sync
/// shares, only the version is compared).
#[tracing::instrument(name = "drive.names", skip_all, fields(names.cached = tracing::field::Empty))]
pub(crate) async fn names(c: &Catalog, space_id: &str) -> Names {
    let cached = c.names.lock().expect("names").get(space_id).cloned();
    let (version, fresh) = c
        .rooms
        .read(&drive_files::space_doc(space_id), |d| {
            let v = d.oplog_frontiers();
            match &cached {
                Some((at, _)) if *at == v => (v, None),
                _ => (v, Some(doc_names(d))),
            }
        })
        .await;
    tracing::Span::current().record("names.cached", fresh.is_none());
    let doc = match (fresh, cached) {
        (Some(n), _) => {
            let n = Arc::new(n);
            c.names
                .lock()
                .expect("names")
                .insert(space_id.to_string(), (version, n.clone()));
            n
        }
        (None, Some((_, n))) => n,
        (None, None) => Arc::new(DocNames {
            locations: HashMap::new(),
            devices: HashMap::new(),
            storages: HashMap::new(),
            available: HashMap::new(),
            adoptions: Vec::new(),
        }),
    };
    Names {
        doc,
        online: c.rooms.agents_online(space_id).await,
    }
}

#[tracing::instrument(name = "drive.hits", skip_all, fields(hits.count = found.len()))]
pub(crate) async fn hits(c: &Catalog, space_id: &str, found: Vec<Found>) -> Vec<Hit> {
    let n = names(c, space_id).await;
    let keys: Vec<(String, i64)> = found
        .iter()
        .filter(|f| !f.dir && !f.sampled.is_empty())
        .map(|f| (f.sampled.clone(), f.size))
        .collect();
    let counts = c
        .rooms
        .store
        .copy_counts(space_id, &keys)
        .await
        .unwrap_or_default();
    let mut out = Vec::with_capacity(found.len());
    for f in found {
        let copies = if f.dir || f.sampled.is_empty() {
            1
        } else {
            counts
                .get(&(f.sampled.clone(), f.size))
                .copied()
                .unwrap_or(0)
        };
        let loc = n.doc.locations.get(&f.location);
        let device = loc.map(|l| l.device.clone()).unwrap_or_default();
        let available = n
            .doc
            .available
            .get(&f.location)
            .and_then(|m| m.get(&f.path))
            .filter(|a| a.size == f.size && a.mtime == f.mtime)
            .map(|a| a.hash.clone());
        out.push(Hit {
            available,
            location_name: loc.map(|l| l.name.clone()).unwrap_or_default(),
            device_name: n.doc.devices.get(&device).cloned().unwrap_or_default(),
            storage: loc
                .and_then(|l| n.doc.storages.get(&l.storage).cloned())
                .unwrap_or_default(),
            online: n.online.contains(&device),
            device,
            copies,
            found: f,
        });
    }
    out
}

/// `?space=&q=&exts=&sampled=&limit=`: files and folders whose names hold
/// every word; with `exts` (`wav,aif`), only files with one of those
/// extensions; with `sampled` (hashes, comma-separated), only files with
/// one of those contents (what tags are on).
pub async fn search(
    State(c): State<Catalog>,
    Query(p): Q,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let space_id = q(&p, "space");
    let limit = q(&p, "limit").parse().unwrap_or(100);
    let exts: Vec<String> = q(&p, "exts").split(',').map(str::to_string).collect();
    let sampled: Vec<String> = q(&p, "sampled").split(',').map(str::to_string).collect();
    let found = c
        .rooms
        .store
        .search_with(space_id, q(&p, "q"), &exts, &sampled, limit)
        .await
        .map_err(fail)?;
    Ok(Json(
        serde_json::json!({ "results": hits(&c, space_id, found).await }),
    ))
}

/// What a POST `/search?space=` asks for, as JSON: as the GET's query,
/// with the contents as a list, as many as a tag is on (a URL holding
/// them all would pass the request-line limit).
#[derive(serde::Deserialize)]
pub struct SearchBody {
    #[serde(default)]
    q: String,
    #[serde(default)]
    exts: Vec<String>,
    #[serde(default)]
    sampled: Vec<String>,
    #[serde(default = "default_limit")]
    limit: i64,
}

fn default_limit() -> i64 {
    100
}

/// POST `/search?space=` with a [`SearchBody`]: as [`search`].
pub async fn search_post(
    State(c): State<Catalog>,
    Query(p): Q,
    Json(b): Json<SearchBody>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let space_id = q(&p, "space");
    let found = c
        .rooms
        .store
        .search_with(space_id, &b.q, &b.exts, &b.sampled, b.limit)
        .await
        .map_err(fail)?;
    Ok(Json(
        serde_json::json!({ "results": hits(&c, space_id, found).await }),
    ))
}

/// `?space=&location=&path=`: one folder of a location's index.
pub async fn list(
    State(c): State<Catalog>,
    Query(p): Q,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let space_id = q(&p, "space");
    let found = c
        .rooms
        .store
        .list_dir(space_id, q(&p, "location"), q(&p, "path"))
        .await
        .map_err(fail)?;
    Ok(Json(
        serde_json::json!({ "entries": hits(&c, space_id, found).await }),
    ))
}

/// `?space=&sampled=&size=`: every location holding this content.
pub async fn copies(
    State(c): State<Catalog>,
    Query(p): Q,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let space_id = q(&p, "space");
    let size = q(&p, "size").parse().unwrap_or(0);
    let found = c
        .rooms
        .store
        .copies(space_id, q(&p, "sampled"), size)
        .await
        .map_err(fail)?;
    let (sites, locations) = c
        .rooms
        .read(&drive_files::space_doc(space_id), |d| {
            (space::sites(d), space::locations(d))
        })
        .await;
    let mut out = hits(&c, space_id, found).await;
    out.sort_by(|a, b| {
        (a.device_name.as_str(), a.found.path.as_str())
            .cmp(&(b.device_name.as_str(), b.found.path.as_str()))
    });
    let with_sites: Vec<serde_json::Value> = out
        .into_iter()
        .map(|h| {
            let holder = locations
                .iter()
                .find(|l| l.id == h.found.location)
                .map_or(h.device.clone(), |l| l.holder().to_string());
            let mut v = serde_json::to_value(&h).unwrap_or_default();
            v["site"] = sites.get(&holder).cloned().unwrap_or_default().into();
            v
        })
        .collect();
    Ok(Json(serde_json::json!({ "copies": with_sites })))
}

/// `?space=&limit=`: every policy checked against every indexed file.
pub async fn protection(
    State(c): State<Catalog>,
    Query(p): Q,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let space_id = q(&p, "space");
    let limit = q(&p, "limit").parse().unwrap_or(50);
    let reports = reports(&c, space_id, limit).await?;
    Ok(Json(serde_json::json!({ "reports": *reports })))
}

/// Every policy checked against every indexed file of a space, each
/// listing at most `limit` files short: from the cache while neither the
/// space document nor its rows have changed.
#[tracing::instrument(name = "drive.protection", skip_all, fields(protection.cached = tracing::field::Empty, protection.files = tracing::field::Empty))]
async fn reports(
    c: &Catalog,
    space_id: &str,
    limit: usize,
) -> Result<Arc<Vec<drive_files::protect::Report>>, StatusCode> {
    let key = (space_id.to_string(), limit.min(1000));
    let span = tracing::Span::current();
    let current = |version: &loro::Frontiers, generation: u64| {
        c.reports
            .done
            .lock()
            .expect("reports")
            .get(&key)
            .filter(|r| r.version == *version && r.generation == generation)
            .map(|r| r.reports.clone())
    };
    let version = c
        .rooms
        .read(&drive_files::space_doc(space_id), |d| d.oplog_frontiers())
        .await;
    if let Some(r) = current(&version, c.rooms.store.generation(space_id)) {
        span.record("protection.cached", true);
        return Ok(r);
    }
    let _working = c.reports.working.lock().await;
    // The generation before the rows: rows changed meanwhile make the
    // answer stale at once, never a stale answer kept as fresh.
    let generation = c.rooms.store.generation(space_id);
    let (version, policies, locations, sites) = c
        .rooms
        .read(&drive_files::space_doc(space_id), |d| {
            (
                d.oplog_frontiers(),
                space::policies(d),
                space::locations(d),
                space::sites(d),
            )
        })
        .await;
    // Someone else worked it out while this one waited.
    if let Some(r) = current(&version, generation) {
        span.record("protection.cached", true);
        return Ok(r);
    }
    span.record("protection.cached", false);
    let rows = c.rooms.store.files(space_id).await.map_err(fail)?;
    span.record("protection.files", rows.len());
    let reports = tokio::task::spawn_blocking(move || {
        drive_files::protect::check(&policies, &locations, &sites, &rows, key.1)
    })
    .await
    .map_err(fail)?;
    let reports = Arc::new(reports);
    c.reports.done.lock().expect("reports").insert(
        (space_id.to_string(), limit.min(1000)),
        Reports {
            version,
            generation,
            reports: reports.clone(),
        },
    );
    Ok(reports)
}

/// `?space=&policy=&limit=`: the files a policy finds short of copies, as
/// browsable hits, each with what it has (`kept`).
pub async fn at_risk(
    State(c): State<Catalog>,
    Query(p): Q,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let space_id = q(&p, "space");
    let limit: usize = q(&p, "limit").parse().unwrap_or(200).clamp(1, 1000);
    // Each policy is checked on its own, so one policy's report is the
    // same in the whole space's set.
    let reports = reports(&c, space_id, limit).await?;
    let mut found = Vec::new();
    let mut kept = Vec::new();
    for short in reports
        .iter()
        .filter(|r| q(&p, "policy").is_empty() || r.policy.id == q(&p, "policy"))
        .flat_map(|r| r.short.iter())
    {
        if let Some(f) = c
            .rooms
            .store
            .entry(space_id, &short.location, &short.path)
            .await
            .map_err(fail)?
        {
            found.push(f);
            kept.push(serde_json::to_value(&short.kept).unwrap_or_default());
        }
    }
    let out: Vec<serde_json::Value> = hits(&c, space_id, found)
        .await
        .into_iter()
        .zip(kept)
        .map(|(h, k)| {
            let mut v = serde_json::to_value(&h).unwrap_or_default();
            v["kept"] = k;
            v
        })
        .collect();
    Ok(Json(serde_json::json!({ "results": out })))
}

/// `?space=`: how many files and bytes of each extension, and in each
/// location, across every index.
pub async fn stats(
    State(c): State<Catalog>,
    Query(p): Q,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let space_id = q(&p, "space");
    let s = c.rooms.store.stats(space_id).await.map_err(fail)?;
    Ok(Json(
        serde_json::json!({ "files": s.files, "bytes": s.bytes, "exts": s.exts, "locations": s.locations }),
    ))
}

/// `?path=&name=`: one file from a location, brought from its device.
pub async fn fetch(
    UrlPath((space_id, location)): UrlPath<(String, String)>,
    State(c): State<Catalog>,
    Query(p): Q,
    headers: axum::http::HeaderMap,
) -> Response {
    let path = q(&p, "path");
    let name = match q(&p, "name") {
        "" => path.rsplit('/').next().unwrap_or("file").to_string(),
        n => n.to_string(),
    };
    // Over iroh when its device serves files that way (docs/transfer.md):
    // the ranges asked for, streamed as they come.
    if let Some(device) = crate::transfer::location_device(&c.rooms, &space_id, &location).await {
        let ask = drive_proto::Browse::ProvideLocation {
            location: location.clone(),
            path: path.to_string(),
        };
        match crate::transfer::provide(&c.rooms, &space_id, &device, ask).await {
            Ok(Some((got, src))) => {
                if c.blobs.has(&got.hash).await.unwrap_or(false) {
                    let mut query = HashMap::new();
                    query.insert("name".to_string(), name);
                    if crate::wants_inline(&p) {
                        query.insert("inline".to_string(), "1".to_string());
                    }
                    return match crate::get_blob_ranged(
                        UrlPath(got.hash),
                        Query(query),
                        headers,
                        State(c.blobs),
                    )
                    .await
                    {
                        Ok(r) => r,
                        Err(s) => s.into_response(),
                    };
                }
                let Some(t) = c.rooms.transfer.get() else {
                    return StatusCode::SERVICE_UNAVAILABLE.into_response();
                };
                let reply = crate::transfer::Reply {
                    name,
                    cache: Some(c.blobs.clone()),
                    inline: crate::wants_inline(&p),
                };
                return crate::transfer::respond(t, &got, src, &headers, reply);
            }
            Ok(None) => {}
            Err(e) => return fetch_failed(e, &location),
        }
    }
    let hash = match c.rooms.fetch(&space_id, &location, path).await {
        Ok(h) => h,
        Err(e) => return fetch_failed(e, &location),
    };
    let mut query = HashMap::new();
    query.insert("name".to_string(), name);
    if crate::wants_inline(&p) {
        query.insert("inline".to_string(), "1".to_string());
    }
    match crate::get_blob_ranged(UrlPath(hash), Query(query), headers, State(c.blobs)).await {
        Ok(r) => r,
        Err(s) => s.into_response(),
    }
}

fn fetch_failed(e: FetchError, location: &str) -> Response {
    match e {
        FetchError::NoLocation => (StatusCode::NOT_FOUND, "no such location").into_response(),
        FetchError::Offline(_) => (
            StatusCode::SERVICE_UNAVAILABLE,
            "the device that has this file is offline",
        )
            .into_response(),
        FetchError::Timeout => {
            (StatusCode::GATEWAY_TIMEOUT, "its device did not answer").into_response()
        }
        FetchError::Refused(why) => {
            tracing::warn!(fetch.location = %location, fetch.why = %why, "a device refused a fetch");
            (StatusCode::NOT_FOUND, why).into_response()
        }
    }
}

/// `?org=`: projects other orgs have shared with this one, with the org
/// that owns each.
pub async fn shared(
    State(c): State<Catalog>,
    Query(p): Q,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let to = q(&p, "org").to_string();
    let orgs: Vec<String> = c
        .rooms
        .store
        .ids()
        .await
        .map_err(fail)?
        .into_iter()
        .filter_map(|id| id.strip_suffix("/drive:space").map(str::to_string))
        .filter(|o| *o != to)
        .collect();
    let mut out = Vec::new();
    for org in orgs {
        let projects = c
            .rooms
            .read(
                &drive_files::space_doc(&org),
                drive_files::projects::projects,
            )
            .await;
        for pr in projects.into_iter().filter(|x| x.shared_with.contains(&to)) {
            out.push(serde_json::json!({ "org": org, "project": pr }));
        }
    }
    Ok(Json(serde_json::json!({ "shared": out })))
}

#[cfg(test)]
mod tests {
    use super::*;
    use drive_files::index::{IndexEntry, LocationIndex};
    use drive_files::space::{Location, Policy};

    fn location(id: &str, volume: &str) -> Location {
        Location {
            id: id.into(),
            name: id.into(),
            device: "pc".into(),
            path: format!("/{id}"),
            volume: volume.into(),
            storage: String::new(),
            mirror_of: String::new(),
            index: String::new(),
            files: 0,
            bytes: 0,
            built: 0,
        }
    }

    fn index(location: &str, files: &[(&str, &str)]) -> LocationIndex {
        LocationIndex {
            location: location.into(),
            root: String::new(),
            built: 1,
            entries: files
                .iter()
                .map(|(path, sampled)| IndexEntry {
                    path: (*path).into(),
                    dir: false,
                    size: 10,
                    mtime: 1,
                    sampled: (*sampled).into(),
                })
                .collect(),
        }
    }

    /// The reports checked afresh from every row, as before the cache.
    async fn slow(c: &Catalog, space_id: &str, limit: usize) -> serde_json::Value {
        let (policies, locations, sites) = c
            .rooms
            .read(&drive_files::space_doc(space_id), |d| {
                (space::policies(d), space::locations(d), space::sites(d))
            })
            .await;
        let rows = c.rooms.store.files(space_id).await.unwrap();
        serde_json::to_value(drive_files::protect::check(
            &policies, &locations, &sites, &rows, limit,
        ))
        .unwrap()
    }

    async fn assert_reports_match(c: &Catalog, space_id: &str) {
        let cached = reports(c, space_id, 50).await.unwrap();
        assert_eq!(
            serde_json::to_value(&*cached).unwrap(),
            slow(c, space_id, 50).await
        );
    }

    #[tokio::test]
    async fn each_hit_counts_the_locations_holding_its_content() {
        let dir = tempfile::tempdir().unwrap();
        let store = crate::store::Store::open(&dir.path().join("drive.db"))
            .await
            .unwrap();
        let rooms = Arc::new(Rooms::new(store, crate::auth::Auth::new(None)));
        let c = Catalog {
            rooms: rooms.clone(),
            blobs: drive_files::storage::StorageConfig::Fs {
                root: dir.path().join("blobs").to_string_lossy().into_owned(),
            }
            .blobs()
            .unwrap(),
            names: Default::default(),
            reports: Default::default(),
        };
        let store = &rooms.store;
        store
            .put_index(
                "home",
                &index("a", &[("x.wav", "hx"), ("y.wav", "hy"), ("n.wav", "")]),
            )
            .await
            .unwrap();
        store
            .put_index("home", &index("b", &[("x.wav", "hx"), ("x2.wav", "hx")]))
            .await
            .unwrap();
        store
            .put_index("other", &index("c", &[("y.wav", "hy")]))
            .await
            .unwrap();
        let found = store.search("home", "wav", &[], 100).await.unwrap();
        let copies: HashMap<(String, String), usize> = hits(&c, "home", found)
            .await
            .into_iter()
            .map(|h| ((h.found.location.clone(), h.found.path.clone()), h.copies))
            .collect();
        let at = |l: &str, p: &str| copies[&(l.to_string(), p.to_string())];
        assert_eq!(at("a", "x.wav"), 2);
        assert_eq!(at("b", "x2.wav"), 2);
        assert_eq!(at("a", "y.wav"), 1, "another space's copy does not count");
        assert_eq!(
            at("a", "n.wav"),
            1,
            "a file with no content hash is its own copy"
        );
    }

    #[tokio::test]
    async fn protection_follows_the_rows_and_the_document() {
        let dir = tempfile::tempdir().unwrap();
        let store = crate::store::Store::open(&dir.path().join("drive.db"))
            .await
            .unwrap();
        let rooms = Arc::new(Rooms::new(store, crate::auth::Auth::new(None)));
        let blobs = drive_files::storage::StorageConfig::Fs {
            root: dir.path().join("blobs").to_string_lossy().into_owned(),
        }
        .blobs()
        .unwrap();
        let c = Catalog {
            rooms: rooms.clone(),
            blobs,
            names: Default::default(),
            reports: Default::default(),
        };
        let doc = drive_files::space_doc("home");
        rooms
            .edit(&doc, "t", |d| {
                space::add_location(d, &location("work", "/")).unwrap();
                space::add_location(d, &location("ext", "/mnt/ext")).unwrap();
                space::set_policy(
                    d,
                    &Policy {
                        id: "two".into(),
                        name: "two copies".into(),
                        scope: vec!["work".into()],
                        copies: 2,
                        media: 2,
                        offsite: 0,
                    },
                )
                .unwrap();
            })
            .await;
        rooms
            .store
            .put_index("home", &index("work", &[("a.wav", "A"), ("b.wav", "B")]))
            .await
            .unwrap();
        assert_reports_match(&c, "home").await;
        assert_eq!(reports(&c, "home", 50).await.unwrap()[0].ok, 0);
        // Asked again with nothing changed: the same answer, not a new one.
        let first = reports(&c, "home", 50).await.unwrap();
        assert!(Arc::ptr_eq(&first, &reports(&c, "home", 50).await.unwrap()));

        // A copy comes in on another disk: a.wav is now kept.
        rooms
            .store
            .put_index("home", &index("ext", &[("backup/a.wav", "A")]))
            .await
            .unwrap();
        assert_reports_match(&c, "home").await;
        assert_eq!(reports(&c, "home", 50).await.unwrap()[0].ok, 1);

        // The policy changes in the document: asked for one copy, both are.
        rooms
            .edit(&doc, "t", |d| {
                space::set_policy(
                    d,
                    &Policy {
                        id: "two".into(),
                        name: "one copy".into(),
                        scope: vec!["work".into()],
                        copies: 1,
                        media: 1,
                        offsite: 0,
                    },
                )
                .unwrap();
            })
            .await;
        assert_reports_match(&c, "home").await;
        assert_eq!(reports(&c, "home", 50).await.unwrap()[0].ok, 2);

        // The copy's location is dropped.
        rooms.store.drop_index("home", "ext").await.unwrap();
        assert_reports_match(&c, "home").await;
    }
}

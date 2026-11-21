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
}

type Q = Query<HashMap<String, String>>;

fn q<'a>(q: &'a HashMap<String, String>, k: &str) -> &'a str {
    q.get(k).map_or("", String::as_str)
}

fn fail(e: impl std::fmt::Display) -> StatusCode {
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
struct Hit {
    #[serde(flatten)]
    found: Found,
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

struct Names {
    locations: HashMap<String, space::Location>,
    devices: HashMap<String, String>,
    storages: HashMap<String, String>,
    online: Vec<String>,
    available: HashMap<String, HashMap<String, drive_files::projects::Available>>,
}

async fn names(c: &Catalog, space_id: &str) -> Names {
    let (locations, devices, storages, available) = c
        .rooms
        .read(&drive_files::space_doc(space_id), |d| {
            let locations: HashMap<String, space::Location> = space::locations(d)
                .into_iter()
                .map(|l| (l.id.clone(), l))
                .collect();
            let available = locations
                .keys()
                .map(|l| (l.clone(), drive_files::projects::available(d, l)))
                .collect();
            (
                locations,
                space::devices(d)
                    .into_iter()
                    .map(|x| (x.id, x.name))
                    .collect(),
                space::storages(d)
                    .into_iter()
                    .map(|x| (x.id, x.name))
                    .collect(),
                available,
            )
        })
        .await;
    Names {
        locations,
        devices,
        storages,
        online: c.rooms.agents_online(space_id).await,
        available,
    }
}

async fn hits(c: &Catalog, space_id: &str, found: Vec<Found>) -> Vec<Hit> {
    let n = names(c, space_id).await;
    let mut out = Vec::with_capacity(found.len());
    for f in found {
        let copies = if f.dir || f.sampled.is_empty() {
            1
        } else {
            let mut locs: Vec<String> = c
                .rooms
                .store
                .copies(space_id, &f.sampled, f.size)
                .await
                .unwrap_or_default()
                .into_iter()
                .map(|x| x.location)
                .collect();
            locs.sort();
            locs.dedup();
            locs.len()
        };
        let loc = n.locations.get(&f.location);
        let device = loc.map(|l| l.device.clone()).unwrap_or_default();
        let available = n
            .available
            .get(&f.location)
            .and_then(|m| m.get(&f.path))
            .filter(|a| a.size == f.size && a.mtime == f.mtime)
            .map(|a| a.hash.clone());
        out.push(Hit {
            available,
            location_name: loc.map(|l| l.name.clone()).unwrap_or_default(),
            device_name: n.devices.get(&device).cloned().unwrap_or_default(),
            storage: loc
                .and_then(|l| n.storages.get(&l.storage).cloned())
                .unwrap_or_default(),
            online: n.online.contains(&device),
            device,
            copies,
            found: f,
        });
    }
    out
}

/// `?space=&q=&exts=&limit=`: files and folders whose names hold every
/// word; with `exts` (`wav,aif`), only files with one of those extensions.
pub async fn search(
    State(c): State<Catalog>,
    Query(p): Q,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let space_id = q(&p, "space");
    let limit = q(&p, "limit").parse().unwrap_or(100);
    let exts: Vec<String> = q(&p, "exts").split(',').map(str::to_string).collect();
    let found = c
        .rooms
        .store
        .search(space_id, q(&p, "q"), &exts, limit)
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
    let (policies, locations, sites) = c
        .rooms
        .read(&drive_files::space_doc(space_id), |d| {
            (space::policies(d), space::locations(d), space::sites(d))
        })
        .await;
    let rows = c.rooms.store.files(space_id).await.map_err(fail)?;
    let reports = drive_files::protect::check(&policies, &locations, &sites, &rows, limit);
    Ok(Json(serde_json::json!({ "reports": reports })))
}

/// `?space=&policy=&limit=`: the files a policy finds short of copies, as
/// browsable hits, each with what it has (`kept`).
pub async fn at_risk(
    State(c): State<Catalog>,
    Query(p): Q,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let space_id = q(&p, "space");
    let limit: usize = q(&p, "limit").parse().unwrap_or(200).clamp(1, 1000);
    let (policies, locations, sites) = c
        .rooms
        .read(&drive_files::space_doc(space_id), |d| {
            (space::policies(d), space::locations(d), space::sites(d))
        })
        .await;
    let policies: Vec<_> = policies
        .into_iter()
        .filter(|x| q(&p, "policy").is_empty() || x.id == q(&p, "policy"))
        .collect();
    let rows = c.rooms.store.files(space_id).await.map_err(fail)?;
    let reports = drive_files::protect::check(&policies, &locations, &sites, &rows, limit);
    let mut found = Vec::new();
    let mut kept = Vec::new();
    for short in reports.iter().flat_map(|r| r.short.iter()) {
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
    let rows = c.rooms.store.files(space_id).await.map_err(fail)?;
    let mut exts: HashMap<String, (u64, u64)> = HashMap::new();
    let mut locations: HashMap<String, (u64, u64)> = HashMap::new();
    let (mut files, mut bytes) = (0u64, 0u64);
    for r in &rows {
        let name = r.path.rsplit('/').next().unwrap_or_default();
        let ext = name
            .rsplit_once('.')
            .map(|(_, e)| e.to_ascii_lowercase())
            .unwrap_or_default();
        let e = exts.entry(ext).or_default();
        e.0 += 1;
        e.1 += r.size;
        let l = locations.entry(r.location.clone()).or_default();
        l.0 += 1;
        l.1 += r.size;
        files += 1;
        bytes += r.size;
    }
    Ok(Json(
        serde_json::json!({ "files": files, "bytes": bytes, "exts": exts, "locations": locations }),
    ))
}

/// `?path=&name=`: one file from a location, brought from its device.
pub async fn fetch(
    UrlPath((space_id, location)): UrlPath<(String, String)>,
    State(c): State<Catalog>,
    Query(p): Q,
) -> Response {
    let path = q(&p, "path");
    let hash = match c.rooms.fetch(&space_id, &location, path).await {
        Ok(h) => h,
        Err(FetchError::NoLocation) => {
            return (StatusCode::NOT_FOUND, "no such location").into_response();
        }
        Err(FetchError::Offline(_)) => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                "the device that has this file is offline",
            )
                .into_response();
        }
        Err(FetchError::Timeout) => {
            return (StatusCode::GATEWAY_TIMEOUT, "its device did not answer").into_response();
        }
        Err(FetchError::Refused(why)) => {
            tracing::warn!(fetch.location = %location, fetch.why = %why, "a device refused a fetch");
            return (StatusCode::NOT_FOUND, why).into_response();
        }
    };
    let name = match q(&p, "name") {
        "" => path.rsplit('/').next().unwrap_or("file").to_string(),
        n => n.to_string(),
    };
    let mut query = HashMap::new();
    query.insert("name".to_string(), name);
    match crate::get_blob(UrlPath(hash), Query(query), State(c.blobs)).await {
        Ok(r) => r,
        Err(s) => s.into_response(),
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
        .list()
        .await
        .map_err(fail)?
        .into_iter()
        .filter_map(|(id, _)| id.strip_suffix("/drive:space").map(str::to_string))
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

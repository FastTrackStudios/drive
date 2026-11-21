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
    let n = c.rooms.store.put_index(&space_id, &index).await.map_err(fail)?;
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
}

struct Names {
    locations: HashMap<String, space::Location>,
    devices: HashMap<String, String>,
    storages: HashMap<String, String>,
    online: Vec<String>,
}

async fn names(c: &Catalog, space_id: &str) -> Names {
    let (locations, devices, storages) = c
        .rooms
        .read(&drive_files::space_doc(space_id), |d| {
            (
                space::locations(d).into_iter().map(|l| (l.id.clone(), l)).collect(),
                space::devices(d).into_iter().map(|x| (x.id, x.name)).collect(),
                space::storages(d).into_iter().map(|x| (x.id, x.name)).collect(),
            )
        })
        .await;
    Names { locations, devices, storages, online: c.rooms.agents_online(space_id).await }
}

async fn hits(c: &Catalog, space_id: &str, found: Vec<Found>) -> Vec<Hit> {
    let n = names(c, space_id).await;
    let mut out = Vec::with_capacity(found.len());
    for f in found {
        let copies = if f.dir || f.sampled.is_empty() {
            1
        } else {
            let mut locs: Vec<String> =
                c.rooms.store.copies(space_id, &f.sampled, f.size).await.unwrap_or_default().into_iter().map(|x| x.location).collect();
            locs.sort();
            locs.dedup();
            locs.len()
        };
        let loc = n.locations.get(&f.location);
        let device = loc.map(|l| l.device.clone()).unwrap_or_default();
        out.push(Hit {
            location_name: loc.map(|l| l.name.clone()).unwrap_or_default(),
            device_name: n.devices.get(&device).cloned().unwrap_or_default(),
            storage: loc.and_then(|l| n.storages.get(&l.storage).cloned()).unwrap_or_default(),
            online: n.online.contains(&device),
            device,
            copies,
            found: f,
        });
    }
    out
}

/// `?space=&q=&limit=`: files and folders whose names hold every word.
pub async fn search(State(c): State<Catalog>, Query(p): Q) -> Result<Json<serde_json::Value>, StatusCode> {
    let space_id = q(&p, "space");
    let limit = q(&p, "limit").parse().unwrap_or(100);
    let found = c.rooms.store.search(space_id, q(&p, "q"), limit).await.map_err(fail)?;
    Ok(Json(serde_json::json!({ "results": hits(&c, space_id, found).await })))
}

/// `?space=&location=&path=`: one folder of a location's index.
pub async fn list(State(c): State<Catalog>, Query(p): Q) -> Result<Json<serde_json::Value>, StatusCode> {
    let space_id = q(&p, "space");
    let found = c.rooms.store.list_dir(space_id, q(&p, "location"), q(&p, "path")).await.map_err(fail)?;
    Ok(Json(serde_json::json!({ "entries": hits(&c, space_id, found).await })))
}

/// `?space=&sampled=&size=`: every location holding this content.
pub async fn copies(State(c): State<Catalog>, Query(p): Q) -> Result<Json<serde_json::Value>, StatusCode> {
    let space_id = q(&p, "space");
    let size = q(&p, "size").parse().unwrap_or(0);
    let found = c.rooms.store.copies(space_id, q(&p, "sampled"), size).await.map_err(fail)?;
    let (sites, locations) = c
        .rooms
        .read(&drive_files::space_doc(space_id), |d| (space::sites(d), space::locations(d)))
        .await;
    let mut out = hits(&c, space_id, found).await;
    out.sort_by(|a, b| (a.device_name.as_str(), a.found.path.as_str()).cmp(&(b.device_name.as_str(), b.found.path.as_str())));
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
pub async fn protection(State(c): State<Catalog>, Query(p): Q) -> Result<Json<serde_json::Value>, StatusCode> {
    let space_id = q(&p, "space");
    let limit = q(&p, "limit").parse().unwrap_or(50);
    let (policies, locations, sites) = c
        .rooms
        .read(&drive_files::space_doc(space_id), |d| (space::policies(d), space::locations(d), space::sites(d)))
        .await;
    let rows = c.rooms.store.files(space_id).await.map_err(fail)?;
    let reports = drive_files::protect::check(&policies, &locations, &sites, &rows, limit);
    Ok(Json(serde_json::json!({ "reports": reports })))
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
        Err(FetchError::NoLocation) => return (StatusCode::NOT_FOUND, "no such location").into_response(),
        Err(FetchError::Offline(_)) => {
            return (StatusCode::SERVICE_UNAVAILABLE, "the device that has this file is offline").into_response();
        }
        Err(FetchError::Timeout) => return (StatusCode::GATEWAY_TIMEOUT, "its device did not answer").into_response(),
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

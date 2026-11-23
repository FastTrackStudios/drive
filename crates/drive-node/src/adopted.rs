//! The Drive tree as a logical path names it, adopted folders included
//! (docs/adopt.md): what the mount shows, for the app, `drive ls` and the
//! MCP tools, which do not mount.
//!
//! `GET /tree?space=&path=` lists a logical folder:
//!
//! - inside an adopted folder, the location's index at the mapped folder
//!   (one indexed query, as `/list`), each entry with where it is;
//! - elsewhere, the Drive tree's own entries with the adopted folders
//!   (and the folders on the way to them) added. An adopted folder hides
//!   a tree entry of the same name, as in the mount.
//!
//! `GET /tree/file?space=&path=` reads a file by its logical path: a tree
//! file from its blob, an adopted one through its location (`/fetch`).
//!
//! The adoptions come from the space's names, read once per version of
//! the space document (`catalog::names`); nothing walks an index.

use std::collections::HashMap;

use axum::Json;
use axum::extract::{Path as UrlPath, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use drive_files::adopt::{self, Adoption, Place};
use drive_files::tree;
use serde_json::{Value, json};
use tracing::field::Empty;

use crate::catalog::{Catalog, Names, Q, fail, hits, names, q};

/// A logical path made canonical (`""`: the top); `None` when it climbs.
fn logical(raw: &str) -> Option<String> {
    let parts: Vec<&str> = raw.split('/').filter(|p| !p.is_empty()).collect();
    if parts.iter().any(|p| *p == "." || *p == "..") {
        return None;
    }
    Some(parts.join("/"))
}

/// Where an adopted folder's bytes are: its device and location, the
/// path inside the location, and the real path on that device (as the
/// mount's `user.drive.where`).
fn where_of(n: &Names, a: &Adoption, loc_path: &str) -> Value {
    let loc = n.doc.locations.get(&a.location);
    let device = loc.map(|l| l.device.clone()).unwrap_or_default();
    let real = loc.map_or(String::new(), |l| {
        if loc_path.is_empty() {
            l.path.clone()
        } else {
            format!("{}/{loc_path}", l.path.trim_end_matches('/'))
        }
    });
    json!({
        "adopted_at": a.at,
        "location": a.location,
        "location_name": loc.map(|l| l.name.clone()).unwrap_or_default(),
        "device": device,
        "device_name": n.doc.devices.get(&device).cloned().unwrap_or_default(),
        "online": n.online.contains(&device),
        "path": loc_path,
        "real": real,
    })
}

/// `?space=&path=`: a logical folder of the Drive tree.
#[tracing::instrument(name = "drive.tree.list", skip_all, fields(
    tree.space = %q(&p, "space"),
    tree.place = Empty,
    tree.entries = Empty,
    tree.outcome = Empty,
))]
pub async fn list(State(c): State<Catalog>, Query(p): Q) -> Result<Json<Value>, StatusCode> {
    let span = tracing::Span::current();
    let r = list_inner(&c, &p).await;
    match &r {
        Ok(v) => {
            span.record("tree.place", v["place"].as_str().unwrap_or_default());
            span.record(
                "tree.entries",
                v["entries"].as_array().map_or(0, Vec::len) as u64,
            );
            span.record("tree.outcome", "listed");
        }
        Err(s) => {
            span.record("tree.outcome", s.as_u16() as u64);
        }
    }
    r.map(Json)
}

async fn list_inner(c: &Catalog, p: &HashMap<String, String>) -> Result<Value, StatusCode> {
    let space_id = q(p, "space");
    let path = logical(q(p, "path")).ok_or(StatusCode::BAD_REQUEST)?;
    let n = names(c, space_id).await;
    let all = &n.doc.adoptions;
    let place = adopt::place(all, &path);
    let loc_path = place.location_path();
    if let Place::Inside { adoption, .. } = place {
        let loc_path = loc_path.unwrap_or_default();
        if !loc_path.is_empty() {
            match c
                .rooms
                .store
                .entry(space_id, &adoption.location, &loc_path)
                .await
                .map_err(fail)?
            {
                Some(e) if e.dir => {}
                Some(_) => return Err(StatusCode::BAD_REQUEST),
                None => return Err(StatusCode::NOT_FOUND),
            }
        }
        let found = c
            .rooms
            .store
            .list_dir(space_id, &adoption.location, &loc_path)
            .await
            .map_err(fail)?;
        let entries: Vec<Value> = hits(c, space_id, found)
            .await
            .into_iter()
            .map(|h| {
                let name = h.found.name.clone();
                let mut v = serde_json::to_value(&h).unwrap_or_default();
                v["source"] = json!("adopted");
                v["logical"] = json!(adopt::join(&path, &name));
                v
            })
            .collect();
        return Ok(json!({
            "path": path,
            "place": "inside",
            "where": where_of(&n, adoption, &loc_path),
            "entries": entries,
        }));
    }
    let added = adopt::names_in(all, &path);
    let tree_rows = c
        .rooms
        .read(&drive_files::files_doc(space_id), |d| {
            match tree::resolve(d, &path) {
                Some(None) => Some(tree::children(d, None)),
                Some(Some(e)) if e.dir => Some(tree::children(d, Some(e.id))),
                _ => None,
            }
        })
        .await;
    if tree_rows.is_none() && added.is_empty() {
        return Err(StatusCode::NOT_FOUND);
    }
    let mut entries: Vec<Value> = tree_rows
        .unwrap_or_default()
        .into_iter()
        // An adopted folder hides a tree entry of the same name.
        .filter(|e| !added.iter().any(|(name, point)| *point && *name == e.name))
        .map(|e| {
            json!({
                "source": "tree",
                "name": e.name,
                "dir": e.dir,
                "size": e.size,
                "mtime": e.mtime,
                "hash": e.hash,
                "id": tree::id_string(e.id),
                "logical": adopt::join(&path, &e.name),
            })
        })
        .collect();
    for (name, point) in added {
        let at = adopt::join(&path, &name);
        if point {
            let Some(a) = all.iter().find(|a| a.at == at) else {
                continue;
            };
            entries.push(json!({
                "source": "adopted",
                "name": name,
                "dir": true,
                "logical": at,
                "where": where_of(&n, a, &a.sub),
            }));
        } else if !entries
            .iter()
            .any(|e| e["name"] == name.as_str() && e["dir"] == true)
        {
            // A folder that is there only on the way to adoptions.
            entries.push(json!({
                "source": "above",
                "name": name,
                "dir": true,
                "logical": at,
            }));
        }
    }
    entries.sort_by_key(|e| {
        (
            e["dir"] != true,
            e["name"].as_str().unwrap_or_default().to_lowercase(),
        )
    });
    Ok(json!({
        "path": path,
        "place": if matches!(place, Place::Above) { "above" } else { "tree" },
        "entries": entries,
    }))
}

/// `?space=&path=`: a file by its logical path. A tree file is its blob;
/// an adopted one is read through its location: the node's copy when it
/// is kept available, else its device, as `/fetch` does.
#[tracing::instrument(name = "drive.tree.file", skip_all, fields(
    tree.space = %q(&p, "space"),
    tree.place = Empty,
    tree.outcome = Empty,
))]
pub async fn file(
    State(c): State<Catalog>,
    Query(p): Q,
    headers: axum::http::HeaderMap,
) -> Response {
    let span = tracing::Span::current();
    let space_id = q(&p, "space").to_string();
    let Some(path) = logical(q(&p, "path")).filter(|x| !x.is_empty()) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let name = path.rsplit('/').next().unwrap_or("file").to_string();
    let mut query = HashMap::new();
    query.insert("name".to_string(), name);
    if crate::wants_inline(&p) {
        query.insert("inline".to_string(), "1".to_string());
    }
    let n = names(&c, &space_id).await;
    let place = adopt::place(&n.doc.adoptions, &path);
    let loc_path = place.location_path();
    if let Place::Inside { adoption, .. } = place {
        span.record("tree.place", "inside");
        let loc_path = loc_path.unwrap_or_default();
        let found = match c
            .rooms
            .store
            .entry(&space_id, &adoption.location, &loc_path)
            .await
        {
            Ok(Some(f)) if !f.dir => f,
            Ok(_) => {
                span.record("tree.outcome", "missing");
                return StatusCode::NOT_FOUND.into_response();
            }
            Err(e) => return fail(e).into_response(),
        };
        let kept = n
            .doc
            .available
            .get(&adoption.location)
            .and_then(|m| m.get(&loc_path))
            .filter(|a| a.size == found.size && a.mtime == found.mtime)
            .map(|a| a.hash.clone());
        if let Some(hash) = kept {
            span.record("tree.outcome", "node_copy");
            return blob(&c, hash, query, headers).await;
        }
        span.record("tree.outcome", "fetched");
        query.insert("path".to_string(), loc_path);
        let location = adoption.location.clone();
        return crate::catalog::fetch(
            UrlPath((space_id, location)),
            State(c),
            Query(query),
            headers,
        )
        .await;
    }
    span.record("tree.place", "tree");
    let e = c
        .rooms
        .read(&drive_files::files_doc(&space_id), |d| {
            tree::resolve(d, &path).flatten()
        })
        .await;
    match e {
        Some(e) if !e.dir && !e.hash.is_empty() => {
            span.record("tree.outcome", "blob");
            blob(&c, e.hash, query, headers).await
        }
        _ => {
            span.record("tree.outcome", "missing");
            StatusCode::NOT_FOUND.into_response()
        }
    }
}

async fn blob(
    c: &Catalog,
    hash: String,
    query: HashMap<String, String>,
    headers: axum::http::HeaderMap,
) -> Response {
    match crate::get_blob_ranged(UrlPath(hash), Query(query), headers, State(c.blobs.clone())).await
    {
        Ok(r) => r,
        Err(s) => s.into_response(),
    }
}

#[cfg(test)]
mod tests;

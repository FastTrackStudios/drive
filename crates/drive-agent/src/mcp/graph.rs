//! The graph tools (read only): backlinks, neighbours, a shortest path and
//! everything under a tag, asked of this peer's own graph (docs/graph.md),
//! brought up to date first from what it has synced.

use drive_graph::db::GraphDb;
use drive_graph::graph::{Edge, Node, target_node};
use drive_graph::index::{Hit, Index, MAX_PATH};
use drive_graph::refresh::Report;
use serde_json::{Value, json};

use super::ctx::{Ctx, page};
use super::tools::opt_str;
use crate::graph::{by_name, raw_key, shown, tag_key, up_to_date};

/// The deepest `graph_neighbors` walks.
const MAX_DEPTH: u64 = 4;

async fn graph(ctx: &Ctx) -> Result<(GraphDb, Report), String> {
    up_to_date(&ctx.a, &ctx.e.tags).await
}

fn node_json(n: &Node) -> Value {
    json!({ "id": n.id, "kind": n.kind.id(), "name": n.label, "thing": shown(n) })
}

fn edge_json(e: &Edge) -> Value {
    json!({ "kind": e.kind.id(), "from": e.src, "to": e.dst, "written": e.raw, "anchor": e.anchor })
}

fn hit_json(h: &Hit) -> Value {
    let mut v = node_json(&h.node);
    v["edge"] = edge_json(&h.edge);
    v
}

/// What the graph read, so an agent knows when notes were out of reach.
fn coverage(r: &Report) -> Value {
    json!({ "notes": r.notes, "read": r.notes - r.unread, "out_of_reach": r.unread })
}

/// A THING, a graph key, or a name the graph knows, as a node id.
async fn node_key(ctx: &Ctx, db: &GraphDb, thing: &str) -> Result<String, String> {
    if let Some(k) = raw_key(thing.trim()) {
        return Ok(k.to_string());
    }
    match ctx.thing(thing).await.and_then(|t| t.target()) {
        Ok(target) => Ok(target_node(&target)),
        Err(e) => by_name(db, thing)
            .await
            .map(|n| n.id)
            .map_err(|n| format!("{e}; {n}")),
    }
}

fn need<'a>(args: &'a Value, k: &str) -> Result<&'a str, String> {
    opt_str(args, k).ok_or_else(|| format!("needs {k}"))
}

pub async fn backlinks(ctx: &Ctx, args: &Value) -> Result<Value, String> {
    let (db, report) = graph(ctx).await?;
    let id = node_key(ctx, &db, need(args, "thing")?).await?;
    let hits = db.backlinks(&id).await.map_err(|e| e.to_string())?;
    let (offset, limit) = ctx.paging(args);
    let mut v = page(hits.iter().map(hit_json).collect(), offset, limit);
    v["node"] = json!(id);
    v["coverage"] = coverage(&report);
    Ok(v)
}

pub async fn neighbors(ctx: &Ctx, args: &Value) -> Result<Value, String> {
    let (db, report) = graph(ctx).await?;
    let id = node_key(ctx, &db, need(args, "thing")?).await?;
    let depth = args["depth"].as_u64().unwrap_or(1).clamp(1, MAX_DEPTH);
    let near = db
        .neighbors(&id, u32::try_from(depth).unwrap_or(1))
        .await
        .map_err(|e| e.to_string())?;
    let (offset, limit) = ctx.paging(args);
    let rows: Vec<Value> = near
        .iter()
        .map(|n| {
            let mut v = node_json(&n.node);
            v["depth"] = json!(n.depth);
            v
        })
        .collect();
    let mut v = page(rows, offset, limit);
    v["node"] = json!(id);
    v["depth"] = json!(depth);
    v["coverage"] = coverage(&report);
    Ok(v)
}

pub async fn path(ctx: &Ctx, args: &Value) -> Result<Value, String> {
    let (db, report) = graph(ctx).await?;
    let from = node_key(ctx, &db, need(args, "from")?).await?;
    let to = node_key(ctx, &db, need(args, "to")?).await?;
    let max = args["max"].as_u64().map_or(MAX_PATH, |m| {
        u32::try_from(m.min(u64::from(MAX_PATH))).unwrap_or(MAX_PATH)
    });
    let steps = db.path(&from, &to, max).await.map_err(|e| e.to_string())?;
    Ok(json!({
        "from": from,
        "to": to,
        "found": steps.is_some(),
        "steps": steps.unwrap_or_default().iter().map(|s| {
            let mut v = node_json(&s.node);
            v["edge"] = s.edge.as_ref().map_or(Value::Null, edge_json);
            v
        }).collect::<Vec<_>>(),
        "coverage": coverage(&report),
    }))
}

pub async fn tagged(ctx: &Ctx, args: &Value) -> Result<Value, String> {
    let (db, report) = graph(ctx).await?;
    let key = tag_key(&ctx.tags(), need(args, "tag")?)?;
    let hits = db.under_tag(&key).await.map_err(|e| e.to_string())?;
    let (offset, limit) = ctx.paging(args);
    let mut v = page(hits.iter().map(hit_json).collect(), offset, limit);
    v["tag"] = json!(key);
    v["coverage"] = coverage(&report);
    Ok(v)
}

/// `link` / `unlink`: one link of an app's kind between two things, in
/// `drive:links` (synced to every peer; the graph reads it).
pub async fn link(ctx: &Ctx, args: &Value, add: bool) -> Result<Value, String> {
    let from = ctx.thing(need(args, "from")?).await?;
    let to = ctx.thing(need(args, "to")?).await?;
    let kind = opt_str(args, "kind").unwrap_or_default().trim().to_string();
    let (ft, tt) = (from.target()?, to.target()?);
    let (id, doc) = crate::graph::links_doc(&ctx.a).await;
    let exists = drive_spine::links::from(&doc, &ft)
        .iter()
        .any(|l| l.to == tt && l.kind == kind);
    let change = json!({
        "from": from.label(), "to": to.label(), "kind": kind,
        "change": if add { "link" } else { "unlink" },
        "already": exists == add,
    });
    if super::tools::dry(args) {
        return Ok(json!({ "dry_run": true, "would": change }));
    }
    if exists == add {
        return Ok(json!({ "done": change }));
    }
    ctx.charge(1)?;
    if add {
        drive_spine::links::link(&doc, &ft, &tt, &kind, &ctx.author, drive_files::now_ms())
    } else {
        drive_spine::links::unlink(&doc, &ft, &tt, &kind)
    }
    .map_err(|e| e.to_string())?;
    ctx.a.drive.commit(&id);
    Ok(json!({ "done": change }))
}

/// `graph_search`: notes, files, tags and documents by their words.
pub async fn search(ctx: &Ctx, args: &Value) -> Result<Value, String> {
    let (db, report) = graph(ctx).await?;
    let (offset, limit) = ctx.paging(args);
    let near = match opt_str(args, "near") {
        Some(n) => Some(node_key(ctx, &db, n).await?),
        None => None,
    };
    let model = crate::graph::model(&ctx.a.dir);
    let q = crate::graph::Query {
        words: need(args, "query")?,
        tag: opt_str(args, "tag"),
        near: near.as_deref(),
        limit: offset + limit,
    };
    let found = crate::graph::search(
        &db,
        model.map(|m| m as &dyn drive_graph::embed::Embedder),
        &ctx.tags(),
        &q,
    )
    .await?;
    let rows: Vec<Value> = found
        .iter()
        .map(|f| {
            let mut v = node_json(&f.node);
            v["score"] = json!(f.score);
            v
        })
        .collect();
    let mut v = page(rows, offset, limit);
    v["coverage"] = coverage(&report);
    v["by"] = json!(if model.is_some() {
        "words and meaning"
    } else {
        "words (no model on this device)"
    });
    Ok(v)
}

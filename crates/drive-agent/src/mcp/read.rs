//! The tools that only look.

use std::collections::{BTreeMap, HashMap, HashSet};

use drive_files::index::{IndexEntry, Tree};
use drive_files::{adopt, projects, space, tree};
use drive_rules::{doc, eval, journal};
use drive_spine::kinds::Kinds;
use drive_spine::tags::query::{self, Search, Subject};
use drive_spine::tags::{Library, Target};
use serde_json::{Value, json};

use super::ctx::{Ctx, Thing, human, page};
use super::tools::opt_str;

pub async fn devices(ctx: &Ctx) -> Result<Value, String> {
    let on = ctx.online();
    let me = ctx.a.device_id();
    let vols = space::volumes(&ctx.a.space);
    let locs = space::locations(&ctx.a.space);
    let out: Vec<Value> = space::devices(&ctx.a.space)
        .into_iter()
        .map(|d| {
            let volumes: Vec<Value> = vols
                .iter()
                .filter(|v| v.device == d.id)
                .map(|v| {
                    json!({
                        "name": v.name, "mount": v.mount, "fs": v.fs,
                        "total": v.total, "free": v.free,
                        "free_human": human(v.free.max(0) as u64),
                        "removable": v.removable, "seen": v.seen
                    })
                })
                .collect();
            json!({
                "id": d.id, "name": d.name, "kind": d.kind, "os": d.os, "roles": d.roles,
                "online": on.contains(&d.id), "this_device": d.id == me,
                "locations": locs.iter().filter(|l| l.device == d.id).count(),
                "volumes": volumes
            })
        })
        .collect();
    Ok(json!({ "devices": out, "online_means": "its Drive agent is connected now" }))
}

pub async fn locations(ctx: &Ctx, args: &Value) -> Result<Value, String> {
    let only = opt_str(args, "device")
        .map(|d| ctx.device_id(d))
        .transpose()?;
    let names = ctx.device_names();
    let on = ctx.online();
    let storages = space::storages(&ctx.a.space);
    let out: Vec<Value> = space::locations(&ctx.a.space)
        .into_iter()
        .filter(|l| only.as_ref().is_none_or(|d| *d == l.device))
        .map(|l| {
            json!({
                "id": l.id, "name": l.name,
                "device": names.get(&l.device).cloned().unwrap_or_else(|| l.device.clone()),
                "online": on.contains(&l.device),
                "path": l.path,
                "storage": storages.iter().find(|s| s.id == l.storage).map(|s| s.name.clone()),
                "mirror_of": (!l.mirror_of.is_empty()).then_some(l.mirror_of.clone()),
                "files": l.files, "bytes": l.bytes, "size": human(l.bytes.max(0) as u64),
                "indexed_at": l.built
            })
        })
        .collect();
    Ok(json!({ "locations": out }))
}

pub async fn browse(ctx: &Ctx, args: &Value) -> Result<Value, String> {
    let (offset, limit) = ctx.paging(args);
    let path = opt_str(args, "path")
        .unwrap_or("")
        .trim_matches('/')
        .to_string();
    if path.split('/').any(|p| p == "..") {
        return Err("refused: a path may not climb out with ..".into());
    }
    if let Some(d) = opt_str(args, "drive") {
        return browse_drive(ctx, d, offset, limit).await;
    }
    if let Some(l) = opt_str(args, "location") {
        let loc = ctx.location(l)?;
        let t = Tree::new(ctx.index_of(&loc).await?);
        if !path.is_empty() && t.get(&path).is_none_or(|e| !e.dir) {
            return Err(format!(
                "{path} is not a folder of {} (as of its last index)",
                loc.name
            ));
        }
        let mut entries: Vec<&IndexEntry> = t.list(&path);
        entries.sort_by(|a, b| {
            b.dir
                .cmp(&a.dir)
                .then_with(|| a.name().to_lowercase().cmp(&b.name().to_lowercase()))
        });
        let rows: Vec<Value> = entries
            .into_iter()
            .map(|e| json!({ "name": e.name(), "dir": e.dir, "size": e.size, "mtime": e.mtime }))
            .collect();
        let mut v = page(rows, offset, limit);
        v["location"] = json!(loc.name);
        v["path"] = json!(path);
        v["from"] = json!("the location's index");
        return Ok(v);
    }
    let device =
        opt_str(args, "device").ok_or("browse needs a drive folder, a location, or a device")?;
    let id = ctx.device_id(device)?;
    let base = format!("/browse/{}/{id}", ctx.space);
    let Some(root) = opt_str(args, "root") else {
        let v = ctx.get(&format!("{base}/roots"), &[]).await?;
        return Ok(json!({ "device": device, "online": v["online"], "roots": v["roots"] }));
    };
    let (off, lim) = (offset.to_string(), limit.to_string());
    let hidden = if args["hidden"].as_bool() == Some(true) {
        "1"
    } else {
        "0"
    };
    let v = ctx
        .get(
            &format!("{base}/list"),
            &[
                ("root", root),
                ("path", &path),
                ("offset", &off),
                ("limit", &lim),
                ("hidden", hidden),
            ],
        )
        .await?;
    let total = v["total"].as_u64().unwrap_or(0) as usize;
    let n = v["entries"].as_array().map_or(0, Vec::len);
    Ok(json!({
        "device": device, "root": root, "path": path, "online": v["online"],
        "total": total, "offset": offset,
        "next_offset": (offset + n < total).then_some(offset + n),
        "hidden_left_out": v["hidden"],
        "as_of": v["as_of"],
        "items": v["entries"]
    }))
}

/// A folder of the Drive tree, adopted folders included (docs/adopt.md):
/// inside one, the location's index at the adopted folder, each entry
/// also named as `LOCATION:PATH`; elsewhere the tree's entries with the
/// adopted folders (and the folders on the way to them) added.
async fn browse_drive(ctx: &Ctx, raw: &str, offset: usize, limit: usize) -> Result<Value, String> {
    let path = raw.trim_start_matches("drive:").trim_matches('/');
    if path.split('/').any(|p| p == "..") {
        return Err("refused: a path may not climb out with ..".into());
    }
    let all = ctx.a.adoptions();
    if let adopt::Place::Inside { adoption, rel } = adopt::place(&all, path) {
        let loc = ctx.location(&adoption.location)?;
        let lp = adopt::join(&adoption.sub, &rel);
        let t = Tree::new(ctx.index_of(&loc).await?);
        if !lp.is_empty() && t.get(&lp).is_none_or(|e| !e.dir) {
            return Err(format!(
                "drive:{path} is not a folder (as of {}'s last index)",
                loc.name
            ));
        }
        let mut entries: Vec<&IndexEntry> = t.list(&lp);
        entries.sort_by(|a, b| {
            b.dir
                .cmp(&a.dir)
                .then_with(|| a.name().to_lowercase().cmp(&b.name().to_lowercase()))
        });
        let rows: Vec<Value> = entries
            .into_iter()
            .map(|e| {
                json!({
                    "name": e.name(), "dir": e.dir, "size": e.size, "mtime": e.mtime,
                    "thing": format!("drive:{}", adopt::join(path, e.name())),
                    "located": format!("{}:{}", loc.name, e.path),
                })
            })
            .collect();
        let mut v = page(rows, offset, limit);
        v["drive"] = json!(path);
        v["adopted_at"] = json!(adoption.at);
        v["where"] = json!(ctx.a.whereabouts(adoption));
        v["from"] = json!("the adopted folder's location index");
        return Ok(v);
    }
    let added = adopt::names_in(&all, path);
    let folder = match tree::resolve(&ctx.a.files, path) {
        Some(None) => Some(None),
        Some(Some(e)) if e.dir => Some(Some(e.id)),
        _ if !added.is_empty() => None,
        _ => return Err(format!("drive:{path} is not a folder of the Drive")),
    };
    let mut rows: Vec<Value> = folder
        .map(|f| tree::children(&ctx.a.files, f))
        .unwrap_or_default()
        .into_iter()
        // An adopted folder hides a tree entry of the same name.
        .filter(|e| !added.iter().any(|(n, point)| *point && *n == e.name))
        .map(|e| {
            json!({
                "name": e.name, "dir": e.dir, "size": e.size, "mtime": e.mtime,
                "thing": format!("drive:{}", adopt::join(path, &e.name)),
            })
        })
        .collect();
    for (name, point) in added {
        if rows.iter().any(|r| r["name"] == name.as_str()) {
            continue;
        }
        let at = adopt::join(path, &name);
        let mut r = json!({ "name": name, "dir": true, "thing": format!("drive:{at}") });
        if point && let Some(a) = all.iter().find(|a| a.at == at) {
            r["adopted"] = json!(true);
            r["where"] = json!(ctx.a.whereabouts(a));
        }
        rows.push(r);
    }
    rows.sort_by_key(|r| {
        (
            r["dir"] != true,
            r["name"].as_str().unwrap_or_default().to_lowercase(),
        )
    });
    let mut v = page(rows, offset, limit);
    v["drive"] = json!(path);
    v["from"] = json!("the Drive tree");
    Ok(v)
}

pub async fn search(ctx: &Ctx, args: &Value) -> Result<Value, String> {
    let (offset, limit) = ctx.paging(args);
    let q = opt_str(args, "query").ok_or("search needs a query")?;
    let lib = ctx.tags();
    let s = Search::new(q, &lib)
        .map_err(|e| format!("the search: {}", e.message))?
        .ok_or("search for what?")?;
    let kinds = Kinds::load(&ctx.a.spine);
    let mut found: Vec<Value> = Vec::new();
    let labels = |on: &drive_spine::tags::OnTarget| -> Vec<String> {
        on.tags.iter().map(|a| lib.label(&a.tag)).collect()
    };
    for (path, e) in tree::files(&ctx.a.files) {
        let on = lib.on(&Target::File(tree::id_string(e.id)));
        let k = kinds.of_file(&e.name, false);
        let words = query::kind_words(&kinds, k);
        if s.matches(&Subject {
            name: &e.name,
            path: &path,
            kinds: &words,
            on: &on,
        }) {
            found.push(json!({
                "thing": format!("drive:{path}"), "in": "drive", "kind": k.id,
                "size": e.size, "mtime": e.mtime, "tags": labels(&on)
            }));
        }
    }
    let hints = s.hints();
    let mut asks: Vec<(&str, String)> = Vec::new();
    if hints.tagged {
        let hashes: Vec<String> = lib
            .targets()
            .filter_map(|(t, _)| match t {
                Target::Content(h) => Some(h),
                _ => None,
            })
            .collect();
        if !hashes.is_empty() {
            asks.push(("sampled", hashes.join(",")));
        }
    }
    for w in &hints.words {
        asks.push(("q", w.clone()));
    }
    for k in &hints.kinds {
        asks.push(("exts", query::kind_extensions(&kinds, k).join(",")));
    }
    let mut seen = HashSet::new();
    let adopted = ctx.a.adoptions();
    let want = (offset + limit).clamp(100, 2000).to_string();
    for (key, val) in asks {
        let v = ctx
            .get(
                "/search",
                &[
                    ("space", ctx.space.as_str()),
                    ("limit", want.as_str()),
                    (key, val.as_str()),
                ],
            )
            .await?;
        for h in v["results"].as_array().into_iter().flatten() {
            let path = h["path"].as_str().unwrap_or_default();
            let loc = h["location"].as_str().unwrap_or_default();
            if !seen.insert(format!("{loc}/{path}")) {
                continue;
            }
            let dir = h["dir"].as_bool() == Some(true);
            let name = h["name"]
                .as_str()
                .unwrap_or_else(|| path.rsplit('/').next().unwrap_or(path));
            let on = lib.on(&Target::Content(
                h["sampled"].as_str().unwrap_or_default().to_string(),
            ));
            let k = kinds.of_file(name, dir);
            let words = query::kind_words(&kinds, k);
            if s.matches(&Subject {
                name,
                path,
                kinds: &words,
                on: &on,
            }) {
                let lname = h["location_name"].as_str().unwrap_or("?");
                // Where it shows in the Drive tree too, when adopted.
                let drive = adopt::logical_of(&adopted, loc, path).map(|p| format!("drive:{p}"));
                found.push(json!({
                    "drive": drive,
                    "thing": format!("{lname}:{path}"), "in": "computer",
                    "device": h["device_name"], "location": lname, "online": h["online"],
                    "dir": dir, "kind": k.id, "size": h["size"], "mtime": h["mtime"],
                    "copies": h["copies"], "tags": labels(&on)
                }));
            }
        }
    }
    let projects = projects::projects(&ctx.a.space);
    for (target, on) in lib.targets() {
        let (label, name) = match &target {
            Target::Doc(id) => ("doc", id.clone()),
            Target::Project(id) => (
                "project",
                projects
                    .iter()
                    .find(|p| p.id == *id)
                    .map_or(id.clone(), |p| p.name.clone()),
            ),
            _ => continue,
        };
        if s.matches(&Subject {
            name: &name,
            path: &name,
            kinds: &[],
            on,
        }) {
            found.push(
                json!({ "thing": format!("{label}:{name}"), "in": label, "tags": labels(on) }),
            );
        }
    }
    let mut v = page(found, offset, limit);
    if hints.open {
        v["note"] = json!(
            "NOT, special: and field searches see the Drive and tagged files, not every file on every computer"
        );
    }
    Ok(v)
}

pub async fn file_info(ctx: &Ctx, args: &Value) -> Result<Value, String> {
    let t = ctx
        .thing(opt_str(args, "thing").ok_or("file_info needs a thing")?)
        .await?;
    let lib = ctx.tags();
    let kinds = Kinds::load(&ctx.a.spine);
    let on = t.target().ok().map(|x| lib.on(&x)).unwrap_or_default();
    let mut v = json!({
        "thing": t.label(),
        "tags": on.tags.iter().map(|a| json!({ "tag": lib.label(&a.tag), "by": a.by, "at": a.at })).collect::<Vec<_>>(),
        "fields": on.fields.iter().map(|f| json!({ "name": f.name, "value": f.value, "kind": f.kind.id() })).collect::<Vec<_>>(),
    });
    let names = ctx.device_names();
    // The content key a sidecar is under, and the copies.
    let mut sidecar_key = None;
    let mut sampled = None;
    match &t {
        Thing::Drive { entry, path } => {
            v["in"] = json!("drive");
            v["name"] = json!(entry.name);
            v["dir"] = json!(entry.dir);
            v["kind"] = json!(kinds.of_file(&entry.name, entry.dir).id);
            v["size"] = json!(entry.size);
            v["modified"] = json!(entry.mtime);
            v["hash"] = json!(entry.hash);
            v["in_trash"] = json!(tree::in_trash(&ctx.a.files, entry.id));
            v["held_by"] = json!(ctx.a.holders(&entry.hash));
            v["path"] = json!(path);
            if !entry.hash.is_empty() {
                sidecar_key = Some(entry.hash.clone());
            }
        }
        Thing::Located { loc, entry } => {
            v["in"] = json!("computer");
            v["name"] = json!(entry.name());
            v["dir"] = json!(entry.dir);
            v["kind"] = json!(kinds.of_file(entry.name(), entry.dir).id);
            v["size"] = json!(entry.size);
            v["modified"] = json!(entry.mtime);
            v["device"] = json!(names.get(&loc.device));
            v["location"] = json!(loc.name);
            v["path"] = json!(entry.path);
            v["projects"] = json!(ctx.projects_holding(&loc.id, &entry.path));
            if !entry.sampled.is_empty() {
                sampled = Some((entry.sampled.clone(), entry.size));
            }
        }
        Thing::Local {
            path,
            sampled: h,
            size,
        } => {
            v["in"] = json!("this computer");
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            v["kind"] = json!(kinds.of_file(&name, false).id);
            v["name"] = json!(name);
            v["size"] = json!(size);
            sampled = Some((h.clone(), *size));
        }
        Thing::Project(p) => {
            v["in"] = json!("project");
            v["project"] = json!(p);
        }
        Thing::Doc(id) => v["in"] = json!(format!("doc {id}")),
        Thing::Content(h) => {
            v["in"] = json!("content");
            v["content"] = json!(h);
        }
    }
    if let Some((h, size)) = sampled {
        v["content"] = json!(format!("content:{h}"));
        sidecar_key = Some(format!("s{h}"));
        let sz = size.to_string();
        match ctx
            .get(
                "/copies",
                &[
                    ("space", ctx.space.as_str()),
                    ("sampled", h.as_str()),
                    ("size", sz.as_str()),
                ],
            )
            .await
        {
            Ok(c) => {
                let copies: Vec<Value> = c["copies"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|c| {
                        json!({
                            "thing": format!("{}:{}", c["location_name"].as_str().unwrap_or("?"), c["path"].as_str().unwrap_or("")),
                            "device": c["device_name"], "online": c["online"], "site": c["site"]
                        })
                    })
                    .collect();
                v["copies"] = json!(copies);
                v["copies_note"] =
                    json!("copies are judged by a sampled hash and the size: likely, not certain");
            }
            Err(e) => v["copies_error"] = json!(e),
        }
    }
    if let Some(key) = sidecar_key {
        let mut have = BTreeMap::new();
        for kind in ["thumb-256", "peaks"] {
            have.insert(
                kind,
                ctx.exists(&format!("/sidecar/{key}/{kind}"), &[("make", "0")])
                    .await,
            );
        }
        v["sidecars"] = json!(have);
    }
    Ok(v)
}

/// Rows of a folder: (path relative to the folder's parent root, dir, size, mtime, content).
struct Row {
    path: String,
    dir: bool,
    size: u64,
    mtime: i64,
    content: String,
}

pub async fn folder_summary(ctx: &Ctx, args: &Value) -> Result<Value, String> {
    let top = args["top"].as_u64().unwrap_or(10).clamp(1, 50) as usize;
    let (label, rows): (String, Vec<Row>) = if let Some(l) = opt_str(args, "location") {
        let loc = ctx.location(l)?;
        let path = opt_str(args, "path")
            .unwrap_or("")
            .trim_matches('/')
            .to_string();
        let idx = ctx.index_of(&loc).await?;
        let prefix = format!("{path}/");
        let rows = idx
            .entries
            .iter()
            .cloned()
            .filter(|e| path.is_empty() || e.path.starts_with(&prefix))
            .map(|e| Row {
                path: e.path,
                dir: e.dir,
                size: e.size,
                mtime: e.mtime,
                content: e.sampled,
            })
            .collect();
        (format!("{}:{path}", loc.name), rows)
    } else if let Some(dp) = args.get("drive_path").and_then(Value::as_str) {
        let dp = dp.trim_matches('/');
        let prefix = format!("{dp}/");
        let mut rows: Vec<Row> = tree::files(&ctx.a.files)
            .into_iter()
            .filter(|(p, _)| dp.is_empty() || p.starts_with(&prefix))
            .map(|(p, e)| Row {
                path: p,
                dir: false,
                size: e.size,
                mtime: e.mtime,
                content: e.hash,
            })
            .collect();
        // Folders, from the files' paths.
        let dirs: HashSet<String> = rows
            .iter()
            .filter_map(|r| r.path.rsplit_once('/').map(|(d, _)| d.to_string()))
            .filter(|d| dp.is_empty() || d.starts_with(&prefix))
            .collect();
        rows.extend(dirs.into_iter().map(|d| Row {
            path: d,
            dir: true,
            size: 0,
            mtime: 0,
            content: String::new(),
        }));
        (format!("drive:{dp}"), rows)
    } else {
        return Err("folder_summary needs a location (and path) or a drive_path".into());
    };
    let kinds = Kinds::load(&ctx.a.spine);
    let files: Vec<&Row> = rows.iter().filter(|r| !r.dir).collect();
    let mut by_kind: HashMap<String, (u64, u64)> = HashMap::new();
    let mut by_ext: HashMap<String, (u64, u64)> = HashMap::new();
    let mut groups: HashMap<&str, Vec<&Row>> = HashMap::new();
    for f in &files {
        let name = f.path.rsplit('/').next().unwrap_or(&f.path);
        let k = kinds.of_file(name, false);
        let e = by_kind.entry(k.id.clone()).or_default();
        e.0 += 1;
        e.1 += f.size;
        let ext = name
            .rsplit_once('.')
            .map(|(_, x)| x.to_ascii_lowercase())
            .unwrap_or_default();
        let e = by_ext.entry(ext).or_default();
        e.0 += 1;
        e.1 += f.size;
        if !f.content.is_empty() && f.size > 0 {
            groups.entry(f.content.as_str()).or_default().push(f);
        }
    }
    let tally = |m: HashMap<String, (u64, u64)>, n: usize| -> Vec<Value> {
        let mut v: Vec<_> = m.into_iter().collect();
        v.sort_by_key(|x| std::cmp::Reverse(x.1.1));
        v.into_iter()
            .take(n)
            .map(|(k, (c, b))| json!({ "name": if k.is_empty() { "(none)".to_string() } else { k }, "files": c, "bytes": b, "size": human(b) }))
            .collect()
    };
    let brief = |r: &Row| json!({ "path": r.path, "size": r.size, "mtime": r.mtime });
    let mut largest = files.clone();
    largest.sort_by_key(|r| std::cmp::Reverse(r.size));
    let mut dated: Vec<&Row> = files.iter().copied().filter(|r| r.mtime > 0).collect();
    dated.sort_by_key(|r| r.mtime);
    let mut dups: Vec<(u64, Vec<&Row>)> = groups
        .into_values()
        .filter(|g| g.len() > 1)
        .map(|g| (g[0].size * (g.len() as u64 - 1), g))
        .collect();
    dups.sort_by_key(|d| std::cmp::Reverse(d.0));
    let bytes: u64 = files.iter().map(|f| f.size).sum();
    Ok(json!({
        "folder": label,
        "files": files.len(),
        "folders": rows.iter().filter(|r| r.dir).count(),
        "bytes": bytes, "size": human(bytes),
        "by_kind": tally(by_kind, 20),
        "by_extension": tally(by_ext, 20),
        "largest": largest.iter().take(top).map(|r| brief(r)).collect::<Vec<_>>(),
        "oldest": dated.iter().take(top).map(|r| brief(r)).collect::<Vec<_>>(),
        "newest": dated.iter().rev().take(top).map(|r| brief(r)).collect::<Vec<_>>(),
        "duplicates_inside": {
            "groups": dups.len(),
            "wasted_bytes": dups.iter().map(|d| d.0).sum::<u64>(),
            "top": dups.iter().take(top).map(|(w, g)| json!({
                "wasted": w, "size": g[0].size,
                "paths": g.iter().map(|r| r.path.clone()).collect::<Vec<_>>()
            })).collect::<Vec<_>>()
        }
    }))
}

pub async fn duplicates(ctx: &Ctx, args: &Value) -> Result<Value, String> {
    let (offset, limit) = ctx.paging(args);
    let min = args["min_size"].as_u64().unwrap_or(1).max(1);
    let scope = opt_str(args, "location")
        .map(|l| ctx.location(l))
        .transpose()?;
    let sub = opt_str(args, "path")
        .unwrap_or("")
        .trim_matches('/')
        .to_string();
    let locs: HashMap<String, space::Location> = space::locations(&ctx.a.space)
        .into_iter()
        .map(|l| (l.id.clone(), l))
        .collect();
    let names = ctx.device_names();
    let on = ctx.online();
    let mut groups: HashMap<(String, u64), Vec<(String, String)>> = HashMap::new();
    for (lid, idx) in ctx.indexes().await {
        for e in idx.entries.iter().cloned() {
            if e.dir || e.sampled.is_empty() || e.size < min {
                continue;
            }
            groups
                .entry((e.sampled, e.size))
                .or_default()
                .push((lid.clone(), e.path));
        }
    }
    let in_scope = |lid: &str, path: &str| {
        scope.as_ref().is_none_or(|l| {
            l.id == lid && (sub.is_empty() || path == sub || path.starts_with(&format!("{sub}/")))
        })
    };
    let mut out: Vec<(u64, Value)> = groups
        .into_iter()
        .filter(|(_, g)| g.len() > 1 && g.iter().any(|(l, p)| in_scope(l, p)))
        .map(|((h, size), g)| {
            let wasted = size * (g.len() as u64 - 1);
            let copies: Vec<Value> = g
                .iter()
                .map(|(lid, p)| {
                    let l = locs.get(lid);
                    json!({
                        "thing": format!("{}:{p}", l.map_or(lid.as_str(), |l| l.name.as_str())),
                        "device": l.and_then(|l| names.get(&l.device)),
                        "online": l.is_some_and(|l| on.contains(&l.device))
                    })
                })
                .collect();
            (wasted, json!({ "content": format!("content:{h}"), "size": size, "wasted": wasted, "copies": copies }))
        })
        .collect();
    out.sort_by_key(|o| std::cmp::Reverse(o.0));
    let wasted: u64 = out.iter().map(|o| o.0).sum();
    let mut v = page(out.into_iter().map(|o| o.1).collect(), offset, limit);
    v["wasted_bytes"] = json!(wasted);
    v["wasted"] = json!(human(wasted));
    v["note"] =
        json!("same content by sampled hash and size: likely copies; nothing here removes any");
    Ok(v)
}

pub async fn projects(ctx: &Ctx, args: &Value) -> Result<Value, String> {
    let (offset, limit) = ctx.paging(args);
    let lib = ctx.tags();
    let locs: HashMap<String, space::Location> = space::locations(&ctx.a.space)
        .into_iter()
        .map(|l| (l.id.clone(), l))
        .collect();
    let names = ctx.device_names();
    let out: Vec<Value> = projects::projects(&ctx.a.space)
        .into_iter()
        .map(|p| {
            let on = lib.on(&Target::Project(p.id.clone()));
            json!({
                "id": p.id, "name": p.name, "kind": p.kind, "created": p.created,
                "parts": p.parts.iter().map(|part| {
                    let l = locs.get(&part.location);
                    json!({
                        "label": part.label,
                        "folder": format!("{}:{}", l.map_or(part.location.as_str(), |l| l.name.as_str()), part.path),
                        "device": l.and_then(|l| names.get(&l.device)),
                        "available": part.available
                    })
                }).collect::<Vec<_>>(),
                "docs": p.docs, "shared_with": p.shared_with,
                "tags": on.tags.iter().map(|a| lib.label(&a.tag)).collect::<Vec<_>>()
            })
        })
        .collect();
    Ok(page(out, offset, limit))
}

pub async fn tags(ctx: &Ctx, args: &Value) -> Result<Value, String> {
    let lib = ctx.tags();
    if let Some(t) = opt_str(args, "thing") {
        let thing = ctx.thing(t).await?;
        let on = lib.on(&thing.target()?);
        return Ok(json!({
            "thing": thing.label(),
            "tags": on.tags.iter().map(|a| json!({ "tag": lib.label(&a.tag), "id": a.tag, "by": a.by, "at": a.at })).collect::<Vec<_>>(),
            "fields": on.fields.iter().map(|f| json!({ "name": f.name, "value": f.value, "kind": f.kind.id(), "by": f.by })).collect::<Vec<_>>()
        }));
    }
    let (offset, limit) = ctx.paging(args);
    let counts = lib.counts();
    let mut all = lib.all();
    all.sort_by_key(|t| lib.label(&t.id).to_lowercase());
    let out: Vec<Value> = all
        .into_iter()
        .map(|t| {
            json!({
                "id": t.id, "name": lib.label(&t.id), "count": counts.get(&t.id).copied().unwrap_or(0),
                "parents": t.parents.iter().map(|p| lib.label(p)).collect::<Vec<_>>(),
                "shorthand": t.shorthand, "aliases": t.aliases, "color": t.color.id(),
                "category": t.category, "hidden": t.hidden, "builtin": t.builtin
            })
        })
        .collect();
    let mut v = page(out, offset, limit);
    v["broken_parents"] = json!(
        lib.broken()
            .iter()
            .map(|(c, p)| format!("{} under {}", lib.label(c), lib.label(p)))
            .collect::<Vec<_>>()
    );
    Ok(v)
}

/// A rule by id or name, else by a part of its name that only one has.
fn find_rule(ctx: &Ctx, name: &str) -> Result<drive_rules::Rule, String> {
    if let Some(r) = doc::find(&ctx.e.rules, name) {
        return Ok(r);
    }
    let low = name.to_lowercase();
    let mut like: Vec<drive_rules::Rule> = doc::rules(&ctx.e.rules)
        .into_iter()
        .filter(|r| r.name.to_lowercase().contains(&low))
        .collect();
    match like.len() {
        1 => Ok(like.remove(0)),
        0 => Err(format!("no rule {name}; `rules` lists them")),
        _ => Err(format!(
            "{name} could be {}; give the whole name or the id",
            like.iter()
                .map(|r| r.name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        )),
    }
}

pub async fn rules(ctx: &Ctx, args: &Value) -> Result<Value, String> {
    let names = ctx.device_names();
    let dev = |id: &str| names.get(id).cloned().unwrap_or_else(|| id.to_string());
    let runs = |id: &str| -> Vec<Value> {
        journal::runs(&ctx.e.journal, id)
            .into_iter()
            .map(|r| json!({
                "device": dev(&r.device), "live": r.live, "at": r.at, "matched": r.matched,
                "done": r.done, "waiting": r.waiting, "failed": r.failed, "held_back": r.skipped,
                "error": (!r.error.is_empty()).then_some(r.error)
            }))
            .collect()
    };
    if let Some(n) = opt_str(args, "rule") {
        let r = find_rule(ctx, n)?;
        return Ok(json!({
            "rule": r,
            "feeds_itself": eval::feeds_itself(&r),
            "runs": runs(&r.id),
            "switch": "only a person turns a rule on and makes it live (the Drive app's Rules, or drive rules on|live NAME)"
        }));
    }
    let out: Vec<Value> = doc::rules(&ctx.e.rules)
        .into_iter()
        .map(|r| {
            json!({
                "id": r.id, "name": r.name, "on": r.enabled, "mode": r.mode,
                "does": r.actions.iter().map(|a| a.kind()).collect::<Vec<_>>(),
                "by": r.by, "created": r.created, "runs": runs(&r.id)
            })
        })
        .collect();
    Ok(json!({ "rules": out }))
}

/// Ask every device in a rule's scope (and the node) for a new dry run,
/// running it here when this device is in scope and no `serve` runs it;
/// never live.
pub async fn ask_dry_run(ctx: &Ctx, r: &drive_rules::Rule) -> i64 {
    let asked = drive_files::now_ms();
    let _ = doc::request_run(&ctx.e.rules, &r.id, false, &ctx.author, asked);
    ctx.commit_rules();
    let me = ctx.e.me(&ctx.a);
    if eval::runs_on(r, &me) && !crate::rules::served(&ctx.a.dir) {
        ctx.e.run(&ctx.a, r, false, None).await;
    }
    asked
}

pub async fn previews(
    ctx: &Ctx,
    r: &drive_rules::Rule,
    asked: Option<i64>,
    wait: u64,
    offset: usize,
    limit: usize,
) -> Value {
    let names = ctx.device_names();
    let on = ctx.online();
    // Who should answer: online devices the rule runs on, and the node
    // when it covers the Drive tree.
    let mut expect: Vec<String> = space::devices(&ctx.a.space)
        .into_iter()
        .filter(|d| on.contains(&d.id))
        .filter(|d| {
            eval::runs_on(
                r,
                &drive_rules::DeviceInfo {
                    id: d.id.clone(),
                    name: d.name.clone(),
                    roles: d.roles.clone(),
                },
            )
        })
        .map(|d| d.id)
        .collect();
    if eval::covers_drive(r) {
        expect.push("node".into());
    }
    let fresh = |p: &journal::Preview| asked.is_none_or(|a| p.at >= a - 1_000);
    if let Some(_asked) = asked {
        let until = std::time::Instant::now() + std::time::Duration::from_secs(wait);
        loop {
            let got = journal::previews(&ctx.e.journal, &r.id);
            if expect
                .iter()
                .all(|d| got.iter().any(|p| p.device == *d && fresh(p)))
                || std::time::Instant::now() >= until
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(400)).await;
        }
    }
    let got = journal::previews(&ctx.e.journal, &r.id);
    let waiting: Vec<String> = expect
        .iter()
        .filter(|d| !got.iter().any(|p| p.device == **d && fresh(p)))
        .map(|d| names.get(d).cloned().unwrap_or_else(|| d.clone()))
        .collect();
    let per: Vec<Value> = got
        .iter()
        .map(|p| {
            let rows: Vec<Value> = p
                .rows
                .iter()
                .map(|row| {
                    json!({
                        "file": row.item.path(),
                        "steps": row.steps.iter().map(|s| json!({
                            "does": s.says,
                            "held_back": s.blocked,
                            "already_so": s.noop
                        })).collect::<Vec<_>>()
                    })
                })
                .collect();
            let mut v = page(rows, offset, limit);
            v["device"] = json!(
                names
                    .get(&p.device)
                    .cloned()
                    .unwrap_or_else(|| p.device.clone())
            );
            v["at"] = json!(p.at);
            v["fresh"] = json!(fresh(p));
            v["matched"] = json!(p.matched);
            v
        })
        .collect();
    json!({
        "rule": r.name, "on": r.enabled, "mode": r.mode,
        "previews": per,
        "waiting_for": waiting,
        "offline_devices_answer_when_they_are_on": true
    })
}

pub async fn rule_preview(ctx: &Ctx, args: &Value) -> Result<Value, String> {
    let (offset, limit) = ctx.paging(args);
    let r = find_rule(
        ctx,
        opt_str(args, "rule").ok_or("rule_preview needs a rule")?,
    )?;
    let wait = args["wait_seconds"].as_u64().unwrap_or(15).min(60);
    let asked = if args["refresh"].as_bool().unwrap_or(true) {
        Some(ask_dry_run(ctx, &r).await)
    } else {
        None
    };
    Ok(previews(ctx, &r, asked, wait, offset, limit).await)
}

pub async fn journal(ctx: &Ctx, args: &Value) -> Result<Value, String> {
    let (offset, limit) = ctx.paging(args);
    let rule = opt_str(args, "rule")
        .map(|n| find_rule(ctx, n))
        .transpose()?;
    let device = opt_str(args, "device")
        .map(|d| ctx.device_id(d))
        .transpose()?;
    let names = ctx.device_names();
    let out: Vec<Value> = journal::entries(&ctx.e.journal)
        .into_iter()
        .filter(|j| rule.as_ref().is_none_or(|r| r.id == j.rule))
        .filter(|j| device.as_ref().is_none_or(|d| *d == j.device))
        .map(|j| {
            json!({
                "id": j.id, "at": j.at,
                "device": names.get(&j.device).cloned().unwrap_or(j.device.clone()),
                "rule": j.rule_name, "action": j.action,
                "status": j.status.map(|s| s.id()),
                "file": j.before.as_ref().map(|b| b.path().to_string()),
                "says": j.says,
                "error": (!j.error.is_empty()).then_some(j.error.clone()),
                "can_undo": j.undo.possible()
            })
        })
        .collect();
    Ok(page(out, offset, limit))
}

/// A tag's label in a library (for the write tools' answers).
pub fn label_all(lib: &Library, ids: &[String]) -> Vec<String> {
    ids.iter().map(|i| lib.label(i)).collect()
}

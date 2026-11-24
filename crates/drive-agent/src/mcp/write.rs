//! The tools that organise. Each goes through Drive's own paths (the tags
//! document, the registry, the rules document, the node's index route),
//! records the session's author, takes `dry_run`, and is charged against
//! the session's write cap. None deletes, trashes or overwrites anything,
//! and none can switch a rule on or make it live.

use drive_files::{projects, space};
use drive_rules::{Action, Matcher, Mode, Rule, doc};
use drive_spine::tags::{self, Color, FieldKind, NewTag};
use serde_json::{Value, json};

use super::ctx::Ctx;
use super::read::{self, label_all};
use super::tools::{dry, opt_str, str_list};

/// `tag` and `untag`: every thing and tag checked first; then all of it,
/// or nothing.
pub async fn tag(ctx: &Ctx, args: &Value, add: bool) -> Result<Value, String> {
    let names = str_list(args, "things");
    let tag_names = str_list(args, "tags");
    if names.is_empty() || tag_names.is_empty() {
        return Err("give at least one thing and one tag".into());
    }
    if names.len() * tag_names.len() > ctx.opts.page_max {
        return Err(format!(
            "at most {} thing-tag pairs in one call; split it",
            ctx.opts.page_max
        ));
    }
    let mut targets = Vec::new();
    for n in &names {
        let t = ctx.thing(n).await?;
        targets.push((t.label(), t.target()?));
    }
    let lib = ctx.tags();
    let mut ids = Vec::new();
    let mut create = Vec::new();
    for t in &tag_names {
        match ctx.tag_id(&lib, t)? {
            Some(id) => ids.push(id),
            None if add && args["create_missing"].as_bool() == Some(true) => create.push(t.clone()),
            None => {
                return Err(format!(
                    "no tag {t}{}",
                    if add {
                        " (create_missing: true makes it, or create_tag with a parent)"
                    } else {
                        ""
                    }
                ));
            }
        }
    }
    // What changes.
    let mut changes = Vec::new();
    let mut n = create.len();
    for (label, target) in &targets {
        let on = lib.on(target).tag_ids();
        let (todo, already): (Vec<String>, Vec<String>) =
            ids.iter().cloned().partition(|id| on.contains(id) != add);
        n += todo.len() + if add { create.len() } else { 0 };
        changes.push(json!({
            "thing": label,
            "change": if add { "add" } else { "remove" },
            "tags": label_all(&lib, &todo).into_iter().chain(create.iter().cloned()).collect::<Vec<_>>(),
            "already": label_all(&lib, &already)
        }));
    }
    if dry(args) {
        return Ok(json!({ "dry_run": true, "would": changes, "new_tags": create, "writes": n }));
    }
    ctx.charge(n)?;
    let now = drive_files::now_ms();
    let mut all = ids.clone();
    for c in &create {
        let id = tags::create(
            &ctx.e.tags,
            NewTag {
                name: c,
                ..NewTag::default()
            },
            &ctx.author,
            now,
        )
        .map_err(|e| e.to_string())?;
        all.push(id);
    }
    for (_, target) in &targets {
        for id in &all {
            if add {
                tags::tag(&ctx.e.tags, target, id, &ctx.author, now)
            } else {
                tags::untag(&ctx.e.tags, target, id)
            }
            .map_err(|e| e.to_string())?;
        }
    }
    ctx.commit_tags();
    Ok(json!({ "done": changes, "new_tags": create, "by": ctx.author }))
}

pub async fn set_field(ctx: &Ctx, args: &Value) -> Result<Value, String> {
    let thing = ctx
        .thing(opt_str(args, "thing").ok_or("set_field needs a thing")?)
        .await?;
    let target = thing.target()?;
    let name = opt_str(args, "name").ok_or("set_field needs a name")?;
    let value = args["value"].as_str().ok_or("set_field needs a value")?;
    let kind = FieldKind::parse(opt_str(args, "kind").unwrap_or("text"));
    let ok = match kind.id() {
        "date" => value.len() == 10 && value.as_bytes()[4] == b'-' && value.as_bytes()[7] == b'-',
        "datetime" => value.len() == 16 && value.as_bytes().get(10) == Some(&b'T'),
        _ => true,
    };
    if !ok {
        return Err(format!(
            "a {} field takes {}",
            kind.id(),
            if kind.id() == "date" {
                "YYYY-MM-DD"
            } else {
                "YYYY-MM-DDTHH:MM"
            }
        ));
    }
    let lib = ctx.tags();
    let before = lib
        .on(&target)
        .fields
        .into_iter()
        .find(|f| f.name.eq_ignore_ascii_case(name));
    let answer = json!({
        "thing": thing.label(), "field": name, "value": value,
        "was": before.as_ref().map(|f| f.value.clone())
    });
    if dry(args) {
        return Ok(json!({ "dry_run": true, "would": answer }));
    }
    ctx.charge(1)?;
    let now = drive_files::now_ms();
    match before {
        Some(f) => tags::set_field(&ctx.e.tags, &target, &f.id, value, &ctx.author, now),
        None => {
            tags::add_field(&ctx.e.tags, &target, name, kind, value, &ctx.author, now).map(|_| ())
        }
    }
    .map_err(|e| e.to_string())?;
    ctx.commit_tags();
    Ok(json!({ "done": answer, "by": ctx.author }))
}

pub async fn create_tag(ctx: &Ctx, args: &Value) -> Result<Value, String> {
    let name = opt_str(args, "name").ok_or("create_tag needs a name")?;
    let lib = ctx.tags();
    let mut parents = Vec::new();
    for p in str_list(args, "parents") {
        parents.push(
            ctx.tag_id(&lib, &p)?
                .ok_or_else(|| format!("no parent tag {p}"))?,
        );
    }
    parents.sort();
    if let Some(same) = lib.all().into_iter().find(|t| {
        t.name.eq_ignore_ascii_case(name) && {
            let mut tp = t.parents.clone();
            tp.sort();
            tp == parents
        }
    }) {
        return Err(format!(
            "refused: there already is a tag {} ({}); use it",
            lib.label(&same.id),
            same.id
        ));
    }
    let color = Color::parse(opt_str(args, "color").unwrap_or(""));
    let aliases = str_list(args, "aliases");
    let shorthand = opt_str(args, "shorthand").unwrap_or("");
    let category = args["category"].as_bool().unwrap_or(false);
    let hidden = args["hidden"].as_bool().unwrap_or(false);
    let summary = json!({
        "name": name, "parents": label_all(&lib, &parents), "color": color.id(),
        "shorthand": shorthand, "aliases": aliases, "category": category, "hidden": hidden
    });
    if dry(args) {
        return Ok(json!({ "dry_run": true, "would_create": summary }));
    }
    ctx.charge(1)?;
    let id = tags::create(
        &ctx.e.tags,
        NewTag {
            name,
            shorthand,
            color,
            parents: &parents,
            category,
        },
        &ctx.author,
        drive_files::now_ms(),
    )
    .map_err(|e| e.to_string())?;
    if hidden {
        tags::set_hidden(&ctx.e.tags, &id, true).map_err(|e| e.to_string())?;
    }
    for a in &aliases {
        tags::add_alias(&ctx.e.tags, &id, a).map_err(|e| e.to_string())?;
    }
    ctx.commit_tags();
    Ok(json!({ "created": summary, "id": id, "by": ctx.author }))
}

pub async fn add_to_project(ctx: &Ctx, args: &Value) -> Result<Value, String> {
    let p = ctx.project(opt_str(args, "project").ok_or("add_to_project needs a project")?)?;
    if let Some(d) = opt_str(args, "doc") {
        if p.docs.iter().any(|x| x == d) {
            return Ok(json!({ "already": true, "project": p.name, "doc": d }));
        }
        if dry(args) {
            return Ok(json!({ "dry_run": true, "would_add": { "project": p.name, "doc": d } }));
        }
        ctx.charge(1)?;
        projects::link_doc(&ctx.a.space, &p.id, d, true).map_err(|e| e.to_string())?;
        ctx.a.commit_space();
        return Ok(json!({ "added": { "project": p.name, "doc": d } }));
    }
    let loc = ctx.location(
        opt_str(args, "location").ok_or("add_to_project needs a location (and path) or a doc")?,
    )?;
    let path = opt_str(args, "path")
        .unwrap_or("")
        .trim_matches('/')
        .to_string();
    if path.split('/').any(|x| x == "..") {
        return Err("refused: a path may not climb out with ..".into());
    }
    if !path.is_empty()
        && ctx.has_index(&loc).await.is_ok()
        && ctx.a.index_entry(&loc.id, &path).await.is_none_or(|e| !e.dir)
    {
        return Err(format!(
            "{path} is not a folder of {} (as of its last index)",
            loc.name
        ));
    }
    if p.parts
        .iter()
        .any(|x| x.location == loc.id && x.path == path)
    {
        return Ok(
            json!({ "already": true, "project": p.name, "folder": format!("{}:{path}", loc.name) }),
        );
    }
    let label = opt_str(args, "label").unwrap_or("Files").to_string();
    let part =
        json!({ "project": p.name, "folder": format!("{}:{path}", loc.name), "label": label });
    if dry(args) {
        return Ok(json!({ "dry_run": true, "would_add": part }));
    }
    ctx.charge(1)?;
    projects::add_part(&ctx.a.space, &p.id, &label, &loc.id, &path).map_err(|e| e.to_string())?;
    ctx.a.commit_space();
    Ok(json!({ "added": part }))
}

pub async fn create_project(ctx: &Ctx, args: &Value) -> Result<Value, String> {
    let name = opt_str(args, "name").ok_or("create_project needs a name")?;
    if let Ok(p) = ctx.project(name) {
        return Err(format!(
            "refused: there already is a project {} ({})",
            p.name, p.id
        ));
    }
    let kind = opt_str(args, "kind").unwrap_or("Project");
    if dry(args) {
        return Ok(json!({ "dry_run": true, "would_create": { "name": name, "kind": kind } }));
    }
    ctx.charge(1)?;
    let id = projects::add_project(&ctx.a.space, name, kind).map_err(|e| e.to_string())?;
    ctx.a.commit_space();
    Ok(json!({ "created": { "name": name, "kind": kind, "id": id } }))
}

pub async fn index_folder(ctx: &Ctx, args: &Value) -> Result<Value, String> {
    let device = ctx.device_id(opt_str(args, "device").ok_or("index_folder needs a device")?)?;
    let root = opt_str(args, "root")
        .ok_or("index_folder needs a root")?
        .to_string();
    let path = opt_str(args, "path")
        .unwrap_or("")
        .trim_matches('/')
        .to_string();
    let name = opt_str(args, "name")
        .ok_or("index_folder needs a name")?
        .to_string();
    if path.split('/').any(|x| x == ".." || x == ".") {
        return Err("refused: a path may hold only plain names".into());
    }
    if let Ok(l) = ctx.location(&name) {
        return Err(format!(
            "refused: there already is a location named {} (names must be unique)",
            l.name
        ));
    }
    let what =
        json!({ "device": opt_str(args, "device"), "root": root, "path": path, "name": name });
    let base = format!("/browse/{}/{device}", ctx.space);
    if dry(args) {
        // The device lists it: it is on and the folder is there.
        let v = ctx
            .get(
                &format!("{base}/list"),
                &[("root", &root), ("path", &path), ("limit", "1")],
            )
            .await?;
        return Ok(json!({
            "dry_run": true, "would_index": what,
            "folder_entries": v["total"], "device_online": v["online"],
            "note": "indexing only reads the folder"
        }));
    }
    ctx.charge(1)?;
    let me = ctx.a.device_id();
    match ctx
        .post(
            &format!("{base}/index"),
            &json!({ "root": root, "path": path, "name": name }),
        )
        .await
    {
        Ok(v) => Ok(
            json!({ "indexed": what, "location": v["location"], "note": "its device walks it now; `locations` shows its files when done" }),
        ),
        // This device, with no `serve` answering the node: index it here.
        Err(e) if device == me && (e.starts_with("offline") || e.starts_with("timeout")) => {
            let folder = local_folder(ctx, &root, &path)?;
            let loc = ctx.a.add_location(&name, &folder)?;
            let st = ctx.a.index_location(&loc).await?;
            Ok(
                json!({ "indexed": what, "location": loc.id, "files": st.files, "bytes": st.bytes, "here": true }),
            )
        }
        Err(e) => Err(e),
    }
}

/// A folder of this computer, confined to one of its roots as browsing is.
fn local_folder(ctx: &Ctx, root: &str, path: &str) -> Result<std::path::PathBuf, String> {
    let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
    let mounts: Vec<String> = space::volumes(&ctx.a.space)
        .into_iter()
        .filter(|v| v.device == ctx.a.device_id())
        .map(|v| v.mount)
        .collect();
    let rootp = std::path::Path::new(root);
    if !(root == "/" || home.as_deref() == Some(rootp) || mounts.iter().any(|m| m == root)) {
        return Err(format!(
            "refused: {root} is not one of this computer's volumes"
        ));
    }
    let canon_root = std::fs::canonicalize(rootp).map_err(|e| format!("{root}: {e}"))?;
    let folder = std::fs::canonicalize(rootp.join(path)).map_err(|e| format!("{path}: {e}"))?;
    let agent = std::fs::canonicalize(&ctx.a.dir).unwrap_or(ctx.a.dir.clone());
    if !folder.starts_with(&canon_root)
        || folder.starts_with(&agent)
        || ["/proc", "/sys", "/dev"]
            .iter()
            .any(|p| folder.starts_with(p))
        || !folder.is_dir()
    {
        return Err("refused: that folder is outside what may be indexed".into());
    }
    Ok(folder)
}

/// Check a rule an agent wrote and make it a new rule: off, a dry run.
pub fn proposed(ctx: &Ctx, given: &Value) -> Result<Rule, String> {
    let Value::Object(given) = given else {
        return Err("rule must be an object".into());
    };
    let mut base = serde_json::to_value(Rule::default()).map_err(|e| e.to_string())?;
    for (k, v) in given {
        // The switches and the identity are not the agent's to set.
        if matches!(k.as_str(), "id" | "enabled" | "mode" | "created" | "by") {
            continue;
        }
        base[k] = v.clone();
    }
    let mut r: Rule = serde_json::from_value(base).map_err(|e| format!("the rule: {e}"))?;
    r.id.clear();
    r.enabled = false;
    r.mode = Mode::DryRun;
    r.by = ctx.author.clone();
    r.name = r.name.trim().to_string();
    if r.name.is_empty() {
        return Err("a rule needs a name".into());
    }
    if doc::find(&ctx.e.rules, &r.name).is_some() {
        return Err(format!(
            "refused: there already is a rule {}; name this one differently",
            r.name
        ));
    }
    if r.actions.is_empty() {
        return Err("a rule needs at least one action".into());
    }
    if r.actions.iter().any(|a| matches!(a, Action::Trash)) {
        return Err("refused: an agent's rule may not trash anything".into());
    }
    crate::rules::resolve_names(&ctx.a, &ctx.e, &mut r)?;
    Matcher::new(&r).map_err(|e| format!("the rule: {e}"))?;
    Ok(r)
}

/// Save a proposed rule (off, a dry run), checking it stayed so.
pub fn save_proposed(ctx: &Ctx, r: &Rule) -> Result<String, String> {
    let id = doc::save(&ctx.e.rules, r, &ctx.author, drive_files::now_ms())
        .map_err(|e| e.to_string())?;
    let saved = doc::rule(&ctx.e.rules, &id).ok_or("the rule was not saved")?;
    if saved.enabled || saved.mode != Mode::DryRun {
        // `doc::save` keeps a fresh rule off; this cannot happen, and if
        // it did, it is switched back before anyone sees it.
        doc::set_enabled(&ctx.e.rules, &id, false).map_err(|e| e.to_string())?;
        doc::set_mode(&ctx.e.rules, &id, Mode::DryRun).map_err(|e| e.to_string())?;
    }
    ctx.commit_rules();
    Ok(id)
}

pub async fn propose_rule(ctx: &Ctx, args: &Value) -> Result<Value, String> {
    let r = proposed(ctx, &args["rule"])?;
    if dry(args) {
        return Ok(json!({ "dry_run": true, "would_create": r, "state": "off, dry run" }));
    }
    ctx.charge(1)?;
    let id = save_proposed(ctx, &r)?;
    let r = doc::rule(&ctx.e.rules, &id).ok_or("the rule was not saved")?;
    let wait = args["wait_seconds"].as_u64().unwrap_or(15).min(60);
    let asked = read::ask_dry_run(ctx, &r).await;
    let preview = read::previews(ctx, &r, Some(asked), wait, 0, 50.min(ctx.opts.page_max)).await;
    Ok(json!({
        "created": { "id": id, "name": r.name, "on": false, "mode": "dry-run", "by": ctx.author },
        "preview": preview,
        "next": format!("Show the person this dry run. Only they can switch it on and live: the Drive app's Rules, or `drive rules on \"{0}\"` then `drive rules live \"{0}\"`.", r.name)
    }))
}

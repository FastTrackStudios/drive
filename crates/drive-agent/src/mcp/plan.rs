//! `plan` and `apply_plan`: a one-off reorganisation as a batch of
//! operations, previewed first.
//!
//! Each operation comes out as one of three:
//!
//! - **apply**: done by `apply_plan`: tags, fields, projects (through the
//!   write tools, so with their checks), and copies, moves and renames
//!   inside the Drive tree (entries listed elsewhere; a copy is a new entry
//!   sharing the same verified blob; a taken name gets " (2)"; nothing is
//!   ever removed or written over);
//! - **rule**: a computer's files are only ever moved, copied or renamed by
//!   file rules, so these become proposed rules (off, a dry run) that a
//!   person looks at and switches on;
//! - **refused**: deleting, trashing, overwriting, anything unknown, and
//!   what cannot be done safely (moves across computers, folders copied
//!   whole).

use std::collections::BTreeMap;

use drive_files::tree::{self, Entry};
use drive_rules::{
    Action, Condition, Dest, Devices, Group, Place, Rule, Scope, Test, Text, TextOp, Trigger, When,
};
use loro::TreeID;
use serde::Serialize;
use serde_json::{Value, json};

use super::ctx::{Ctx, Thing};
use super::tools::{dry, opt_str};
use super::write;

/// Operations that are never carried out, by name.
pub const DESTRUCTIVE: &[&str] = &[
    "delete",
    "remove",
    "rm",
    "trash",
    "unlink",
    "erase",
    "purge",
    "empty_trash",
    "overwrite",
    "replace",
    "truncate",
    "wipe",
    "shred",
];

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Outcome {
    Apply,
    Rule,
    Refused,
}

/// What an operation does, worked out when it is planned.
#[derive(Clone, Debug)]
enum Act {
    /// A write tool, with its arguments.
    Tool {
        tool: &'static str,
        args: Value,
    },
    DriveCopy {
        from: TreeID,
        folder: String,
        name: String,
    },
    DriveMove {
        from: TreeID,
        folder: String,
        name: String,
    },
    /// Part of a proposed rule: (where it looks, the action) and the file.
    Rule {
        place: Place,
        action: Action,
        path: String,
    },
    None,
}

#[derive(Clone, Debug, Serialize)]
pub struct Op {
    pub i: usize,
    pub op: String,
    pub outcome: Outcome,
    pub says: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub why: Option<String>,
    /// What the write tool's dry run said it would do.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub would: Option<Value>,
    #[serde(skip)]
    act: Act,
}

/// A tag, field or project operation in words.
fn say(op: &str, raw: &Value) -> String {
    let s = |k: &str| raw[k].as_str().unwrap_or_default().to_string();
    let tags = super::tools::str_list(raw, "tags").join(", ");
    match op {
        "tag" => format!("tag {} with {tags}", s("thing")),
        "untag" => format!("take {tags} off {}", s("thing")),
        "set_field" => format!("set {} to {:?} on {}", s("name"), s("value"), s("thing")),
        _ if !s("doc").is_empty() => format!("add doc:{} to project {}", s("doc"), s("project")),
        _ => format!(
            "add {}:{} to project {}",
            s("location"),
            s("path"),
            s("project")
        ),
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct Plan {
    pub id: String,
    pub title: String,
    pub ops: Vec<Op>,
    pub applied: bool,
}

fn refused(i: usize, op: &str, says: String, why: impl Into<String>) -> Op {
    Op {
        i,
        op: op.to_string(),
        outcome: Outcome::Refused,
        says,
        why: Some(why.into()),
        would: None,
        act: Act::None,
    }
}

/// A folder named as `drive:FOLDER` or `LOCATION:FOLDER`.
enum Folder {
    Drive(String),
    Location {
        id: String,
        name: String,
        device: String,
        path: String,
    },
}

fn folder(ctx: &Ctx, to: &str) -> Result<Folder, String> {
    let to = to.trim();
    if let Some(p) = to.strip_prefix("drive:") {
        let p = p.trim_matches('/').to_string();
        if p.split('/').any(|x| x == "..") {
            return Err("a folder may not climb out with ..".into());
        }
        if p == tree::TRASH || p.starts_with(&format!("{}/", tree::TRASH)) {
            return Err("putting things in the Trash is trashing them".into());
        }
        return Ok(Folder::Drive(p));
    }
    if let Some((l, p)) = to.split_once(':') {
        let loc = ctx.location(l)?;
        let p = p.trim_matches('/').to_string();
        if p.split('/').any(|x| x == ".." || x == ".drive-trash") {
            return Err("a folder may not climb out with .. or go into .drive-trash".into());
        }
        return Ok(Folder::Location {
            id: loc.id,
            name: loc.name,
            device: loc.device,
            path: p,
        });
    }
    Err(format!(
        "{to}: name a folder as drive:FOLDER or LOCATION:FOLDER"
    ))
}

fn plain_name(name: &str) -> Result<(), String> {
    if name.is_empty() || name.contains('/') || name == "." || name == ".." {
        Err(format!("{name:?} is not a plain file name"))
    } else {
        Ok(())
    }
}

async fn one(ctx: &Ctx, i: usize, raw: &Value) -> Op {
    let op = raw["op"].as_str().unwrap_or_default().trim().to_lowercase();
    if op.is_empty() {
        return refused(i, "", "?".into(), "an operation needs `op`");
    }
    if DESTRUCTIVE.contains(&op.as_str()) {
        return refused(
            i,
            &op,
            format!(
                "{op} {}",
                raw["thing"].as_str().or(raw["from"].as_str()).unwrap_or("")
            ),
            "Drive's agent tools never delete, trash or overwrite anything",
        );
    }
    if raw["overwrite"].as_bool() == Some(true) || raw["replace"].as_bool() == Some(true) {
        return refused(
            i,
            &op,
            format!(
                "{op} {}",
                raw["from"].as_str().or(raw["thing"].as_str()).unwrap_or("")
            ),
            "nothing is ever overwritten: a taken name gets \" (2)\"",
        );
    }
    match op.as_str() {
        "tag" | "untag" | "set_field" | "add_to_project" => {
            let (tool, args) = match op.as_str() {
                "tag" => (
                    "tag",
                    json!({ "things": [raw["thing"]], "tags": raw["tags"], "create_missing": raw["create_missing"] }),
                ),
                "untag" => (
                    "untag",
                    json!({ "things": [raw["thing"]], "tags": raw["tags"] }),
                ),
                "set_field" => (
                    "set_field",
                    json!({ "thing": raw["thing"], "name": raw["name"], "value": raw["value"], "kind": raw["kind"] }),
                ),
                _ => (
                    "add_to_project",
                    json!({ "project": raw["project"], "location": raw["location"], "path": raw["path"], "label": raw["label"], "doc": raw["doc"] }),
                ),
            };
            let mut check = strip_nulls(args.clone());
            check["dry_run"] = json!(true);
            let r = match tool {
                "tag" => write::tag(ctx, &check, true).await,
                "untag" => write::tag(ctx, &check, false).await,
                "set_field" => write::set_field(ctx, &check).await,
                _ => write::add_to_project(ctx, &check).await,
            };
            match r {
                Ok(v) => Op {
                    i,
                    op: op.clone(),
                    outcome: Outcome::Apply,
                    says: say(&op, raw),
                    would: v.get("would").or(v.get("would_add")).cloned(),
                    why: None,
                    act: Act::Tool {
                        tool,
                        args: strip_nulls(args),
                    },
                },
                Err(e) => refused(i, &op, op.clone(), e),
            }
        }
        "copy" | "move" | "rename" => {
            let Some(from_name) = opt_str(raw, "from").or(opt_str(raw, "thing")) else {
                return refused(i, &op, op.clone(), "needs `from`");
            };
            let from = match ctx.thing(from_name).await {
                Ok(t) => t,
                Err(e) => return refused(i, &op, format!("{op} {from_name}"), e),
            };
            let new_name = opt_str(raw, "name").map(str::to_string);
            if let Some(n) = &new_name
                && let Err(e) = plain_name(n)
            {
                return refused(i, &op, format!("{op} {from_name}"), e);
            }
            let dest = if op == "rename" {
                None
            } else {
                match opt_str(raw, "to").map(|t| folder(ctx, t)) {
                    Some(Ok(f)) => Some(f),
                    Some(Err(e)) => return refused(i, &op, format!("{op} {from_name}"), e),
                    None => {
                        return refused(
                            i,
                            &op,
                            format!("{op} {from_name}"),
                            "needs `to`: a folder (drive:FOLDER or LOCATION:FOLDER)",
                        );
                    }
                }
            };
            if op == "rename" && new_name.is_none() {
                return refused(i, &op, format!("rename {from_name}"), "needs `name`");
            }
            match from {
                Thing::Drive { entry, path } => drive_op(ctx, i, &op, entry, &path, dest, new_name),
                Thing::Located { loc, entry } => {
                    if entry.dir {
                        return refused(
                            i,
                            &op,
                            format!("{op} {from_name}"),
                            "folders of a computer are not moved or copied by a plan; name its files",
                        );
                    }
                    let name = new_name.clone().unwrap_or_default();
                    let action = match (&op[..], dest) {
                        ("rename", _) => Action::Rename {
                            name: new_name.unwrap_or_default(),
                        },
                        ("copy", Some(Folder::Drive(p))) => Action::Copy {
                            to: Dest::Drive { path: p },
                            name,
                        },
                        ("copy", Some(Folder::Location { id, path, .. })) => Action::Copy {
                            to: Dest::Location { location: id, path },
                            name,
                        },
                        (
                            "move",
                            Some(Folder::Location {
                                id, device, path, ..
                            }),
                        ) if device == loc.device => Action::Move {
                            to: Dest::Location { location: id, path },
                            name,
                        },
                        ("move", _) => {
                            return refused(
                                i,
                                &op,
                                format!("move {from_name}"),
                                "moves to another computer or into the Drive are not built yet: copy instead (the original stays)",
                            );
                        }
                        _ => {
                            return refused(
                                i,
                                &op,
                                format!("{op} {from_name}"),
                                "an operation of the wrong shape",
                            );
                        }
                    };
                    Op {
                        i,
                        op: op.clone(),
                        outcome: Outcome::Rule,
                        says: format!("{op} {}:{} ({})", loc.name, entry.path, describe(&action, ctx)),
                        why: Some("a computer's files are moved, copied and renamed only by file rules: this becomes a proposed rule (off, a dry run) that a person switches on and live".into()),
                        would: None,
                        act: Act::Rule { place: Place::Location { location: loc.id.clone(), path: String::new() }, action, path: entry.path.clone() },
                    }
                }
                other => refused(
                    i,
                    &op,
                    format!("{op} {}", other.label()),
                    "only files of the Drive or of a computer's location can be copied, moved or renamed",
                ),
            }
        }
        other => refused(
            i,
            other,
            other.to_string(),
            format!(
                "no operation {other} (tag, untag, set_field, add_to_project, copy, move, rename)"
            ),
        ),
    }
}

fn describe(a: &Action, ctx: &Ctx) -> String {
    let loc_name = |id: &str| {
        ctx.location(id)
            .map(|l| l.name)
            .unwrap_or_else(|_| id.to_string())
    };
    let dest = |d: &Dest| match d {
        Dest::Drive { path } => format!("drive:{path}"),
        Dest::Location { location, path } => format!("{}:{path}", loc_name(location)),
    };
    match a {
        Action::Copy { to, name } | Action::Move { to, name } => {
            format!(
                "to {}{}",
                dest(to),
                if name.is_empty() {
                    String::new()
                } else {
                    format!(" as {name}")
                }
            )
        }
        Action::Rename { name } => format!("as {name}"),
        _ => a.kind().to_string(),
    }
}

fn drive_op(
    ctx: &Ctx,
    i: usize,
    op: &str,
    entry: Entry,
    path: &str,
    dest: Option<Folder>,
    new_name: Option<String>,
) -> Op {
    let doc = &ctx.a.files;
    if tree::in_trash(doc, entry.id) {
        return refused(
            i,
            op,
            format!("{op} drive:{path}"),
            "it is in the Trash; restoring is a person's call",
        );
    }
    let name = new_name.unwrap_or_else(|| entry.name.clone());
    let folder = match dest {
        None => tree::parent_of(doc, entry.id)
            .map(|p| tree::path_of(doc, p))
            .unwrap_or_default(),
        Some(Folder::Drive(p)) => p,
        Some(Folder::Location {
            name: lname,
            path: lpath,
            id,
            ..
        }) => {
            if op == "copy" && !entry.dir {
                let action = Action::Copy {
                    to: Dest::Location {
                        location: id,
                        path: lpath.clone(),
                    },
                    name: if name == entry.name {
                        String::new()
                    } else {
                        name.clone()
                    },
                };
                return Op {
                    i,
                    op: op.to_string(),
                    outcome: Outcome::Rule,
                    says: format!("copy drive:{path} to {lname}:{lpath}"),
                    why: Some("a copy onto a computer is made by a file rule (the computer fetches and checks it): this becomes a proposed rule, off and a dry run".into()),
                    would: None,
                    act: Act::Rule { place: Place::Drive { path: String::new() }, action, path: path.to_string() },
                };
            }
            return refused(
                i,
                op,
                format!("{op} drive:{path}"),
                "moves from the Drive onto a computer are not built yet, nor folders copied whole: copy files instead",
            );
        }
    };
    if op == "move" && (folder == path || folder.starts_with(&format!("{path}/"))) {
        return refused(
            i,
            op,
            format!("move drive:{path}"),
            "a folder cannot go inside itself",
        );
    }
    let to = if folder.is_empty() {
        name.clone()
    } else {
        format!("{folder}/{name}")
    };
    match op {
        "copy" => {
            if entry.dir {
                return refused(
                    i,
                    op,
                    format!("copy drive:{path}"),
                    "folders are not copied whole: copy their files",
                );
            }
            if entry.hash.is_empty() {
                return refused(
                    i,
                    op,
                    format!("copy drive:{path}"),
                    "its bytes have not been uploaded yet",
                );
            }
            Op {
                i,
                op: op.into(),
                outcome: Outcome::Apply,
                says: format!(
                    "copy drive:{path} to drive:{to} (a new entry sharing its verified blob; \" (2)\" if the name is taken)"
                ),
                why: None,
                would: None,
                act: Act::DriveCopy {
                    from: entry.id,
                    folder,
                    name,
                },
            }
        }
        _ => Op {
            i,
            op: op.into(),
            outcome: Outcome::Apply,
            says: format!(
                "{op} drive:{path} to drive:{to} (\" (2)\" if the name is taken; nothing removed)"
            ),
            why: None,
            would: None,
            act: Act::DriveMove {
                from: entry.id,
                folder,
                name,
            },
        },
    }
}

fn strip_nulls(mut v: Value) -> Value {
    if let Value::Object(m) = &mut v {
        m.retain(|_, x| !x.is_null());
    }
    v
}

pub async fn plan(ctx: &Ctx, args: &Value) -> Result<Value, String> {
    let Some(ops) = args["operations"].as_array() else {
        return Err("plan needs operations (an array)".into());
    };
    if ops.is_empty() {
        return Err("a plan needs at least one operation".into());
    }
    if ops.len() > ctx.opts.page_max {
        return Err(format!(
            "at most {} operations in a plan; split it",
            ctx.opts.page_max
        ));
    }
    let title = opt_str(args, "title").unwrap_or("Plan").to_string();
    let mut out = Vec::new();
    for (i, raw) in ops.iter().enumerate() {
        out.push(one(ctx, i, raw).await);
    }
    let n = ctx.plans.lock().expect("plans").len();
    let id = format!("p{}-{}", n + 1, drive_files::now_ms() % 100_000);
    let plan = Plan {
        id: id.clone(),
        title,
        ops: out,
        applied: false,
    };
    let v = summary(ctx, &plan);
    ctx.plans.lock().expect("plans").insert(id, plan);
    Ok(v)
}

fn rules_of(ctx: &Ctx, plan: &Plan) -> Vec<Rule> {
    // One rule per (place, action): its files by exact path.
    let mut groups: BTreeMap<String, (Place, Action, Vec<String>)> = BTreeMap::new();
    for o in &plan.ops {
        if let Act::Rule {
            place,
            action,
            path,
        } = &o.act
        {
            let key = format!(
                "{}|{}",
                serde_json::to_string(place).unwrap_or_default(),
                serde_json::to_string(action).unwrap_or_default()
            );
            groups
                .entry(key)
                .or_insert_with(|| (place.clone(), action.clone(), Vec::new()))
                .2
                .push(path.clone());
        }
    }
    groups
        .into_values()
        .enumerate()
        .map(|(n, (place, action, paths))| Rule {
            name: format!(
                "{} ({}) {}: {} {} file(s) {}",
                plan.title,
                plan.id,
                n + 1,
                action.kind(),
                paths.len(),
                describe(&action, ctx)
            ),
            scope: Scope {
                devices: Devices::All,
                places: vec![place],
            },
            // Not on change, not on a schedule: once, when a person runs it.
            trigger: Trigger {
                on_change: false,
                every_minutes: 0,
            },
            when: When {
                all: true,
                groups: vec![Group {
                    all: false,
                    conditions: paths
                        .into_iter()
                        .map(|p| {
                            Condition::is(Test::Path {
                                text: Text {
                                    op: TextOp::Is,
                                    value: p,
                                },
                            })
                        })
                        .collect(),
                }],
            },
            actions: vec![action],
            ..Rule::default()
        })
        .collect()
}

fn summary(ctx: &Ctx, plan: &Plan) -> Value {
    let count = |o: &str| {
        plan.ops
            .iter()
            .filter(|x| serde_json::to_value(&x.outcome).ok() == Some(json!(o)))
            .count()
    };
    let rules: Vec<Value> = rules_of(ctx, plan)
        .into_iter()
        .map(|r| json!({ "name": r.name, "files": r.when.groups[0].conditions.len(), "state": "off, dry run" }))
        .collect();
    json!({
        "plan_id": plan.id, "title": plan.title, "applied": plan.applied,
        "apply": count("apply"), "become_rules": count("rule"), "refused": count("refused"),
        "operations": plan.ops,
        "proposed_rules": rules,
        "next": "Show the person this preview; apply_plan carries out the `apply` operations and proposes the rules (off, dry run). Refused operations are skipped."
    })
}

/// A free name in a Drive folder: the name, else "name (2)", "name (3)", ….
fn free_name(ctx: &Ctx, parent: Option<TreeID>, name: &str, me: Option<TreeID>) -> String {
    let (stem, ext) = match name.rsplit_once('.') {
        Some((s, e)) if !s.is_empty() => (s.to_string(), format!(".{e}")),
        _ => (name.to_string(), String::new()),
    };
    let mut put = name.to_string();
    let mut n = 2;
    while tree::lookup(&ctx.a.files, parent, &put).is_some_and(|e| Some(e.id) != me) {
        put = format!("{stem} ({n}){ext}");
        n += 1;
    }
    put
}

/// A Drive folder, made where missing (only folders are made).
fn ensure_folder(ctx: &Ctx, path: &str) -> Result<Option<TreeID>, String> {
    let mut cur: Option<TreeID> = None;
    for part in path.split('/').filter(|p| !p.is_empty()) {
        cur = Some(match tree::lookup(&ctx.a.files, cur, part) {
            Some(e) if e.dir => e.id,
            Some(_) => return Err(format!("{part} in drive:{path} is a file, not a folder")),
            None => tree::mkdir(&ctx.a.files, cur, part).map_err(|e| e.to_string())?,
        });
    }
    Ok(cur)
}

async fn drive_act(ctx: &Ctx, act: &Act) -> Result<Value, String> {
    match act {
        Act::DriveCopy { from, folder, name } => {
            let e = tree::entry(&ctx.a.files, *from).ok_or("the file is gone from the Drive")?;
            // The blob it shares must be whole on the node (checked by hash there).
            if !ctx.head(&format!("/blobs/{}", e.hash)).await {
                return Err("its bytes are not on the node yet; not copied".into());
            }
            ctx.charge(1)?;
            let parent = ensure_folder(ctx, folder)?;
            let put = free_name(ctx, parent, name, None);
            let id = tree::create_file(&ctx.a.files, parent, &put).map_err(|e| e.to_string())?;
            tree::set_content(&ctx.a.files, id, e.size, &e.hash).map_err(|e| e.to_string())?;
            ctx.a.commit_files();
            Ok(json!({ "copied_to": format!("drive:{}", tree::path_of(&ctx.a.files, id)) }))
        }
        Act::DriveMove { from, folder, name } => {
            ctx.charge(1)?;
            tree::entry(&ctx.a.files, *from).ok_or("the file is gone from the Drive")?;
            let parent = ensure_folder(ctx, folder)?;
            let put = free_name(ctx, parent, name, Some(*from));
            tree::rename(&ctx.a.files, *from, parent, &put).map_err(|e| e.to_string())?;
            ctx.a.commit_files();
            Ok(json!({ "now_at": format!("drive:{}", tree::path_of(&ctx.a.files, *from)) }))
        }
        _ => Err("not a Drive operation".into()),
    }
}

pub async fn apply(ctx: &Ctx, args: &Value) -> Result<Value, String> {
    let id = opt_str(args, "plan_id").ok_or("apply_plan needs a plan_id")?;
    let plan = ctx
        .plans
        .lock()
        .expect("plans")
        .get(id)
        .cloned()
        .ok_or_else(|| format!("no plan {id} in this session; make it with `plan`"))?;
    if plan.applied {
        return Err(format!("refused: plan {id} was already applied"));
    }
    let rules = rules_of(ctx, &plan);
    let applying: Vec<&Op> = plan
        .ops
        .iter()
        .filter(|o| matches!(o.outcome, Outcome::Apply))
        .collect();
    if dry(args) {
        let mut v = summary(ctx, &plan);
        v["dry_run"] = json!(true);
        return Ok(v);
    }
    let need = applying.len() + rules.len();
    if ctx.writes_used() + need > ctx.opts.max_writes {
        return Err(format!(
            "refused: this plan needs at least {need} writes and the session has {} left",
            ctx.opts.max_writes.saturating_sub(ctx.writes_used())
        ));
    }
    let mut results = Vec::new();
    for o in &plan.ops {
        let r = match (&o.outcome, &o.act) {
            (Outcome::Apply, Act::Tool { tool, args }) => match *tool {
                "tag" => write::tag(ctx, args, true).await,
                "untag" => write::tag(ctx, args, false).await,
                "set_field" => write::set_field(ctx, args).await,
                _ => write::add_to_project(ctx, args).await,
            },
            (Outcome::Apply, act) => drive_act(ctx, act).await,
            (Outcome::Rule, _) => Ok(json!("in a proposed rule (below)")),
            _ => Ok(json!({ "skipped": o.why })),
        };
        results.push(match r {
            Ok(v) => json!({ "i": o.i, "op": o.op, "ok": v }),
            Err(e) => json!({ "i": o.i, "op": o.op, "error": e }),
        });
    }
    let mut proposed = Vec::new();
    for r in rules {
        let saved = (|| {
            ctx.charge(1)?;
            let mut r = r.clone();
            r.by = ctx.author.clone();
            crate::rules::resolve_names(&ctx.a, &ctx.e, &mut r)?;
            drive_rules::Matcher::new(&r).map_err(|e| e.to_string())?;
            write::save_proposed(ctx, &r)
        })();
        proposed.push(match saved {
            Ok(id) => json!({ "rule": r.name, "id": id, "state": "off, dry run" }),
            Err(e) => json!({ "rule": r.name, "error": e }),
        });
    }
    for p in &proposed {
        if let Some(rid) = p["id"].as_str()
            && let Some(r) = drive_rules::doc::rule(&ctx.e.rules, rid)
        {
            super::read::ask_dry_run(ctx, &r).await;
        }
    }
    if let Some(p) = ctx.plans.lock().expect("plans").get_mut(id) {
        p.applied = true;
    }
    Ok(json!({
        "plan_id": id, "results": results, "proposed_rules": proposed, "by": ctx.author,
        "next": if proposed.is_empty() { "done".to_string() } else {
            "The proposed rules are off and dry runs: show the person `rule_preview` for each; they switch them on and live in the Drive app or with drive rules on|live NAME.".to_string()
        }
    }))
}

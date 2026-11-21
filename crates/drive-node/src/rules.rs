//! File rules over the Drive tree (docs/rules.md): the devices' agents run
//! the rules over their own locations; the node runs them over each org's
//! Drive tree, with the same evaluator (`drive_rules`). Its actions are
//! document edits only (tags, fields, moves and copies inside the tree,
//! the Trash folder), each journalled with its undo; a copy to a
//! computer's location is left in the journal for that computer's agent.
//!
//! Every 15 s the node looks at each org that has rules: "Run now"
//! requests, schedules, the tree having changed (rules that run on
//! change), and undos asked of it.

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use drive_files::{projects, space, tree};
use drive_rules::eval::{self, Effect, Facts, Item, Spot};
use drive_rules::guard::Guard;
use drive_rules::journal::{self, Entry, Preview, Row, Status, Undo};
use drive_rules::names::is_temporary;
use drive_rules::{DeviceInfo, LocInfo, Matcher, Mode, Rule, Runner, World, doc};
use drive_spine::kinds::Kinds;
use drive_spine::tags::{Library, Target};
use loro::{ExportMode, LoroDoc};
use serde::{Deserialize, Serialize};

use crate::rooms::Rooms;

const SETTLE_MS: i64 = 60_000;
const NODE: &str = "node";

#[derive(Default, Serialize, Deserialize)]
struct State {
    guard: Guard,
    handled: BTreeMap<String, i64>,
    scheduled: BTreeMap<String, i64>,
    /// Org → the files document's version the on-change rules last saw.
    seen_files: BTreeMap<String, Vec<u8>>,
    /// Rule → the rule as last run (switched on, made live or edited:
    /// it runs once at once).
    #[serde(default)]
    seen_rules: BTreeMap<String, String>,
}

/// What a rule is, as a string (its switches and its content).
fn sig(r: &Rule) -> String {
    serde_json::to_string(r).unwrap_or_default()
}

async fn copy_of(rooms: &Rooms, id: &str) -> LoroDoc {
    let bytes = rooms
        .read(id, |d| d.export(ExportMode::Snapshot).unwrap_or_default())
        .await;
    let d = LoroDoc::new();
    let _ = d.import(&bytes);
    d
}

/// Run until the process stops.
pub async fn run_loop(rooms: Arc<Rooms>, data: PathBuf) {
    let path = data.join("rules-state.json");
    let mut st: State = std::fs::read(&path)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default();
    let mut tick = tokio::time::interval(Duration::from_secs(15));
    loop {
        tick.tick().await;
        let orgs: Vec<String> = match rooms.store.list().await {
            Ok(l) => l
                .into_iter()
                .filter_map(|(id, _)| {
                    id.strip_suffix(&format!("/{}", doc::RULES))
                        .map(str::to_string)
                })
                .collect(),
            Err(_) => continue,
        };
        for org in orgs {
            pass(&rooms, &org, &mut st).await;
        }
        st.guard.prune(drive_files::now_ms());
        if let Ok(b) = serde_json::to_vec(&st) {
            let _ = std::fs::write(&path, b);
        }
    }
}

async fn pass(rooms: &Rooms, org: &str, st: &mut State) {
    let now = drive_files::now_ms();
    let rules_id = doc::rules_doc(org);
    let rules = rooms.read(&rules_id, doc::rules).await;
    let files_id = drive_files::files_doc(org);
    let vv = rooms.read(&files_id, |d| d.oplog_vv().encode()).await;
    let tree_changed = st.seen_files.get(org) != Some(&vv);
    for r in rules.iter().filter(|r| eval::covers_drive(r)) {
        let req = rooms.read(&rules_id, |d| doc::request(d, &r.id)).await;
        let handled = st.handled.get(&r.id).copied().unwrap_or(0);
        let why = if let Some(q) = req.filter(|q| q.at > handled) {
            st.handled.insert(r.id.clone(), q.at);
            Some(q.live && r.enabled && r.mode == Mode::Live)
        } else if r.enabled
            && r.trigger.every_minutes > 0
            && now - st.scheduled.get(&r.id).copied().unwrap_or(0)
                >= i64::from(r.trigger.every_minutes) * 60_000
        {
            st.scheduled.insert(r.id.clone(), now);
            Some(r.mode == Mode::Live)
        } else if r.enabled && st.seen_rules.get(&r.id) != Some(&sig(r)) {
            Some(r.mode == Mode::Live)
        } else if r.enabled && r.trigger.on_change && tree_changed {
            Some(r.mode == Mode::Live)
        } else {
            None
        };
        if let Some(live) = why {
            st.seen_rules.insert(r.id.clone(), sig(r));
            run(rooms, org, r, live, st).await;
        }
    }
    // What this pass changed counts as seen (rules never feed themselves).
    let vv = rooms.read(&files_id, |d| d.oplog_vv().encode()).await;
    st.seen_files.insert(org.to_string(), vv);
    undos(rooms, org, st).await;
}

/// One rule over an org's Drive tree.
async fn run(rooms: &Rooms, org: &str, rule: &Rule, live: bool, st: &mut State) -> journal::Run {
    let now = drive_files::now_ms();
    let journal_id = journal::journal_doc(org);
    let mut out = journal::Run {
        rule: rule.id.clone(),
        device: NODE.into(),
        at: now,
        live,
        ..journal::Run::default()
    };
    let matcher = match Matcher::new(rule) {
        Ok(m) => m,
        Err(e) => {
            out.error = e.to_string();
            let o = out.clone();
            rooms
                .edit(&journal_id, "drive rules (node)", |j| {
                    journal::put_run(j, &o)
                })
                .await
                .ok();
            return out;
        }
    };
    let files_id = drive_files::files_doc(org);
    let tags_id = drive_spine::tags::tags_doc(org);
    let files = copy_of(rooms, &files_id).await;
    let tags = copy_of(rooms, &tags_id).await;
    let reg = copy_of(rooms, &drive_files::space_doc(org)).await;
    let spine = copy_of(rooms, &drive_spine::spine_doc(org)).await;
    let kinds = Kinds::load(&spine);
    let lib = Library::load(&tags);
    let devices: Vec<DeviceInfo> = space::devices(&reg)
        .into_iter()
        .map(|d| DeviceInfo {
            id: d.id,
            name: d.name,
            roles: d.roles,
        })
        .collect();
    let locations: Vec<LocInfo> = space::locations(&reg)
        .into_iter()
        .map(|l| LocInfo {
            id: l.id,
            name: l.name,
            device: l.device,
            in_storage: !l.storage.is_empty(),
        })
        .collect();
    let projs = projects::projects(&reg);
    let world = World {
        now,
        kinds: &kinds,
        tags: &lib,
        devices: &devices,
        locations: &locations,
        projects: &projs,
        runner: Runner::Node,
    };
    let author = format!("rule {} (node)", rule.name);
    let run_id = format!("node-{now:x}");
    let mut rows = Vec::new();
    for (path, e) in tree::files(&files) {
        if e.dir || tree::in_trash(&files, e.id) || path.split('/').any(is_temporary) {
            continue;
        }
        let id = tree::id_string(e.id);
        let item = Item::Drive {
            id: id.clone(),
            path,
        };
        let holders: Vec<String> = space::holders(&reg, &e.hash)
            .into_iter()
            .map(|(w, _)| w)
            .collect();
        let facts = Facts {
            item: Some(item.clone()),
            size: e.size,
            modified: e.mtime,
            content: e.hash.clone(),
            copies: holders.len().max(1) as u32,
            holders,
            ..Facts::default()
        };
        if !matcher.matches(&facts, &world) {
            continue;
        }
        if e.hash.is_empty() || now - e.mtime < SETTLE_MS {
            out.skipped += 1;
            continue;
        }
        if st
            .guard
            .holds_off(&rule.id, &item.key(), &e.hash, now)
            .is_some()
        {
            out.skipped += 1;
            continue;
        }
        let fp = drive_rules::guard::fingerprint(&e.hash, e.size, e.mtime);
        if st.guard.handled(&rule.id, &item.key(), &fp) {
            continue;
        }
        let plan = matcher.plan(&facts, &world);
        out.matched += 1;
        rows.push(Row::of(&plan));
        if !live || !plan.does_something() {
            continue;
        }
        st.guard.did(&rule.id, &item.key(), &fp);
        let mut cur = item.clone();
        for step in &plan.steps {
            if step.blocked.is_some() {
                out.skipped += 1;
                continue;
            }
            if step.noop {
                continue;
            }
            let mut j = Entry {
                run: run_id.clone(),
                device: NODE.into(),
                rule: rule.id.clone(),
                rule_name: rule.name.clone(),
                at: drive_files::now_ms(),
                action: step.effect.kind().into(),
                says: step.says.clone(),
                before: Some(cur.clone()),
                content: e.hash.clone(),
                ..Entry::default()
            };
            let result: Result<Option<Item>, String> = match &step.effect {
                Effect::Tag { .. } | Effect::Field { .. } => {
                    let (t, eff, a) = (
                        Target::File(id.clone()),
                        step.effect.clone(),
                        author.clone(),
                    );
                    rooms
                        .edit(&tags_id, &author, move |d| {
                            drive_rules::tree::apply_tags(d, &t, &eff, &a, drive_files::now_ms())
                        })
                        .await
                        .map(|u| {
                            j.undo = u;
                            None
                        })
                }
                Effect::Move {
                    to: Spot::Drive { .. },
                }
                | Effect::Copy {
                    to: Spot::Drive { .. },
                }
                | Effect::Trash => {
                    let (c, eff) = (cur.clone(), step.effect.clone());
                    rooms
                        .edit(&files_id, &author, move |d| {
                            drive_rules::tree::apply_files(d, &c, &eff)
                        })
                        .await
                        .map(|(after, u)| {
                            j.undo = u;
                            Some(after)
                        })
                }
                Effect::Copy {
                    to:
                        Spot::Location {
                            device, location, ..
                        },
                } => {
                    // That computer's agent fetches the blob when it is on.
                    j.status = Some(Status::Waiting);
                    j.for_device = device.clone();
                    j.blob = e.hash.clone();
                    let to = match &step.effect {
                        Effect::Copy { to } => to.path(),
                        _ => String::new(),
                    };
                    Ok(Some(Item::Location {
                        device: device.clone(),
                        location: location.clone(),
                        path: to,
                    }))
                }
                other => Err(format!("{} is not done for Drive files", other.kind())),
            };
            match result {
                Ok(after) => {
                    st.guard.touch(&rule.id, &cur.key(), j.at);
                    if let Some(a) = &after {
                        st.guard.made(&rule.id, &a.key(), &e.hash, j.at);
                        if matches!(step.effect, Effect::Move { .. }) {
                            cur = a.clone();
                        }
                    }
                    j.after = after;
                    if j.status.is_none() {
                        j.status = Some(Status::Done);
                    }
                    if j.status == Some(Status::Waiting) {
                        out.waiting += 1;
                    } else {
                        out.done += 1;
                    }
                }
                Err(err) => {
                    tracing::warn!(rule.name = %rule.name, rules.action = %j.action, error = %err, "a rule's action failed on the Drive tree");
                    j.status = Some(Status::Failed);
                    j.error = err;
                    out.failed += 1;
                }
            }
            let failed = j.status == Some(Status::Failed);
            rooms
                .edit(&journal_id, &author, |d| journal::record(d, &j))
                .await
                .ok();
            if failed {
                break;
            }
        }
    }
    let p = Preview {
        rule: rule.id.clone(),
        device: NODE.into(),
        at: now,
        matched: out.matched,
        rows,
    };
    let o = out.clone();
    rooms
        .edit(&journal_id, &author, |d| {
            let _ = journal::put_preview(d, &p);
            journal::put_run(d, &o)
        })
        .await
        .ok();
    tracing::info!(
        rule.name = %rule.name,
        rules.org = %org,
        rules.live = live,
        rules.matched = out.matched,
        rules.done = out.done,
        rules.failed = out.failed,
        "a rule ran over the Drive tree"
    );
    out
}

/// Undos asked of the node (its own entries that are not a computer's).
async fn undos(rooms: &Rooms, org: &str, st: &mut State) {
    let journal_id = journal::journal_doc(org);
    let asked: Vec<Entry> = rooms
        .read(&journal_id, |d| {
            journal::entries(d)
                .into_iter()
                .filter(|e| {
                    e.device == NODE
                        && e.undo_requested > 0
                        && matches!(
                            e.status,
                            Some(Status::Done) | Some(Status::Waiting) | Some(Status::UndoRefused)
                        )
                })
                .collect()
        })
        .await;
    if asked.is_empty() {
        return;
    }
    let rules: HashMap<String, Rule> = rooms
        .read(&doc::rules_doc(org), doc::rules)
        .await
        .into_iter()
        .map(|r| (r.id.clone(), r))
        .collect();
    for mut e in asked {
        let author = "undo (node)";
        let result: Result<(), String> = if e.status == Some(Status::Waiting) {
            e.for_device.clear();
            Ok(())
        } else {
            match &e.undo {
                Undo::Tag { .. } | Undo::Field { .. } => {
                    let u = e.undo.clone();
                    rooms
                        .edit(&drive_spine::tags::tags_doc(org), author, move |d| {
                            drive_rules::tree::undo_tags(d, &u, author, drive_files::now_ms())
                        })
                        .await
                }
                Undo::TreeMove { .. } | Undo::TreeTrash { .. }
                    if rules
                        .get(&e.rule)
                        .is_some_and(|r| r.enabled && r.mode == Mode::Live) =>
                {
                    Err(format!(
                        "{} is live and would do it again at once: switch it off or to a dry run first",
                        e.rule_name
                    ))
                }
                Undo::TreeMove { .. } | Undo::TreeTrash { .. } | Undo::TreeCopy { .. } => {
                    let u = e.undo.clone();
                    rooms
                        .edit(&drive_files::files_doc(org), author, move |d| {
                            drive_rules::tree::undo_files(d, &u)
                        })
                        .await
                }
                _ => Err("this action cannot be undone".into()),
            }
        };
        match result {
            Ok(()) => {
                e.status = Some(Status::Undone);
                e.error.clear();
            }
            Err(err) => {
                tracing::warn!(rule.name = %e.rule_name, error = %err, "an undo was refused");
                e.status = Some(Status::UndoRefused);
                e.error = err;
            }
        }
        e.undo_requested = 0;
        if let Some(b) = &e.before {
            st.guard.touch(&e.rule, &b.key(), drive_files::now_ms());
        }
        rooms
            .edit(&journal_id, author, |d| journal::record(d, &e))
            .await
            .ok();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use drive_rules::{Action, Condition, Dest, Group, Test, Text, TextOp, When};

    async fn rooms(dir: &std::path::Path) -> Rooms {
        let store = crate::store::Store::open(&dir.join("drive.db"))
            .await
            .unwrap();
        Rooms::new(store, crate::auth::Auth::new(None))
    }

    fn mix_rule() -> Rule {
        Rule {
            name: "File mixes".into(),
            when: When {
                all: true,
                groups: vec![Group {
                    all: true,
                    conditions: vec![Condition::is(Test::Name {
                        text: Text {
                            op: TextOp::Contains,
                            value: "mix".into(),
                        },
                    })],
                }],
            },
            actions: vec![Action::Move {
                to: Dest::Drive {
                    path: "Mixes/{year}".into(),
                },
                name: String::new(),
            }],
            ..Rule::default()
        }
    }

    #[tokio::test]
    async fn the_drive_tree_dry_run_changes_nothing_and_live_runs_undo() {
        let dir = tempfile::tempdir().unwrap();
        let rooms = rooms(dir.path()).await;
        let org = "home";
        let files_id = drive_files::files_doc(org);
        let old = drive_files::now_ms() - 3_600_000;
        let id = rooms
            .edit(&files_id, "test", |d| {
                let inbox = tree::mkdir(d, None, "Inbox").unwrap();
                let f = tree::create_file(d, Some(inbox), "song mix.wav").unwrap();
                tree::set_content(d, f, 3, "abc").unwrap();
                // Written an hour ago (rules wait for fresh files to settle).
                d.get_tree("files")
                    .get_meta(f)
                    .unwrap()
                    .insert("mtime", old)
                    .unwrap();
                tree::create_file(d, Some(inbox), "notes.txt").unwrap();
                tree::id_string(f)
            })
            .await;
        let rule_id = rooms
            .edit(&doc::rules_doc(org), "test", |d| {
                doc::save(d, &mix_rule(), "test", 1).unwrap()
            })
            .await;
        let rule = rooms
            .read(&doc::rules_doc(org), |d| doc::rule(d, &rule_id))
            .await
            .unwrap();
        let mut st = State::default();

        // A new rule is a dry run: it says what it would do, changes nothing.
        let before = rooms
            .read(&files_id, |d| d.export(ExportMode::Snapshot).unwrap())
            .await;
        let r = run(&rooms, org, &rule, false, &mut st).await;
        assert_eq!(r.matched, 1);
        assert_eq!(r.done, 0);
        let after = rooms
            .read(&files_id, |d| d.export(ExportMode::Snapshot).unwrap())
            .await;
        assert_eq!(before, after, "a dry run edits nothing");
        let p = rooms
            .read(&journal::journal_doc(org), |d| {
                journal::previews(d, &rule_id)
            })
            .await;
        assert!(p[0].rows[0].steps[0].says.contains("Mixes/"), "{p:?}");

        // Live: moved inside the tree, journalled; the same pass's guard
        // keeps it from being taken again.
        let r = run(&rooms, org, &rule, true, &mut st).await;
        assert_eq!((r.matched, r.done), (1, 1));
        let path = rooms
            .read(&files_id, |d| {
                tree::path_of(d, tree::parse_id(&id).unwrap())
            })
            .await;
        assert!(path.starts_with("Mixes/"), "{path}");
        let r = run(&rooms, org, &rule, true, &mut st).await;
        assert_eq!(r.done, 0, "a rule leaves alone what it just did");

        // Undo (the rule is off): back where it was.
        let entry = rooms
            .read(&journal::journal_doc(org), |d| journal::entries(d))
            .await
            .remove(0);
        rooms
            .edit(&journal::journal_doc(org), "test", |d| {
                journal::request_undo(d, &entry.id, 5)
            })
            .await
            .unwrap();
        undos(&rooms, org, &mut st).await;
        let path = rooms
            .read(&files_id, |d| {
                tree::path_of(d, tree::parse_id(&id).unwrap())
            })
            .await;
        assert_eq!(path, "Inbox/song mix.wav");
        let e = rooms
            .read(&journal::journal_doc(org), |d| journal::entry(d, &entry.id))
            .await
            .unwrap();
        assert_eq!(e.status, Some(Status::Undone));
    }
}

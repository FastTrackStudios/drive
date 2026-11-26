//! File rules on this device (docs/rules.md): the agent runs the rules
//! whose scope includes it over its own locations' files, after a
//! location changes (its watcher), on a schedule, and when someone asks
//! ("Run now"); it fetches copies other devices' rules sent it, and undoes
//! what it did when asked. The decisions are `drive_rules`' (the same code
//! as the app's preview); the changes on disk are [`crate::rules_fs`]'s.
//!
//! ```text
//! drive rules ls                      # every rule: on/off, dry run/live, last runs
//! drive rules show NAME               # the rule, and each device's latest dry run
//! drive rules run NAME [--live]       # run it here now (and ask every device and the node)
//! drive rules journal [NAME]          # what rules did, newest first
//! drive rules undo ID                 # take an action back
//! drive rules add FILE.json           # a new rule (off, dry run) from JSON
//! drive rules on|off NAME             # switch it on or off
//! drive rules live|dry-run NAME       # let it act, or only say what it would do
//! drive rules rm NAME
//! ```

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use drive_files::index::IndexEntry;
use drive_files::{projects, space, tree as ftree};
use drive_rules::eval::{self, Effect, Facts, Item, Spot};
use drive_rules::guard::Guard;
use drive_rules::journal::{self, Entry, Preview, Row, Status, Undo};
use drive_rules::names::{bundle_of, is_bundle, is_temporary};
use drive_rules::{DeviceInfo, LocInfo, Matcher, Mode, Rule, Runner, World, doc};
use drive_spine::kinds::Kinds;
use drive_spine::tags::{Library, Target};
use loro::LoroDoc;
use serde::{Deserialize, Serialize};

use crate::agent::Agent;
use crate::rules_fs::{self, TRASH_DIR};
use crate::{out, settle, start};

/// A file modified this recently may still be being written: rules wait.
pub const SETTLE_MS: i64 = 60_000;

/// What this device remembers between runs (in its data folder).
#[derive(Default, Serialize, Deserialize)]
struct State {
    guard: Guard,
    /// Rule → the newest "Run now" request this device answered.
    handled: BTreeMap<String, i64>,
    /// Rule → when it last ran on its schedule here.
    scheduled: BTreeMap<String, i64>,
    /// Rule → the rule as last run here (switched on, made live or
    /// edited: it runs once at once).
    #[serde(default)]
    seen_rules: BTreeMap<String, String>,
}

/// The rules, the journal and the tags, open; this device's memory.
pub struct Engine {
    pub rules_id: String,
    pub journal_id: String,
    pub tags_id: String,
    pub rules: LoroDoc,
    pub journal: LoroDoc,
    pub tags: LoroDoc,
    state: tokio::sync::Mutex<State>,
}

/// What one run of one rule here came to.
#[derive(Debug, Default)]
pub struct Outcome {
    pub run: journal::Run,
    pub rows: Vec<Row>,
}

fn file_ms(t: std::io::Result<std::time::SystemTime>) -> Option<i64> {
    t.ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as i64)
}

/// One thing a rule may take in a location: a file, or a bundle whole.
struct Candidate {
    path: String,
    bundle: bool,
    size: u64,
    mtime: i64,
    sampled: String,
}

/// The bundles among a page of a location's entries: each one's size
/// (what is inside it, summed) and newest change, added to `bundles`. Every
/// page of the location goes through this before [`candidates`].
fn bundle_sizes(entries: &[IndexEntry], bundles: &mut HashMap<String, (u64, i64)>) {
    for e in entries {
        if let Some(b) = bundle_of(&e.path)
            && b != e.path
        {
            let s = bundles.entry(b.to_string()).or_default();
            s.0 += e.size;
            s.1 = s.1.max(e.mtime);
        }
    }
}

/// A page of a location's files and bundles (never what is inside a
/// bundle, never temporary or partial files, never plain folders), with
/// the location's `bundles` from [`bundle_sizes`].
fn candidates(entries: &[IndexEntry], bundles: &HashMap<String, (u64, i64)>) -> Vec<Candidate> {
    let skip = |e: &IndexEntry| e.path.split('/').any(|p| is_temporary(p) || p == TRASH_DIR);
    entries
        .iter()
        .filter(|e| !skip(e))
        .filter_map(|e| {
            let inside = bundle_of(&e.path).is_some_and(|b| b != e.path);
            if inside {
                return None;
            }
            if e.dir {
                if !is_bundle(e.name()) {
                    return None;
                }
                let (size, mtime) = bundles.get(&e.path).copied().unwrap_or((0, e.mtime));
                return Some(Candidate {
                    path: e.path.clone(),
                    bundle: true,
                    size,
                    mtime: mtime.max(e.mtime),
                    sampled: String::new(),
                });
            }
            Some(Candidate {
                path: e.path.clone(),
                bundle: false,
                size: e.size,
                mtime: e.mtime,
                sampled: e.sampled.clone(),
            })
        })
        .collect()
}

impl Engine {
    pub async fn open(a: &Agent) -> Engine {
        let space = a.cfg.lock().expect("cfg").space.clone();
        let rules_id = doc::rules_doc(&space);
        let journal_id = journal::journal_doc(&space);
        let tags_id = drive_spine::tags::tags_doc(&space);
        let rules = a.drive.open(&rules_id).await;
        let journal = a.drive.open(&journal_id).await;
        let tags = a.drive.open(&tags_id).await;
        let _ = tokio::time::timeout(std::time::Duration::from_secs(4), async {
            a.drive.synced(&rules_id).await;
            a.drive.synced(&journal_id).await;
            a.drive.synced(&tags_id).await;
        })
        .await;
        let state = std::fs::read(a.dir.join("rules-state.json"))
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        Engine {
            rules_id,
            journal_id,
            tags_id,
            rules,
            journal,
            tags,
            state: tokio::sync::Mutex::new(state),
        }
    }

    async fn save_state(&self, a: &Agent) {
        let mut st = self.state.lock().await;
        st.guard.prune(drive_files::now_ms());
        if let Ok(b) = serde_json::to_vec(&*st) {
            let _ = std::fs::write(a.dir.join("rules-state.json"), b);
        }
    }

    fn commit(&self, a: &Agent) {
        a.drive.commit(&self.journal_id);
        a.drive.commit(&self.tags_id);
        a.commit_space();
        a.commit_files();
    }

    pub(crate) fn me(&self, a: &Agent) -> DeviceInfo {
        let id = a.device_id();
        let d = space::devices(&a.space).into_iter().find(|d| d.id == id);
        DeviceInfo {
            name: d.as_ref().map_or_else(
                || a.cfg.lock().expect("cfg").device_name.clone(),
                |d| d.name.clone(),
            ),
            roles: d.map(|d| d.roles).unwrap_or_default(),
            id,
        }
    }

    /// The rules this device runs now: on, live or a dry run.
    pub fn mine(&self, a: &Agent) -> Vec<Rule> {
        let me = self.me(a);
        doc::rules(&self.rules)
            .into_iter()
            .filter(|r| r.enabled && eval::runs_on(r, &me))
            .collect()
    }

    /// The periodic part: "Run now" requests, schedules, copies waiting
    /// for this device, uploads to retry, undos asked for.
    pub async fn tick(&self, a: &Agent) {
        let now = drive_files::now_ms();
        let me = self.me(a);
        for r in doc::rules(&self.rules) {
            if !eval::runs_on(&r, &me) {
                continue;
            }
            let req = doc::request(&self.rules, &r.id);
            let handled = self
                .state
                .lock()
                .await
                .handled
                .get(&r.id)
                .copied()
                .unwrap_or(0);
            if let Some(req) = req.filter(|q| q.at > handled) {
                self.state.lock().await.handled.insert(r.id.clone(), req.at);
                // Asked for: a dry run whatever the rule's switches say;
                // live only when asked live of a rule that is on and live.
                let live = req.live && r.enabled && r.mode == Mode::Live;
                self.run(a, &r, live, None).await;
                continue;
            }
            let sig = serde_json::to_string(&r).unwrap_or_default();
            if r.enabled && self.state.lock().await.seen_rules.get(&r.id) != Some(&sig) {
                self.state.lock().await.seen_rules.insert(r.id.clone(), sig);
                self.run(a, &r, r.mode == Mode::Live, None).await;
                continue;
            }
            if r.enabled && r.trigger.every_minutes > 0 {
                let last = self
                    .state
                    .lock()
                    .await
                    .scheduled
                    .get(&r.id)
                    .copied()
                    .unwrap_or(0);
                if now - last >= i64::from(r.trigger.every_minutes) * 60_000 {
                    self.state.lock().await.scheduled.insert(r.id.clone(), now);
                    self.run(a, &r, r.mode == Mode::Live, None).await;
                }
            }
        }
        self.incoming(a).await;
        self.retry_uploads(a).await;
        self.undos(a).await;
        self.save_state(a).await;
    }

    /// A location changed and was indexed again: the rules that run on
    /// change, over it.
    pub async fn after_index(&self, a: &Agent, locations: &[String]) {
        let projs = projects::projects(&a.space);
        for r in self.mine(a).into_iter().filter(|r| {
            r.trigger.on_change
                && locations
                    .iter()
                    .any(|l| eval::covers_location(r, l, &projs))
        }) {
            // Over all of this device's places, so its preview stays whole.
            self.run(a, &r, r.mode == Mode::Live, None).await;
        }
        self.save_state(a).await;
    }

    /// Run one rule over this device's locations (or one of them). Live
    /// only when `live`; otherwise a dry run whose plan goes to the journal
    /// for the app's preview.
    pub async fn run(&self, a: &Agent, rule: &Rule, live: bool, only: Option<&str>) -> Outcome {
        let now = drive_files::now_ms();
        let me = self.me(a);
        let mut o = Outcome {
            run: journal::Run {
                rule: rule.id.clone(),
                device: me.id.clone(),
                at: now,
                live,
                ..journal::Run::default()
            },
            rows: Vec::new(),
        };
        let matcher = match Matcher::new(rule) {
            Ok(m) => m,
            Err(e) => {
                o.run.error = e.to_string();
                let _ = journal::put_run(&self.journal, &o.run);
                self.commit(a);
                return o;
            }
        };
        let moves_allowed = a.cfg.lock().expect("cfg").allow_rule_moves;
        let kinds = Kinds::load(&a.spine);
        let lib = Library::load(&self.tags);
        let devices: Vec<DeviceInfo> = space::devices(&a.space)
            .into_iter()
            .map(|d| DeviceInfo {
                id: d.id,
                name: d.name,
                roles: d.roles,
            })
            .collect();
        let all_locations = space::locations(&a.space);
        let locations: Vec<LocInfo> = all_locations
            .iter()
            .map(|l| LocInfo {
                id: l.id.clone(),
                name: l.name.clone(),
                device: l.device.clone(),
                in_storage: !l.storage.is_empty(),
            })
            .collect();
        let projs = projects::projects(&a.space);
        // Copies, by content, from every location's index: looked up a
        // page of candidates at a time, never gathered for every file.
        a.sync_indexes().await;
        let device_of: HashMap<&str, &str> = all_locations
            .iter()
            .map(|l| (l.id.as_str(), l.device.as_str()))
            .collect();
        let world = World {
            now,
            kinds: &kinds,
            tags: &lib,
            devices: &devices,
            locations: &locations,
            projects: &projs,
            runner: Runner::Device {
                id: me.id.clone(),
                moves_allowed,
            },
        };
        let run_id = format!("{}-{now:x}", me.id);
        for loc in a.my_locations() {
            if !loc.storage.is_empty()
                || only.is_some_and(|l| l != loc.id)
                || !eval::covers_location(rule, &loc.id, &projs)
            {
                continue;
            }
            if a.index_db.meta(&loc.id).await.ok().flatten().is_none() {
                continue;
            }
            let root = PathBuf::from(&loc.path);
            let mut bundles = HashMap::new();
            let mut pages = a.index_pages(&loc.id, "");
            while let Some(page) = pages.next().await {
                bundle_sizes(&page, &mut bundles);
            }
            let mut pages = a.index_pages(&loc.id, "");
            while let Some(page) = pages.next().await {
                let cands = candidates(&page, &bundles);
                let hashes: Vec<String> = cands
                    .iter()
                    .filter(|c| !c.sampled.is_empty())
                    .map(|c| c.sampled.clone())
                    .collect();
                let held = a.index_db.locations_of(&hashes).await.unwrap_or_default();
                for c in cands {
                    let abs = root.join(&c.path);
                    let meta = match std::fs::symlink_metadata(&abs) {
                        Ok(m) if !m.file_type().is_symlink() => m,
                        _ => continue,
                    };
                    let item = Item::Location {
                        device: me.id.clone(),
                        location: loc.id.clone(),
                        path: c.path.clone(),
                    };
                    let (n, holders) = held
                        .get(&c.sampled)
                        .map(|locs| {
                            let mut devs: Vec<String> = locs
                                .iter()
                                .filter_map(|l| device_of.get(l.as_str()).map(|d| d.to_string()))
                                .collect();
                            devs.sort();
                            devs.dedup();
                            (locs.len() as u32, devs)
                        })
                        .unwrap_or((1, vec![me.id.clone()]));
                    let facts = Facts {
                        item: Some(item.clone()),
                        bundle: c.bundle,
                        size: c.size,
                        modified: c.mtime,
                        created: file_ms(meta.created()),
                        added: None,
                        content: c.sampled.clone(),
                        copies: n,
                        holders,
                    };
                    if !matcher.matches(&facts, &world) {
                        continue;
                    }
                    if now - c.mtime < SETTLE_MS {
                        // Still being written, perhaps: the next pass sees it.
                        o.run.skipped += 1;
                        continue;
                    }
                    if let Some(why) = self.state.lock().await.guard.holds_off(
                        &rule.id,
                        &item.key(),
                        &c.sampled,
                        now,
                    ) {
                        tracing::debug!(rule.name = %rule.name, rules.held = %why, "a rule left a file alone");
                        o.run.skipped += 1;
                        continue;
                    }
                    let fp = drive_rules::guard::fingerprint(&c.sampled, c.size, c.mtime);
                    if self
                        .state
                        .lock()
                        .await
                        .guard
                        .handled(&rule.id, &item.key(), &fp)
                    {
                        // Done before, unchanged since: nothing to do.
                        continue;
                    }
                    let plan = matcher.plan(&facts, &world);
                    o.run.matched += 1;
                    o.rows.push(Row::of(&plan));
                    if !live || !plan.does_something() {
                        continue;
                    }
                    // Unchanged since it was indexed, or not now.
                    let changed = !c.bundle
                        && (meta.len() != c.size
                            || file_ms(meta.modified())
                                .is_some_and(|m| (m - c.mtime).abs() > 2_000));
                    if changed {
                        o.run.skipped += 1;
                        continue;
                    }
                    self.carry_out(a, rule, &run_id, &facts, &plan, &root, &mut o.run)
                        .await;
                    self.state
                        .lock()
                        .await
                        .guard
                        .did(&rule.id, &item.key(), &fp);
                }
            }
        }
        let _ = journal::put_preview(
            &self.journal,
            &Preview {
                rule: rule.id.clone(),
                device: me.id.clone(),
                at: now,
                matched: o.run.matched,
                rows: o.rows.clone(),
            },
        );
        let _ = journal::put_run(&self.journal, &o.run);
        self.commit(a);
        tracing::info!(
            rule.name = %rule.name,
            rules.live = live,
            rules.matched = o.run.matched,
            rules.done = o.run.done,
            rules.failed = o.run.failed,
            rules.waiting = o.run.waiting,
            rules.skipped = o.run.skipped,
            "a rule ran"
        );
        o
    }

    /// Do a plan's steps to one file, journalling each.
    #[allow(clippy::too_many_arguments)]
    async fn carry_out(
        &self,
        a: &Agent,
        rule: &Rule,
        run_id: &str,
        facts: &Facts,
        plan: &drive_rules::Plan,
        root: &Path,
        run: &mut journal::Run,
    ) {
        let me = a.device_id();
        let by = format!("rule {} on {}", rule.name, self.me(a).name);
        let mut cur = facts.item().clone();
        let mut cur_root = root.to_path_buf();
        for step in &plan.steps {
            if step.blocked.is_some() {
                run.skipped += 1;
                continue;
            }
            if step.noop {
                continue;
            }
            let now = drive_files::now_ms();
            let mut e = Entry {
                run: run_id.to_string(),
                device: me.clone(),
                rule: rule.id.clone(),
                rule_name: rule.name.clone(),
                at: now,
                action: step.effect.kind().to_string(),
                says: step.says.clone(),
                before: Some(cur.clone()),
                content: facts.content.clone(),
                ..Entry::default()
            };
            let r = self
                .step(a, &by, facts, &cur, &cur_root, &step.effect, &mut e)
                .await;
            match r {
                Ok(after) => {
                    if e.status.is_none() {
                        e.status = Some(Status::Done);
                    }
                    let mut st = self.state.lock().await;
                    st.guard.touch(&rule.id, &cur.key(), now);
                    if let Some(after) = &after {
                        st.guard.made(&rule.id, &after.key(), &facts.content, now);
                    }
                    drop(st);
                    if matches!(step.effect, Effect::Move { .. })
                        && let Some(Item::Location { location, .. }) = &after
                    {
                        if let Some(l) = a.my_locations().into_iter().find(|l| l.id == *location) {
                            cur_root = PathBuf::from(l.path);
                        }
                        cur = after.clone().expect("checked");
                    }
                    e.after = after;
                }
                Err(err) => {
                    e.status = Some(Status::Failed);
                    e.error = err;
                }
            }
            match e.status {
                Some(Status::Waiting) => run.waiting += 1,
                Some(Status::Failed) => {
                    tracing::warn!(rule.name = %rule.name, rules.action = %e.action, error = %e.error, "a rule's action failed");
                    run.failed += 1
                }
                _ => run.done += 1,
            }
            let _ = journal::record(&self.journal, &e);
            if e.status == Some(Status::Failed) {
                // Later steps assumed this one: stop here for this file.
                break;
            }
        }
    }

    /// One step. Returns where the file (or its copy) is after.
    #[allow(clippy::too_many_arguments)]
    async fn step(
        &self,
        a: &Agent,
        by: &str,
        facts: &Facts,
        cur: &Item,
        root: &Path,
        effect: &Effect,
        e: &mut Entry,
    ) -> Result<Option<Item>, String> {
        let now = drive_files::now_ms();
        let Item::Location { location, path, .. } = cur else {
            return Err("not a file of this device".into());
        };
        let src = root.join(path);
        match effect {
            Effect::Tag { .. } | Effect::Field { .. } => {
                let target = Target::Content(facts.content.clone());
                e.undo = drive_rules::tree::apply_tags(&self.tags, &target, effect, by, now)?;
                Ok(None)
            }
            Effect::Project { project } => {
                let label = cur.name().to_string();
                let part = projects::add_part(&a.space, project, &label, location, path)
                    .map_err(|e| e.to_string())?;
                e.undo = Undo::Part {
                    project: project.clone(),
                    part,
                };
                Ok(None)
            }
            Effect::Sidecars { kinds } => {
                a.answer_sidecars(location, path, kinds, crate::sidecars::Remake::All)
                    .await?;
                Ok(None)
            }
            Effect::Available => {
                let key = journal::wish_available(&self.journal, cur, &e.rule, now)
                    .map_err(|e| e.to_string())?;
                e.undo = Undo::Available { key };
                Ok(None)
            }
            Effect::Trash => Err(eval::NO_TRASH.into()),
            Effect::Nothing => Err("nothing to do".into()),
            Effect::Copy { to } => self.copy(a, &src, to, e).await.map(Some),
            Effect::Move { to } => {
                let Spot::Location {
                    device,
                    location: to_loc,
                    folder,
                    name,
                } = to
                else {
                    return Err("a move between a computer and the Drive is not built yet".into());
                };
                if *device != a.device_id() {
                    return Err("a move to another computer is not built yet".into());
                }
                // Checked again here, whatever the plan said.
                if !a.cfg.lock().expect("cfg").allow_rule_moves {
                    return Err(eval::MOVES_OFF.into());
                }
                let to_root = self.root_of(a, to_loc)?;
                // 1. Copy, verified byte for byte.
                let (copy, full) = rules_fs::copy_verified(&src, &to_root.join(folder), name)?;
                let to_rel = rel(&to_root, &copy);
                // 2. Only then the original, into .drive-trash (never unlinked).
                let trashed = match rules_fs::to_trash(root, path) {
                    Ok(t) => t,
                    Err(err) => {
                        e.error = format!("copied; the original stays where it was ({err})");
                        e.undo = Undo::DeviceCopy {
                            location: to_loc.clone(),
                            path: to_rel.clone(),
                            full,
                        };
                        return Ok(Some(Item::Location {
                            device: device.clone(),
                            location: to_loc.clone(),
                            path: to_rel,
                        }));
                    }
                };
                e.undo = Undo::DeviceMove {
                    location: location.clone(),
                    from: path.clone(),
                    trashed,
                    to_location: to_loc.clone(),
                    to: to_rel.clone(),
                    full,
                };
                Ok(Some(Item::Location {
                    device: device.clone(),
                    location: to_loc.clone(),
                    path: to_rel,
                }))
            }
        }
    }

    fn root_of(&self, a: &Agent, location: &str) -> Result<PathBuf, String> {
        a.my_locations()
            .into_iter()
            .find(|l| l.id == location && l.storage.is_empty())
            .map(|l| PathBuf::from(l.path))
            .ok_or_else(|| format!("{location} is not a folder of this device"))
    }

    async fn copy(&self, a: &Agent, src: &Path, to: &Spot, e: &mut Entry) -> Result<Item, String> {
        match to {
            Spot::Location {
                device,
                location,
                folder,
                name,
            } if *device == a.device_id() => {
                let root = self.root_of(a, location)?;
                let (copy, full) = rules_fs::copy_verified(src, &root.join(folder), name)?;
                let path = rel(&root, &copy);
                e.undo = Undo::DeviceCopy {
                    location: location.clone(),
                    path: path.clone(),
                    full,
                };
                Ok(Item::Location {
                    device: device.clone(),
                    location: location.clone(),
                    path,
                })
            }
            Spot::Location {
                device, location, ..
            } => {
                // Through the node: its bytes go up now, the other device
                // fetches them (now if it is on, else when it comes on).
                if src.is_dir() {
                    return Err("copying a bundle to another computer is not built yet".into());
                }
                e.for_device = device.clone();
                e.status = Some(Status::Waiting);
                e.after = Some(Item::Location {
                    device: device.clone(),
                    location: location.clone(),
                    path: to.path(),
                });
                match self.upload(a, src).await {
                    Ok(h) => e.blob = h,
                    Err(err) => e.error = format!("{err}; tried again until it goes"),
                }
                Ok(Item::Location {
                    device: device.clone(),
                    location: location.clone(),
                    path: to.path(),
                })
            }
            Spot::Drive { folder, name } => {
                if src.is_dir() {
                    return Err("copying a bundle into the Drive is not built yet".into());
                }
                let parent = drive_rules::tree::ensure_folder(&a.files, folder)?;
                let free = drive_rules::names::free_name(name, |n| {
                    ftree::lookup(&a.files, parent, n).is_some()
                });
                let entry = a.put(src, parent, &free).await?;
                let id = ftree::id_string(entry.id);
                e.undo = Undo::TreeCopy { id: id.clone() };
                Ok(Item::Drive {
                    id,
                    path: ftree::path_of(&a.files, entry.id),
                })
            }
        }
    }

    /// A file's bytes onto the node, checked by their hash there.
    async fn upload(&self, a: &Agent, src: &Path) -> Result<String, String> {
        let bytes = tokio::fs::read(src).await.map_err(|e| e.to_string())?;
        let h = drive_files::hash::of_bytes(&bytes);
        let url = format!("{}/blobs/{h}", a.cfg.lock().expect("cfg").node_http());
        match a.http.put(&url).body(bytes).send().await {
            Ok(r) if r.status().is_success() => {
                let _ = space::set_held(&a.space, "node", &h, Some("stored"));
                Ok(h)
            }
            Ok(r) => Err(format!("the node refused the copy ({})", r.status())),
            Err(e) => Err(format!("the node is out of reach ({e})")),
        }
    }

    /// Copies other devices' rules sent this device: fetched from the
    /// node, written under a free name, checked.
    pub async fn incoming(&self, a: &Agent) {
        let me = a.device_id();
        let mut changed = false;
        for mut e in journal::entries(&self.journal) {
            if e.status != Some(Status::Waiting) || e.for_device != me || e.blob.is_empty() {
                continue;
            }
            let Some(Item::Location { location, path, .. }) = e.after.clone() else {
                continue;
            };
            let result = async {
                let root = self.root_of(a, &location)?;
                let bytes = a.fetch_bytes(&e.blob).await?;
                rules_fs::under(&root, &path)?;
                let (folder, name) = path.rsplit_once('/').unwrap_or(("", path.as_str()));
                let dir = rules_fs::under(&root, folder)?;
                let fin = rules_fs::write_verified(&bytes, &e.blob, &dir, name)?;
                Ok::<String, String>(rel(&root, &fin))
            }
            .await;
            e.tries += 1;
            match result {
                Ok(p) => {
                    e.status = Some(Status::Done);
                    e.error.clear();
                    e.undo = Undo::DeviceCopy {
                        location: location.clone(),
                        path: p.clone(),
                        full: e.blob.clone(),
                    };
                    let key = Item::Location {
                        device: me.clone(),
                        location: location.clone(),
                        path: p.clone(),
                    };
                    self.state.lock().await.guard.made(
                        &e.rule,
                        &key.key(),
                        &e.content,
                        drive_files::now_ms(),
                    );
                    e.after = Some(key);
                    tracing::info!(rule.name = %e.rule_name, "a copy from another device arrived");
                }
                Err(err) => e.error = err,
            }
            let _ = journal::record(&self.journal, &e);
            changed = true;
        }
        if changed {
            self.commit(a);
        }
    }

    /// Copies for other devices whose bytes could not reach the node.
    async fn retry_uploads(&self, a: &Agent) {
        let me = a.device_id();
        let mut changed = false;
        for mut e in journal::entries(&self.journal) {
            if e.status != Some(Status::Waiting) || e.device != me || !e.blob.is_empty() {
                continue;
            }
            let Some(Item::Location { location, path, .. }) = &e.before else {
                continue;
            };
            let Ok(root) = self.root_of(a, location) else {
                continue;
            };
            e.tries += 1;
            let uploaded = match rules_fs::under(&root, path) {
                Ok(src) => self.upload(a, &src).await,
                Err(err) => Err(err),
            };
            match uploaded {
                Ok(h) => {
                    e.blob = h;
                    e.error.clear();
                }
                Err(err) => e.error = format!("{err}; tried again until it goes"),
            }
            let _ = journal::record(&self.journal, &e);
            changed = true;
        }
        if changed {
            self.commit(a);
        }
    }

    /// The device that undoes an entry: the one whose disk holds what it
    /// changed, else the one that did it.
    fn owner(&self, a: &Agent, e: &Entry) -> String {
        let loc_dev = |id: &str| {
            space::locations(&a.space)
                .into_iter()
                .find(|l| l.id == id)
                .map(|l| l.device)
        };
        match &e.undo {
            Undo::DeviceCopy { location, .. } | Undo::DeviceMove { location, .. } => {
                loc_dev(location).unwrap_or_else(|| e.device.clone())
            }
            _ if e.status == Some(Status::Waiting) && !e.for_device.is_empty() => {
                e.for_device.clone()
            }
            _ => e.device.clone(),
        }
    }

    async fn undos(&self, a: &Agent) {
        let me = a.device_id();
        let mut changed = false;
        for e in journal::entries(&self.journal) {
            if e.undo_requested == 0
                || !matches!(
                    e.status,
                    Some(Status::Done) | Some(Status::Waiting) | Some(Status::UndoRefused)
                )
                || self.owner(a, &e) != me
            {
                continue;
            }
            let _ = self.undo_here(a, e).await;
            changed = true;
        }
        if changed {
            self.commit(a);
        }
    }

    /// Undo one entry on this device. Refused (and said why) when what it
    /// made has changed since, or when the rule is live and would do it
    /// again at once.
    pub async fn undo_here(&self, a: &Agent, mut e: Entry) -> Result<(), String> {
        let by = format!("undo on {}", self.me(a).name);
        let now = drive_files::now_ms();
        let result: Result<(), String> = async {
            if e.status == Some(Status::Waiting) {
                // A copy that has not arrived: it simply will not.
                e.for_device.clear();
                return Ok(());
            }
            match &e.undo {
                Undo::None => Err("this action cannot be undone (it only made derived data)".into()),
                Undo::Tag { .. } | Undo::Field { .. } => {
                    drive_rules::tree::undo_tags(&self.tags, &e.undo, &by, now)
                }
                Undo::Part { project, part } => {
                    projects::remove_part(&a.space, project, part).map_err(|e| e.to_string())
                }
                Undo::Available { key } => {
                    journal::unwish_available(&self.journal, key).map_err(|e| e.to_string())
                }
                Undo::TreeMove { .. } | Undo::TreeCopy { .. } | Undo::TreeTrash { .. } => {
                    drive_rules::tree::undo_files(&a.files, &e.undo)
                }
                Undo::DeviceCopy { location, path, full } => {
                    let root = self.root_of(a, location)?;
                    let now_hash = rules_fs::full_hash(&rules_fs::under(&root, path)?)
                        .map_err(|_| format!("the copy {path} is no longer there"))?;
                    if now_hash != *full {
                        return Err(format!("the copy {path} changed since; it stays"));
                    }
                    rules_fs::to_trash(&root, path).map(|_| ())
                }
                Undo::DeviceMove {
                    location,
                    from,
                    trashed,
                    to_location,
                    to,
                    full,
                } => {
                    if let Some(r) = doc::rule(&self.rules, &e.rule)
                        && r.enabled
                        && r.mode == Mode::Live
                    {
                        return Err(format!(
                            "{} is live and would move it again at once: switch it off or to a dry run first",
                            r.name
                        ));
                    }
                    let root = self.root_of(a, location)?;
                    let to_root = self.root_of(a, to_location)?;
                    let now_hash = rules_fs::full_hash(&rules_fs::under(&to_root, to)?)
                        .map_err(|_| format!("the moved file {to} is no longer there"))?;
                    if now_hash != *full {
                        return Err(format!("{to} changed since it was moved; it stays"));
                    }
                    rules_fs::restore(&root, trashed, from)?;
                    rules_fs::to_trash(&to_root, to).map(|_| ())
                }
            }
        }
        .await;
        match &result {
            Ok(()) => {
                e.status = Some(Status::Undone);
                e.error.clear();
            }
            Err(err) => {
                e.status = Some(Status::UndoRefused);
                e.error = err.clone();
                tracing::warn!(rule.name = %e.rule_name, rules.action = %e.action, error = %err, "an undo was refused");
            }
        }
        e.undo_requested = 0;
        let _ = journal::record(&self.journal, &e);
        if let Some(item) = &e.before {
            self.state
                .lock()
                .await
                .guard
                .touch(&e.rule, &item.key(), now);
        }
        result
    }
}

fn rel(root: &Path, p: &Path) -> String {
    p.strip_prefix(root)
        .unwrap_or(p)
        .to_string_lossy()
        .replace('\\', "/")
}

// ── one runner per device ───────────────────────────────────────────────

/// `serve` claims the rules of this device: the command line then asks
/// it rather than running them a second time beside it.
pub fn claim(dir: &Path) {
    let _ = std::fs::write(dir.join("rules.pid"), std::process::id().to_string());
}

/// Whether a running `serve` (not this process) has claimed them.
pub(crate) fn served(dir: &Path) -> bool {
    let Some(pid) = std::fs::read_to_string(dir.join("rules.pid"))
        .ok()
        .and_then(|s| s.trim().parse::<i32>().ok())
    else {
        return false;
    };
    // SAFETY: kill with signal 0 only checks that the process exists.
    pid != std::process::id() as i32 && unsafe { libc::kill(pid, 0) } == 0
}

// ── the command line ────────────────────────────────────────────────────

/// A rule written by hand may name locations, devices, projects and tags
/// by name: they become ids.
pub(crate) fn resolve_names(a: &Agent, e: &Engine, r: &mut Rule) -> Result<(), String> {
    use drive_rules::{Action, Dest, Devices, Place, Test};
    let locs = space::locations(&a.space);
    let devs = space::devices(&a.space);
    let projs = projects::projects(&a.space);
    let lib = Library::load(&e.tags);
    let loc = |x: &mut String| -> Result<(), String> {
        if !locs.iter().any(|l| l.id == *x) {
            *x = locs
                .iter()
                .find(|l| l.name.eq_ignore_ascii_case(x))
                .map(|l| l.id.clone())
                .ok_or(format!("no location {x}"))?;
        }
        Ok(())
    };
    let dev = |x: &mut String| -> Result<(), String> {
        if !devs.iter().any(|d| d.id == *x) {
            *x = devs
                .iter()
                .find(|d| d.name.eq_ignore_ascii_case(x))
                .map(|d| d.id.clone())
                .ok_or(format!("no device {x}"))?;
        }
        Ok(())
    };
    let proj = |x: &mut String| -> Result<(), String> {
        if !x.is_empty() && !projs.iter().any(|p| p.id == *x) {
            *x = projs
                .iter()
                .find(|p| p.name.eq_ignore_ascii_case(x))
                .map(|p| p.id.clone())
                .ok_or(format!("no project {x}"))?;
        }
        Ok(())
    };
    let tag = |x: &mut String| -> Result<(), String> {
        if lib.get(x).is_none() {
            *x = match lib.resolve(x).as_slice() {
                [one] => one.clone(),
                [] => return Err(format!("no tag {x}")),
                _ => return Err(format!("{x} names several tags; give its id")),
            };
        }
        Ok(())
    };
    if let Devices::Named(v) = &mut r.scope.devices {
        for d in v {
            dev(d)?;
        }
    }
    for p in &mut r.scope.places {
        match p {
            Place::Location { location, .. } => loc(location)?,
            Place::Project { project } => proj(project)?,
            Place::Tag { tag: t } => tag(t)?,
            Place::Drive { .. } => {}
        }
    }
    for g in &mut r.when.groups {
        for c in &mut g.conditions {
            match &mut c.test {
                Test::Tag { tag: t } => tag(t)?,
                Test::Project { project } => proj(project)?,
                Test::HeldBy { device } => dev(device)?,
                _ => {}
            }
        }
    }
    for act in &mut r.actions {
        match act {
            Action::AddTags { tags } | Action::RemoveTags { tags } => {
                for t in tags {
                    tag(t)?;
                }
            }
            Action::AddToProject { project } => proj(project)?,
            Action::Copy {
                to: Dest::Location { location, .. },
                ..
            }
            | Action::Move {
                to: Dest::Location { location, .. },
                ..
            } => loc(location)?,
            _ => {}
        }
    }
    Ok(())
}

fn ago(ms: i64) -> String {
    if ms == 0 {
        return "never".into();
    }
    let s = ((drive_files::now_ms() - ms) / 1000).max(0);
    match s {
        0..60 => "just now".into(),
        60..3600 => format!("{} min ago", s / 60),
        3600..86400 => format!("{} h ago", s / 3600),
        _ => format!("{} days ago", s / 86400),
    }
}

fn find(e: &Engine, name: &str) -> Result<Rule, String> {
    doc::find(&e.rules, name).ok_or_else(|| format!("no rule {name}"))
}

fn print_row(r: &Row) {
    out(format!("  {}", r.item.path()));
    for s in &r.steps {
        let note = match (&s.blocked, s.noop) {
            (Some(b), _) => format!("  [held back: {b}]"),
            (None, true) => "  [already so]".into(),
            _ => String::new(),
        };
        out(format!("    → {}{note}", s.says));
    }
}

pub async fn command(dir: &Path, args: &[String]) -> Result<(), String> {
    let sub = args.get(1).map(String::as_str).unwrap_or("ls");
    let rest: Vec<String> = args.iter().skip(2).cloned().collect();
    let a = start(dir).await?;
    let e = Engine::open(&a).await;
    let who = {
        let c = a.cfg.lock().expect("cfg");
        c.signed_in_as
            .clone()
            .unwrap_or_else(|| c.device_name.clone())
    };
    let devices: HashMap<String, String> = space::devices(&a.space)
        .into_iter()
        .map(|d| (d.id, d.name))
        .chain([("node".to_string(), "the node".to_string())])
        .collect();
    let dev = |id: &str| devices.get(id).cloned().unwrap_or_else(|| id.to_string());
    match sub {
        "ls" => {
            let rules = doc::rules(&e.rules);
            if rules.is_empty() {
                out("no rules yet (drive rules add FILE.json, or the Drive app's Rules)");
            }
            for r in rules {
                out(format!(
                    "{}  {}  {}  {} action(s)  {}",
                    r.name,
                    if r.enabled { "on" } else { "off" },
                    r.mode.label(),
                    r.actions.len(),
                    r.id
                ));
                for run in journal::runs(&e.journal, &r.id) {
                    out(format!(
                        "    {}: {} {}, {} matched, {} done, {} waiting, {} failed, {} held back{}",
                        dev(&run.device),
                        if run.live { "ran" } else { "dry run" },
                        ago(run.at),
                        run.matched,
                        run.done,
                        run.waiting,
                        run.failed,
                        run.skipped,
                        if run.error.is_empty() { String::new() } else { format!(" ({})", run.error) }
                    ));
                }
            }
        }
        "show" => {
            let r = find(&e, rest.first().ok_or("rules show NAME")?)?;
            out(serde_json::to_string_pretty(&r).map_err(|e| e.to_string())?);
            if eval::feeds_itself(&r) {
                out("note: it copies or moves into a place it looks at; a rule never takes what it made");
            }
            for p in journal::previews(&e.journal, &r.id) {
                out(format!("{} ({}): {} file(s) match", dev(&p.device), ago(p.at), p.matched));
                for row in &p.rows {
                    print_row(row);
                }
            }
        }
        "run" => {
            let r = find(&e, rest.first().ok_or("rules run NAME [--live]")?)?;
            let live = rest.iter().any(|x| x == "--live");
            if live && !(r.enabled && r.mode == Mode::Live) {
                return Err(format!(
                    "{} is {}: a person switches it on and live first (rules on {0}; rules live {0}), after looking at its dry run",
                    r.name,
                    if r.enabled { "a dry run" } else { "off" }
                ));
            }
            // Every device in scope and the node too.
            doc::request_run(&e.rules, &r.id, live, &who, drive_files::now_ms()).map_err(|e| e.to_string())?;
            a.drive.commit(&e.rules_id);
            let me = e.me(&a);
            if eval::runs_on(&r, &me) && served(&a.dir) {
                // This device's `serve` runs it (one runner per device).
                let asked = drive_files::now_ms();
                let mut answered = None;
                for _ in 0..90 {
                    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                    if let Some(run) = journal::runs(&e.journal, &r.id)
                        .into_iter()
                        .find(|x| x.device == me.id && x.at >= asked - 1_000)
                    {
                        answered = Some(run);
                        break;
                    }
                }
                let Some(run) = answered else {
                    out("asked; this device's agent has not answered yet (see `rules show`)");
                    return Ok(());
                };
                out(format!(
                    "{} on {} (by its agent): {} file(s) match, {} done, {} waiting, {} failed, {} held back",
                    if run.live { "ran" } else { "dry run" },
                    me.name,
                    run.matched,
                    run.done,
                    run.waiting,
                    run.failed,
                    run.skipped
                ));
                if let Some(p) = journal::previews(&e.journal, &r.id).into_iter().find(|p| p.device == me.id) {
                    for row in &p.rows {
                        print_row(row);
                    }
                }
            } else if eval::runs_on(&r, &me) {
                let o = e.run(&a, &r, live, None).await;
                e.state.lock().await.handled.insert(r.id.clone(), drive_files::now_ms());
                e.save_state(&a).await;
                if !o.run.error.is_empty() {
                    return Err(o.run.error);
                }
                out(format!(
                    "{} on {}: {} file(s) match{}",
                    if live { "ran" } else { "dry run" },
                    me.name,
                    o.run.matched,
                    if live {
                        format!(", {} done, {} waiting, {} failed, {} held back", o.run.done, o.run.waiting, o.run.failed, o.run.skipped)
                    } else {
                        String::new()
                    }
                ));
                for row in &o.rows {
                    print_row(row);
                }
            } else {
                out(format!("{} does not run on this device; asked the devices it runs on", r.name));
            }
            settle(&a).await;
        }
        "journal" => {
            let only = match rest.first() {
                Some(n) => Some(find(&e, n)?.id),
                None => None,
            };
            for j in journal::entries(&e.journal)
                .into_iter()
                .filter(|j| only.as_ref().is_none_or(|r| *r == j.rule))
                .take(100)
            {
                out(format!(
                    "{}  {}  {}  {}  {}: {}{}{}",
                    j.id,
                    ago(j.at),
                    dev(&j.device),
                    j.rule_name,
                    j.status.unwrap_or(Status::Done).id(),
                    j.before.as_ref().map(|b| b.path().to_string()).unwrap_or_default(),
                    format_args!(" → {}", j.says),
                    if j.error.is_empty() { String::new() } else { format!(" ({})", j.error) }
                ));
            }
        }
        "undo" => {
            let id = rest.first().ok_or("rules undo ID")?;
            let j = journal::entry(&e.journal, id).ok_or(format!("no journal entry {id}"))?;
            if !j.undo.possible() && j.status != Some(Status::Waiting) {
                return Err("this action cannot be undone (it only made derived data)".into());
            }
            let owner = e.owner(&a, &j);
            if owner == a.device_id() && !served(&a.dir) {
                e.undo_here(&a, j).await?;
                e.commit(&a);
                e.save_state(&a).await;
                out("undone");
            } else {
                journal::request_undo(&e.journal, id, drive_files::now_ms()).map_err(|e| e.to_string())?;
                a.drive.commit(&e.journal_id);
                out(format!("asked {} to undo it (it does so when it is on)", dev(&owner)));
            }
            settle(&a).await;
        }
        "add" => {
            let file = rest.first().ok_or("rules add FILE.json")?;
            let text = std::fs::read_to_string(file).map_err(|e| format!("{file}: {e}"))?;
            let mut r: Rule = serde_json::from_str(&text).map_err(|e| format!("{file}: {e}"))?;
            r.id.clear();
            resolve_names(&a, &e, &mut r)?;
            Matcher::new(&r).map_err(|e| e.to_string())?;
            let id = doc::save(&e.rules, &r, &who, drive_files::now_ms()).map_err(|e| e.to_string())?;
            a.drive.commit(&e.rules_id);
            out(format!("{} ({id}): off, a dry run. Look at it with `rules run {}`, then `rules on` and `rules live`.", r.name, r.name));
            settle(&a).await;
        }
        "on" | "off" => {
            let r = find(&e, rest.first().ok_or("rules on|off NAME")?)?;
            doc::set_enabled(&e.rules, &r.id, sub == "on").map_err(|e| e.to_string())?;
            a.drive.commit(&e.rules_id);
            out(format!("{}: {sub}", r.name));
            settle(&a).await;
        }
        "live" | "dry-run" => {
            let r = find(&e, rest.first().ok_or("rules live|dry-run NAME")?)?;
            let mode = if sub == "live" { Mode::Live } else { Mode::DryRun };
            doc::set_mode(&e.rules, &r.id, mode).map_err(|e| e.to_string())?;
            a.drive.commit(&e.rules_id);
            out(format!("{}: {}", r.name, mode.label()));
            settle(&a).await;
        }
        "rm" => {
            let r = find(&e, rest.first().ok_or("rules rm NAME")?)?;
            doc::remove(&e.rules, &r.id).map_err(|e| e.to_string())?;
            a.drive.commit(&e.rules_id);
            out(format!("{} removed (its journal stays)", r.name));
            settle(&a).await;
        }
        _ => return Err("rules ls|show NAME|run NAME [--live]|journal [NAME]|undo ID|add FILE|on|off|live|dry-run|rm NAME".into()),
    }
    Ok(())
}

#[cfg(test)]
mod engine_tests {
    use drive_rules::{Cmp, Condition, Group, Mode, Rule, Test, When};

    use super::*;
    use crate::adopt::tests::{device, node};

    fn aged(path: &Path, bytes: &[u8]) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, bytes).unwrap();
        // Older than the settle time, so a rule considers it.
        let then = std::time::SystemTime::now() - std::time::Duration::from_secs(3600);
        std::fs::File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(then)
            .unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_rule_reads_the_index_database_page_by_page() {
        let (url, _node_dir) = node().await;
        let (dir, disk) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let a = device(&url, dir.path(), "studio", "Studio PC").await;
        let (songs, backup) = (disk.path().join("songs"), disk.path().join("backup"));
        aged(&songs.join("a.wav"), b"the same take");
        aged(&songs.join("b.wav"), b"only here");
        aged(
            &songs.join("Song.logicx/Alternatives/inside.wav"),
            b"the same take",
        );
        aged(&backup.join("old/a.wav"), b"the same take");
        let loc = a.add_location("Songs", &songs).unwrap();
        let other = a.add_location("Backup", &backup).unwrap();
        a.index_location(&loc).await.unwrap();
        a.index_location(&other).await.unwrap();

        let engine = Engine::open(&a).await;
        let rule = Rule {
            id: "r1".into(),
            name: "Copied elsewhere".into(),
            enabled: true,
            mode: Mode::DryRun,
            when: When {
                all: true,
                groups: vec![Group {
                    all: true,
                    conditions: vec![Condition::is(Test::Copies {
                        cmp: Cmp::More,
                        n: 1,
                    })],
                }],
            },
            ..Rule::default()
        };
        let out = engine.run(&a, &rule, false, Some(&loc.id)).await;
        assert_eq!(out.run.error, "");
        // a.wav (also in Backup, and inside the bundle) matches; b.wav has
        // one copy; the bundle's inside is never a candidate of its own.
        let paths: Vec<String> = out
            .rows
            .iter()
            .filter_map(|r| match &r.item {
                Item::Location { path, .. } => Some(path.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(paths, ["a.wav"], "{:?}", out.rows);
    }
}

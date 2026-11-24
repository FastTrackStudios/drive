//! Rules in the browser (docs/rules.md): the list (on/off, dry run or
//! live), a rule's page (what it would do on each device, its recent
//! actions with Undo) and the editor (scope, trigger, conditions,
//! actions), a sheet on a phone. The model and the evaluator are
//! `drive_rules`: the Drive tree's dry run is worked out here, in the
//! browser, with the same code the node runs; each device's comes from
//! its agent through the journal.

use dioxus::prelude::*;
use drive_dioxus::{use_doc, use_drive};
use drive_files::{files_doc, projects, space, space_doc, tree};
use drive_rules::eval::{self, Facts, Item, Matcher};
use drive_rules::journal::{self, Row, Status};
use drive_rules::{
    Action, Cmp, Condition, DateOf, Dest, DeviceInfo, Devices, Group, LocInfo, Mode,
    Place as RulePlace, Range, Rule, Runner, Test, Text, TextOp, Unit, World, doc,
};
use drive_spine::kinds::Kinds;
use drive_spine::tags::Library;

use crate::browser::Place;
use crate::nav::use_nav;

pub const STYLE: Asset = asset!("/assets/rules.css");

fn now() -> i64 {
    crate::host::now_ms() as i64
}

fn me() -> String {
    let ctx = use_drive();
    if ctx.who.is_empty() {
        ctx.me.peek().name.clone()
    } else {
        ctx.who.clone()
    }
}

fn ago(ms: i64) -> String {
    if ms == 0 {
        return "never".into();
    }
    let s = ((now() - ms) / 1000).max(0);
    match s {
        0..60 => "just now".into(),
        60..3600 => format!("{} min ago", s / 60),
        3600..86400 => format!("{} h ago", s / 3600),
        _ => format!("{} days ago", s / 86400),
    }
}

/// Names of devices by id (the node is "the Drive").
fn device_names(reg: &loro::LoroDoc) -> Vec<(String, String)> {
    let mut v: Vec<(String, String)> = space::devices(reg)
        .into_iter()
        .map(|d| (d.id, d.name))
        .collect();
    v.push(("node".into(), "Drive (node)".into()));
    v
}

fn name_of(names: &[(String, String)], id: &str) -> String {
    names
        .iter()
        .find(|(i, _)| i == id)
        .map_or_else(|| id.to_string(), |(_, n)| n.clone())
}

// ── the list ────────────────────────────────────────────────────────────

#[component]
pub fn RulesPage(space: String) -> Element {
    let rules_doc = use_doc(doc::rules_doc(&space));
    let journal = use_doc(journal::journal_doc(&space));
    let reg = use_doc(space_doc(&space));
    let nav = use_nav();
    let by = me();
    let rules = doc::rules(&rules_doc.loro());
    let names = device_names(&reg.loro());
    let jd = journal.loro();
    let new_rule = {
        let rules_doc = rules_doc.clone();
        move |_| {
            let r = Rule {
                name: "New rule".into(),
                ..Rule::default()
            };
            let id = rules_doc.edit(|d| doc::save(d, &r, &by, now()).ok());
            if let Some(id) = id {
                nav.go(Place::Rule(id));
            }
        }
    };
    rsx! {
        section { class: "pane wide rules",
            div { class: "panehead",
                h1 { "Rules" }
                span { class: "count", "{rules.len()}" }
                button { class: "primary", onclick: new_rule, "New rule" }
            }
            p { class: "hint",
                "A rule takes the files it matches, on the devices and in the places it names, and does its actions to them. Every rule starts off and as a dry run: it says what it would do, and changes nothing until you make it live."
            }
            if rules.is_empty() {
                p { class: "empty", "No rules yet." }
            }
            div { class: "rulelist",
                for r in rules {
                    {
                        let runs = journal::runs(&jd, &r.id);
                        let matched: usize = runs.iter().map(|x| x.matched).sum();
                        let last = runs.iter().map(|x| x.at).max().unwrap_or(0);
                        let summary = if runs.is_empty() {
                            "not run yet".to_string()
                        } else {
                            format!(
                                "{matched} file(s) match on {} · {}",
                                runs.iter().map(|x| name_of(&names, &x.device)).collect::<Vec<_>>().join(", "),
                                ago(last)
                            )
                        };
                        let id = r.id.clone();
                        rsx! {
                            div { key: "{r.id}", class: "rulerow",
                                button { class: "rulename", onclick: move |_| nav.go(Place::Rule(id.clone())),
                                    span { class: "title", "{r.name}" }
                                    span { class: "sub", "{r.actions.len()} action(s) · {summary}" }
                                }
                                Switches { space: space.clone(), rule: r.clone() }
                            }
                        }
                    }
                }
            }
        }
    }
}

/// A rule's on/off switch and its dry run / live choice. Going live asks
/// first.
#[component]
fn Switches(space: String, rule: Rule) -> Element {
    let rules_doc = use_doc(doc::rules_doc(&space));
    let mut confirming = use_signal(|| false);
    let id = rule.id.clone();
    let on = rule.enabled;
    let live = rule.mode == Mode::Live;
    let toggle = {
        let (rd, id) = (rules_doc.clone(), id.clone());
        move |_| {
            rd.edit(|d| doc::set_enabled(d, &id, !on).ok());
        }
    };
    let dry = {
        let (rd, id) = (rules_doc.clone(), id.clone());
        move |_| {
            confirming.set(false);
            rd.edit(|d| doc::set_mode(d, &id, Mode::DryRun).ok());
        }
    };
    let go_live = {
        let (rd, id) = (rules_doc.clone(), id.clone());
        move |_| {
            confirming.set(false);
            rd.edit(|d| doc::set_mode(d, &id, Mode::Live).ok());
        }
    };
    rsx! {
        div { class: "switches",
            button {
                class: if on { "toggle on" } else { "toggle" },
                title: if on { "On: runs on its triggers" } else { "Off: does not run" },
                onclick: toggle,
                span { class: "knob" }
                span { class: "lbl", if on { "On" } else { "Off" } }
            }
            div { class: "segmented",
                button { class: if !live { "mode-button on" } else { "mode-button" }, onclick: dry, "Dry run" }
                button {
                    class: if live { "mode-button on golive" } else { "mode-button" },
                    onclick: move |_| if !live { confirming.set(true) },
                    "Live"
                }
            }
            if confirming() {
                div { class: "confirm",
                    span { "Make it live? It will change files where it runs." }
                    button { class: "primary small", onclick: go_live, "Make live" }
                    button { class: "primary ghost small", onclick: move |_| confirming.set(false), "Cancel" }
                }
            }
        }
    }
}

// ── one rule ────────────────────────────────────────────────────────────

#[component]
pub fn RulePage(space: String, id: String) -> Element {
    let rules_doc = use_doc(doc::rules_doc(&space));
    let journal = use_doc(journal::journal_doc(&space));
    let reg = use_doc(space_doc(&space));
    let nav = use_nav();
    let by = me();
    let mut editing = use_signal(|| false);
    let mut draft = use_signal(Rule::default);
    let mut removing = use_signal(|| false);
    let phone = app_shell::ui::use_shell().form() == app_shell::ui::FormFactor::Phone;
    let mut auto_opened = use_signal(|| false);
    {
        let (rd, id) = (rules_doc.clone(), id.clone());
        use_effect(move || {
            if *auto_opened.peek() {
                return;
            }
            if let Some(r) = doc::rule(&rd.loro(), &id) {
                auto_opened.set(true);
                if r.actions.is_empty() && r.when.groups.is_empty() && r.name == "New rule" {
                    draft.set(r);
                    editing.set(true);
                }
            }
        });
    }
    let Some(rule) = doc::rule(&rules_doc.loro(), &id) else {
        return rsx! {
            section { class: "pane", p { class: "empty", "This rule is gone (or still loading)." } }
        };
    };
    let names = device_names(&reg.loro());
    let problem = Matcher::new(&rule).err().map(|e| e.to_string());
    let feeds = eval::feeds_itself(&rule);
    let ask = {
        let (rd, id, by) = (rules_doc.clone(), id.clone(), by.clone());
        move |live: bool| {
            rd.edit(|d| doc::request_run(d, &id, live, &by, now()).ok());
        }
    };
    let ask2 = ask.clone();
    let start_edit = {
        let rule = rule.clone();
        move |_| {
            draft.set(rule.clone());
            editing.set(true);
        }
    };
    let save = {
        let (rd, by) = (rules_doc.clone(), by.clone());
        move |_| {
            let r = draft();
            rd.edit(|d| doc::save(d, &r, &by, now()).ok());
            editing.set(false);
        }
    };
    let remove = {
        let (rd, id) = (rules_doc.clone(), id.clone());
        move |_| {
            rd.edit(|d| doc::remove(d, &id).ok());
            nav.go(Place::Rules);
        }
    };
    let live = rule.enabled && rule.mode == Mode::Live;
    let editor = rsx! {
        RuleEditor { space: space.clone(), draft }
        div { class: "editorfoot",
            button { class: "primary", onclick: save, "Save" }
            button { class: "primary ghost", onclick: move |_| editing.set(false), "Cancel" }
        }
    };
    rsx! {
        section { class: "pane wide rules",
            div { class: "panehead rulehead",
                button { class: "back", onclick: move |_| nav.go(Place::Rules), "‹ Rules" }
                h1 { "{rule.name}" }
            }
            div { class: "rulebar",
                Switches { space: space.clone(), rule: rule.clone() }
                div { class: "actions",
                    button { class: "primary ghost", onclick: move |_| ask(false), "Dry run now" }
                    if live {
                        button { class: "primary", onclick: move |_| ask2(true), "Run now" }
                    }
                    if !editing() {
                        button { class: "primary ghost", onclick: start_edit, "Edit" }
                    }
                    button { class: "primary ghost ruledanger", onclick: move |_| removing.set(true), "Remove" }
                }
            }
            if removing() {
                div { class: "confirm",
                    span { "Remove this rule? What it already did stays in the journal." }
                    button { class: "primary small", onclick: remove, "Remove" }
                    button { class: "primary ghost small", onclick: move |_| removing.set(false), "Cancel" }
                }
            }
            if let Some(p) = problem {
                p { class: "searcherr", "It cannot run as written: {p}" }
            }
            if feeds {
                p { class: "hint warn", "It copies or moves into a place it looks at. A rule never takes what it made, so it cannot loop, but check the dry run." }
            }
            RuleSummary { space: space.clone(), rule: rule.clone() }
            if editing() && !phone {
                div { class: "reditor", {editor} }
            }
            if phone {
                app_shell::ui::Sheet {
                    open: editing(),
                    title: "Edit rule",
                    detent: app_shell::gestures::Detent::Large,
                    detents: vec![app_shell::gestures::Detent::Large],
                    render: {
                        let (space, rd, by) = (space.clone(), rules_doc.clone(), by.clone());
                        Callback::new(move |()| {
                            let (rd, by) = (rd.clone(), by.clone());
                            rsx! {
                                div { class: "reditor insheet",
                                    RuleEditor { space: space.clone(), draft }
                                    div { class: "editorfoot",
                                        button {
                                            class: "primary",
                                            onclick: move |_| {
                                                let r = draft();
                                                rd.edit(|d| doc::save(d, &r, &by, now()).ok());
                                                editing.set(false);
                                            },
                                            "Save"
                                        }
                                        button { class: "primary ghost", onclick: move |_| editing.set(false), "Cancel" }
                                    }
                                }
                            }
                        })
                    },
                    on_close: move |()| editing.set(false),
                }
            }
            h2 { class: "rulesec", "What it would do" }
            Previews { space: space.clone(), rule: rule.clone(), names: names.clone() }
            h2 { class: "rulesec", "What it did" }
            Journal { rule: rule.id.clone(), names, journal: journal.clone() }
        }
    }
}

/// The rule in words.
#[component]
fn RuleSummary(space: String, rule: Rule) -> Element {
    let reg = use_doc(space_doc(&space));
    let tags = use_doc(drive_spine::tags::tags_doc(&space));
    let ctx = Ctx::load(&reg.loro(), &tags.loro());
    let devices = match &rule.scope.devices {
        Devices::All => "every device".to_string(),
        Devices::Roles(r) => format!("devices that are {}", r.join(" or ")),
        Devices::Named(ids) => ids
            .iter()
            .map(|i| ctx.device(i))
            .collect::<Vec<_>>()
            .join(", "),
    };
    let places = if rule.scope.places.is_empty() {
        "everything".to_string()
    } else {
        rule.scope
            .places
            .iter()
            .map(|p| ctx.place(p))
            .collect::<Vec<_>>()
            .join("; ")
    };
    let trigger = {
        let mut t = Vec::new();
        if rule.trigger.on_change {
            t.push("when files change".to_string());
        }
        if rule.trigger.every_minutes > 0 {
            t.push(format!("every {} min", rule.trigger.every_minutes));
        }
        t.push("on Run now".into());
        t.join(", ")
    };
    let conds: Vec<String> = rule
        .when
        .groups
        .iter()
        .map(|g| {
            g.conditions
                .iter()
                .map(|c| ctx.condition(c))
                .collect::<Vec<_>>()
                .join(if g.all { " and " } else { " or " })
        })
        .filter(|s| !s.is_empty())
        .collect();
    let when = if conds.is_empty() {
        "every file in scope".to_string()
    } else if conds.len() == 1 {
        conds[0].clone()
    } else {
        conds
            .iter()
            .map(|c| format!("({c})"))
            .collect::<Vec<_>>()
            .join(if rule.when.all { " and " } else { " or " })
    };
    rsx! {
        div { class: "rulesummary",
            div { span { class: "k", "Runs on" } span { "{devices}" } }
            div { span { class: "k", "Looks at" } span { "{places}" } }
            div { span { class: "k", "When" } span { "{trigger}" } }
            div { span { class: "k", "Takes" } span { "{when}" } }
            div { span { class: "k", "Does" }
                ol {
                    for (i , a) in rule.actions.iter().enumerate() {
                        li { key: "{i}", "{ctx.action(a)}" }
                    }
                    if rule.actions.is_empty() {
                        li { class: "muted", "nothing yet" }
                    }
                }
            }
        }
    }
}

/// Each place's dry run: the Drive tree's worked out here, each device's
/// from its agent.
#[component]
fn Previews(space: String, rule: Rule, names: Vec<(String, String)>) -> Element {
    let journal = use_doc(journal::journal_doc(&space));
    let files = use_doc(files_doc(&space));
    let reg = use_doc(space_doc(&space));
    let tags = use_doc(drive_spine::tags::tags_doc(&space));
    let spine = use_doc(drive_spine::spine_doc(&space));
    let previews = journal::previews(&journal.loro(), &rule.id);
    let drive_rows: Option<(usize, Vec<Row>)> = eval::covers_drive(&rule).then(|| {
        drive_preview(
            &rule,
            &files.loro(),
            &reg.loro(),
            &tags.loro(),
            &spine.loro(),
        )
    });
    let device_previews: Vec<_> = previews
        .into_iter()
        .filter(|p| p.device != "node")
        .collect();
    rsx! {
        div { class: "rprevs",
            if let Some((n, rows)) = drive_rows {
                PreviewBlock { title: "The Drive (worked out here)", when: "now".to_string(), matched: n, rows }
            }
            for p in device_previews.iter() {
                PreviewBlock {
                    key: "{p.device}",
                    title: name_of(&names, &p.device),
                    when: ago(p.at),
                    matched: p.matched,
                    rows: p.rows.clone(),
                }
            }
            if device_previews.is_empty() {
                p { class: "hint",
                    "No device has run it yet. “Dry run now” asks every device it runs on (each answers when it is on)."
                }
            }
        }
    }
}

#[component]
fn PreviewBlock(title: String, when: String, matched: usize, rows: Vec<Row>) -> Element {
    let mut all = use_signal(|| false);
    let shown = if all() {
        rows.len()
    } else {
        rows.len().min(20)
    };
    let more = rows.len() - shown;
    rsx! {
        div { class: "preview",
            div { class: "prevhead",
                span { class: "title", "{title}" }
                span { class: "chip", "{matched} match" }
                span { class: "when", "{when}" }
            }
            if rows.is_empty() {
                p { class: "empty", "Nothing matches." }
            }
            for (i , r) in rows.iter().take(shown).enumerate() {
                div { key: "{i}", class: "prevrow",
                    div { class: "path", "{r.item.path()}" }
                    for (j , s) in r.steps.iter().enumerate() {
                        div {
                            key: "{j}",
                            class: if s.blocked.is_some() { "step held" } else if s.noop { "step noop" } else { "step" },
                            span { class: "arrow", "→" }
                            span { "{s.says}" }
                            if let Some(b) = &s.blocked {
                                span { class: "why", "held back: {b}" }
                            } else if s.noop {
                                span { class: "why", "already so" }
                            }
                        }
                    }
                }
            }
            if more > 0 {
                button { class: "primary ghost small", onclick: move |_| all.set(true), "Show {more} more" }
            }
        }
    }
}

/// The Drive tree's dry run, with the evaluator the node uses.
fn drive_preview(
    rule: &Rule,
    files: &loro::LoroDoc,
    reg: &loro::LoroDoc,
    tags: &loro::LoroDoc,
    spine: &loro::LoroDoc,
) -> (usize, Vec<Row>) {
    let Ok(m) = Matcher::new(rule) else {
        return (0, Vec::new());
    };
    let kinds = Kinds::load(spine);
    let lib = Library::load(tags);
    let ctx = Ctx::load(reg, tags);
    let projs = projects::projects(reg);
    let w = World {
        now: now(),
        kinds: &kinds,
        tags: &lib,
        devices: &ctx.devices,
        locations: &ctx.locations,
        projects: &projs,
        runner: Runner::Node,
    };
    let mut n = 0;
    let mut rows = Vec::new();
    for (path, e) in tree::files(files) {
        if e.dir || tree::in_trash(files, e.id) {
            continue;
        }
        let holders: Vec<String> = space::holders(reg, &e.hash)
            .into_iter()
            .map(|(w, _)| w)
            .collect();
        let f = Facts {
            item: Some(Item::Drive {
                id: tree::id_string(e.id),
                path,
            }),
            size: e.size,
            modified: e.mtime,
            content: e.hash.clone(),
            copies: holders.len().max(1) as u32,
            holders,
            ..Facts::default()
        };
        if m.matches(&f, &w) {
            n += 1;
            if rows.len() < journal::PREVIEW_ROWS {
                rows.push(Row::of(&m.plan(&f, &w)));
            }
        }
    }
    (n, rows)
}

/// What the rule did, with Undo.
#[component]
fn Journal(rule: String, names: Vec<(String, String)>, journal: drive_dioxus::Doc) -> Element {
    let entries: Vec<_> = journal::entries(&journal.loro())
        .into_iter()
        .filter(|e| e.rule == rule)
        .take(100)
        .collect();
    let runs = journal::runs(&journal.loro(), &rule);
    rsx! {
        div { class: "runs",
            for r in runs.iter() {
                div { key: "{r.device}", class: "runline",
                    span { class: "title", "{name_of(&names, &r.device)}" }
                    span { class: if r.live { "chip warn" } else { "chip" }, if r.live { "ran live" } else { "dry run" } }
                    span { class: "when", "{ago(r.at)}" }
                    span { "{r.matched} matched · {r.done} done · {r.waiting} waiting · {r.failed} failed · {r.skipped} held back" }
                    if !r.error.is_empty() {
                        span { class: "why", "{r.error}" }
                    }
                }
            }
        }
        if entries.is_empty() {
            p { class: "empty", "It has not changed anything yet." }
        }
        div { class: "journal",
            for e in entries {
                {
                    let status = e.status.unwrap_or(Status::Done);
                    let undoable = (e.undo.possible() && matches!(status, Status::Done | Status::UndoRefused)) || status == Status::Waiting;
                    let asked = e.undo_requested > 0;
                    let id = e.id.clone();
                    let j = journal.clone();
                    let waiting_for = (status == Status::Waiting && !e.for_device.is_empty())
                        .then(|| name_of(&names, &e.for_device));
                    rsx! {
                        div { key: "{e.id}", class: "jrow",
                            div { class: "jmain",
                                span { class: "chip {status.id()}", "{status.id()}" }
                                span { class: "path", {e.before.as_ref().map(|b| b.path().to_string()).unwrap_or_default()} }
                                span { "→ {e.says}" }
                            }
                            div { class: "jmeta",
                                span { "{name_of(&names, &e.device)} · {ago(e.at)}" }
                                if let Some(d) = waiting_for {
                                    span { class: "why", "waiting for {d} (it fetches the copy when it is on)" }
                                }
                                if !e.error.is_empty() {
                                    span { class: "why", "{e.error}" }
                                }
                                if asked {
                                    span { class: "chip", "undo asked" }
                                } else if undoable {
                                    button {
                                        class: "primary ghost small",
                                        onclick: move |_| {
                                            j.edit(|d| journal::request_undo(d, &id, now()).ok());
                                        },
                                        "Undo"
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
        p { class: "hint",
            "Undo is done by the device that holds the file, when it is on. It refuses when the file changed since, or when the rule is live and would do it again at once."
        }
    }
}

// ── words for places, conditions and actions ────────────────────────────

struct Ctx {
    devices: Vec<DeviceInfo>,
    locations: Vec<LocInfo>,
    projects: Vec<(String, String)>,
    tags: Vec<(String, String)>,
}

impl Ctx {
    fn load(reg: &loro::LoroDoc, tags: &loro::LoroDoc) -> Ctx {
        let lib = Library::load(tags);
        Ctx {
            devices: space::devices(reg)
                .into_iter()
                .map(|d| DeviceInfo {
                    id: d.id,
                    name: d.name,
                    roles: d.roles,
                })
                .collect(),
            locations: space::locations(reg)
                .into_iter()
                .map(|l| LocInfo {
                    id: l.id,
                    name: l.name,
                    device: l.device,
                    in_storage: !l.storage.is_empty(),
                })
                .collect(),
            projects: projects::projects(reg)
                .into_iter()
                .map(|p| (p.id, p.name))
                .collect(),
            tags: lib
                .all()
                .into_iter()
                .map(|t| (t.id.clone(), lib.label(&t.id)))
                .collect(),
        }
    }

    fn device(&self, id: &str) -> String {
        self.devices
            .iter()
            .find(|d| d.id == id)
            .map_or_else(|| id.to_string(), |d| d.name.clone())
    }

    fn location(&self, id: &str) -> String {
        self.locations.iter().find(|l| l.id == id).map_or_else(
            || id.to_string(),
            |l| format!("{} on {}", l.name, self.device(&l.device)),
        )
    }

    fn project(&self, id: &str) -> String {
        self.projects
            .iter()
            .find(|(i, _)| i == id)
            .map_or_else(|| id.to_string(), |(_, n)| n.clone())
    }

    fn tag(&self, id: &str) -> String {
        self.tags
            .iter()
            .find(|(i, _)| i == id)
            .map_or_else(|| id.to_string(), |(_, n)| n.clone())
    }

    fn place(&self, p: &RulePlace) -> String {
        match p {
            RulePlace::Location { location, path } if path.is_empty() => self.location(location),
            RulePlace::Location { location, path } => {
                format!("{}: {path}", self.location(location))
            }
            RulePlace::Drive { path } if path.is_empty() => "the whole Drive".into(),
            RulePlace::Drive { path } => format!("the Drive: {path}"),
            RulePlace::Project { project } => format!("project {}", self.project(project)),
            RulePlace::Tag { tag } => format!("tagged {}", self.tag(tag)),
        }
    }

    fn dest(&self, d: &Dest) -> String {
        match d {
            Dest::Location { location, path } => format!("{} / {path}", self.location(location)),
            Dest::Drive { path } => format!("the Drive / {path}"),
        }
    }

    fn condition(&self, c: &Condition) -> String {
        let t = |t: &Text| format!("{} “{}”", t.op.label(), t.value);
        let s = match &c.test {
            Test::Name { text } => format!("name {}", t(text)),
            Test::Path { text } => format!("path {}", t(text)),
            Test::Extension { any } => format!("extension is {}", any.join(", ")),
            Test::Kind { kind, family } => {
                format!(
                    "kind is {kind}{}",
                    if *family { " (or under it)" } else { "" }
                )
            }
            Test::Size { cmp, bytes } => format!("size {} {}", cmp.label(), human(*bytes)),
            Test::Date { of, range } => format!("{} {}", of.id(), range_words(range)),
            Test::Tag { tag } => format!("tagged {}", self.tag(tag)),
            Test::Field { name, text } => format!("{name} {}", t(text)),
            Test::Project { project } if project.is_empty() => "in a project".into(),
            Test::Project { project } => format!("in project {}", self.project(project)),
            Test::Copies { cmp, n } => format!("copies {} {n}", cmp.label()),
            Test::HeldBy { device } => format!("held by {}", self.device(device)),
        };
        if c.not { format!("not ({s})") } else { s }
    }

    fn action(&self, a: &Action) -> String {
        match a {
            Action::AddTags { tags } => {
                format!(
                    "tag {}",
                    tags.iter()
                        .map(|t| self.tag(t))
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            }
            Action::RemoveTags { tags } => {
                format!(
                    "untag {}",
                    tags.iter()
                        .map(|t| self.tag(t))
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            }
            Action::SetField { name, value } => format!("set {name} to “{value}”"),
            Action::AddToProject { project } => format!("add to project {}", self.project(project)),
            Action::Copy { to, name } => format!(
                "copy to {}{}",
                self.dest(to),
                if name.is_empty() {
                    String::new()
                } else {
                    format!(" as “{name}”")
                }
            ),
            Action::Move { to, name } => format!(
                "move to {}{}",
                self.dest(to),
                if name.is_empty() {
                    String::new()
                } else {
                    format!(" as “{name}”")
                }
            ),
            Action::Rename { name } => format!("rename to “{name}”"),
            Action::Sidecars {
                thumbnail,
                waveform,
            } => format!(
                "make {}",
                match (thumbnail, waveform) {
                    (true, true) => "a thumbnail and a waveform",
                    (true, false) => "a thumbnail",
                    (false, true) => "a waveform",
                    _ => "nothing",
                }
            ),
            Action::HighlyAvailable => "keep it highly available".into(),
            Action::Trash => "move it to the Drive's Trash".into(),
        }
    }
}

fn human(n: u64) -> String {
    match n {
        0..1024 => format!("{n} B"),
        1024..1_048_576 => format!("{:.0} KB", n as f64 / 1024.0),
        1_048_576..1_073_741_824 => format!("{:.0} MB", n as f64 / 1_048_576.0),
        _ => format!("{:.1} GB", n as f64 / 1_073_741_824.0),
    }
}

fn range_words(r: &Range) -> String {
    match r {
        Range::InLast { n, unit } => format!("in the last {n} {}", unit.id()),
        Range::OlderThan { n, unit } => format!("more than {n} {} ago", unit.id()),
        Range::Before { day } => format!("before {day}"),
        Range::After { day } => format!("on or after {day}"),
    }
}

// ── the editor ──────────────────────────────────────────────────────────

const TESTS: [(&str, &str); 11] = [
    ("name", "Name"),
    ("path", "Path"),
    ("extension", "Extension"),
    ("kind", "Kind"),
    ("size", "Size"),
    ("date", "Date"),
    ("tag", "Tag"),
    ("field", "Field"),
    ("project", "Project"),
    ("copies", "Copies"),
    ("held-by", "Held by"),
];

fn test_id(t: &Test) -> &'static str {
    match t {
        Test::Name { .. } => "name",
        Test::Path { .. } => "path",
        Test::Extension { .. } => "extension",
        Test::Kind { .. } => "kind",
        Test::Size { .. } => "size",
        Test::Date { .. } => "date",
        Test::Tag { .. } => "tag",
        Test::Field { .. } => "field",
        Test::Project { .. } => "project",
        Test::Copies { .. } => "copies",
        Test::HeldBy { .. } => "held-by",
    }
}

fn new_test(id: &str) -> Test {
    let text = || Text {
        op: TextOp::Contains,
        value: String::new(),
    };
    match id {
        "path" => Test::Path { text: text() },
        "extension" => Test::Extension { any: vec![] },
        "kind" => Test::Kind {
            kind: "audio".into(),
            family: true,
        },
        "size" => Test::Size {
            cmp: Cmp::More,
            bytes: 100 * 1_048_576,
        },
        "date" => Test::Date {
            of: DateOf::Modified,
            range: Range::OlderThan {
                n: 30,
                unit: Unit::Days,
            },
        },
        "tag" => Test::Tag { tag: String::new() },
        "field" => Test::Field {
            name: String::new(),
            text: text(),
        },
        "project" => Test::Project {
            project: String::new(),
        },
        "copies" => Test::Copies {
            cmp: Cmp::Less,
            n: 2,
        },
        "held-by" => Test::HeldBy {
            device: String::new(),
        },
        _ => Test::Name { text: text() },
    }
}

const ACTIONS: [(&str, &str); 10] = [
    ("tag", "Add tags"),
    ("untag", "Remove tags"),
    ("field", "Set a field"),
    ("project", "Add to project"),
    ("copy", "Copy to"),
    ("move", "Move to"),
    ("rename", "Rename"),
    ("sidecars", "Make sidecars"),
    ("available", "Keep highly available"),
    ("trash", "Drive Trash (Drive files only)"),
];

fn new_action(id: &str) -> Action {
    let dest = || Dest::Drive {
        path: "Sorted/{year}".into(),
    };
    match id {
        "untag" => Action::RemoveTags { tags: vec![] },
        "field" => Action::SetField {
            name: "Source".into(),
            value: "{device}".into(),
        },
        "project" => Action::AddToProject {
            project: String::new(),
        },
        "copy" => Action::Copy {
            to: dest(),
            name: String::new(),
        },
        "move" => Action::Move {
            to: dest(),
            name: String::new(),
        },
        "rename" => Action::Rename {
            name: "{year}-{month}-{day} {name}.{ext}".into(),
        },
        "sidecars" => Action::Sidecars {
            thumbnail: true,
            waveform: true,
        },
        "available" => Action::HighlyAvailable,
        "trash" => Action::Trash,
        _ => Action::AddTags { tags: vec![] },
    }
}

/// The lists the editor's pickers offer.
#[derive(Clone, PartialEq)]
struct Lists {
    devices: Vec<(String, String)>,
    locations: Vec<(String, String)>,
    projects: Vec<(String, String)>,
    tags: Vec<(String, String)>,
    kinds: Vec<(String, String)>,
}

#[component]
fn RuleEditor(space: String, draft: Signal<Rule>) -> Element {
    let reg = use_doc(space_doc(&space));
    let tags = use_doc(drive_spine::tags::tags_doc(&space));
    let spine = use_doc(drive_spine::spine_doc(&space));
    let ctx = Ctx::load(&reg.loro(), &tags.loro());
    let lists = Lists {
        devices: ctx
            .devices
            .iter()
            .map(|d| (d.id.clone(), d.name.clone()))
            .collect(),
        locations: ctx
            .locations
            .iter()
            .filter(|l| !l.in_storage)
            .map(|l| (l.id.clone(), ctx.location(&l.id)))
            .collect(),
        projects: ctx.projects.clone(),
        tags: ctx.tags.clone(),
        kinds: Kinds::load(&spine.loro())
            .all()
            .iter()
            .filter(|k| k.doc_prefix.is_empty())
            .map(|k| (k.id.clone(), k.name.clone()))
            .collect(),
    };
    let r = draft();
    let problem = Matcher::new(&r).err().map(|e| e.to_string());
    rsx! {
        div { class: "redit",
            label { class: "field",
                span { "Name" }
                input { value: "{r.name}", oninput: move |e| draft.write().name = e.value() }
            }
            fieldset {
                legend { "Where" }
                ScopeEditor { draft, lists: lists.clone() }
            }
            fieldset {
                legend { "When" }
                div { class: "row",
                    label { class: "check",
                        input {
                            r#type: "checkbox",
                            checked: r.trigger.on_change,
                            onchange: move |e| draft.write().trigger.on_change = e.checked(),
                        }
                        "When files change"
                    }
                    label { class: "check",
                        "Every"
                        input {
                            class: "num",
                            r#type: "number",
                            min: "0",
                            value: "{r.trigger.every_minutes}",
                            oninput: move |e| draft.write().trigger.every_minutes = e.value().parse().unwrap_or(0),
                        }
                        "minutes (0: no schedule)"
                    }
                }
                p { class: "hint", "Run now works whatever these say." }
            }
            fieldset {
                legend { "Which files" }
                div { class: "row",
                    "Match"
                    select {
                        onchange: move |e| draft.write().when.all = e.value() == "all",
                        option { value: "all", selected: r.when.all, "all" }
                        option { value: "any", selected: !r.when.all, "any" }
                    }
                    "of these groups"
                }
                for (gi , g) in r.when.groups.iter().enumerate() {
                    div { key: "{gi}", class: "rgroup",
                        div { class: "row",
                            select {
                                onchange: move |e| {
                                    if let Some(group) = draft.write().when.groups.get_mut(gi) {
                                        group.all = e.value() == "all";
                                    }
                                },
                                option { value: "all", selected: g.all, "All of" }
                                option { value: "any", selected: !g.all, "Any of" }
                            }
                            button {
                                class: "x",
                                title: "Remove the group",
                                onclick: move |_| {
                                    let mut rule = draft.write();
                                    if gi < rule.when.groups.len() {
                                        rule.when.groups.remove(gi);
                                    }
                                },
                                "×"
                            }
                        }
                        for (ci , c) in g.conditions.iter().enumerate() {
                            ConditionRow { key: "{gi}-{ci}", draft, gi, ci, cond: c.clone(), lists: lists.clone() }
                        }
                        button {
                            class: "primary ghost small",
                            onclick: move |_| {
                                if let Some(group) = draft.write().when.groups.get_mut(gi) {
                                    group.conditions.push(Condition::is(new_test("name")));
                                }
                            },
                            "+ Condition"
                        }
                    }
                }
                button {
                    class: "primary ghost small",
                    onclick: move |_| {
                        draft
                            .write()
                            .when
                            .groups
                            .push(Group {
                                all: true,
                                conditions: vec![Condition::is(new_test("name"))],
                            })
                    },
                    "+ Group"
                }
                if r.when.groups.is_empty() {
                    p { class: "hint", "No conditions: every file in its places matches." }
                }
            }
            fieldset {
                legend { "Do" }
                for (ai , a) in r.actions.iter().enumerate() {
                    ActionRow { key: "{ai}-{a.kind()}", draft, ai, action: a.clone(), lists: lists.clone() }
                }
                crate::shell::AddPick {
                    class: "add",
                    prompt: "+ Action…",
                    on_pick: move |v: String| draft.write().actions.push(new_action(&v)),
                    for (id , label) in ACTIONS {
                        option { key: "{id}", value: "{id}", "{label}" }
                    }
                }
                p { class: "hint",
                    "Names and folders may use {{name}} {{ext}} {{year}} {{month}} {{day}} {{kind}} {{project}} {{device}}. Copies never overwrite (a taken name gets “ (2)”). Moves and renames of a computer's files only happen on devices that allow them, as a checked copy with the original kept in .drive-trash."
                }
            }
            if let Some(p) = problem {
                p { class: "searcherr", "{p}" }
            }
        }
    }
}

fn select_opts(list: &[(String, String)], cur: &str, none: &str) -> Element {
    rsx! {
        option { value: "", selected: cur.is_empty(), "{none}" }
        for (id , name) in list.iter() {
            option { key: "{id}", value: "{id}", selected: cur == id, "{name}" }
        }
    }
}

#[component]
fn ScopeEditor(draft: Signal<Rule>, lists: Lists) -> Element {
    let r = draft();
    let mode = match &r.scope.devices {
        Devices::All => "all",
        Devices::Roles(_) => "roles",
        Devices::Named(_) => "named",
    };
    let picked: Vec<String> = match &r.scope.devices {
        Devices::Roles(v) | Devices::Named(v) => v.clone(),
        Devices::All => vec![],
    };
    rsx! {
        div { class: "row",
            "Devices"
            select {
                onchange: move |e| {
                    draft.write().scope.devices = match e.value().as_str() {
                        "roles" => Devices::Roles(vec!["workstation".into()]),
                        "named" => Devices::Named(vec![]),
                        _ => Devices::All,
                    };
                },
                option { value: "all", selected: mode == "all", "All" }
                option { value: "roles", selected: mode == "roles", "By role" }
                option { value: "named", selected: mode == "named", "These devices" }
            }
        }
        if mode != "all" {
            div { class: "row wrap",
                for (id , name) in if mode == "roles" { vec![("workstation".to_string(), "Workstations".to_string()), ("storage-server".to_string(), "Storage servers".to_string())] } else { lists.devices.clone() } {
                    {
                        let on = picked.contains(&id);
                        let id2 = id.clone();
                        rsx! {
                            label { key: "{id}", class: "check",
                                input {
                                    r#type: "checkbox",
                                    checked: on,
                                    onchange: move |e| {
                                        let mut w = draft.write();
                                        if let Devices::Roles(v) | Devices::Named(v) = &mut w.scope.devices {
                                            v.retain(|x| *x != id2);
                                            if e.checked() {
                                                v.push(id2.clone());
                                            }
                                        }
                                    },
                                }
                                "{name}"
                            }
                        }
                    }
                }
            }
        }
        div { class: "places",
            "Places"
            if r.scope.places.is_empty() {
                span { class: "muted", " everything" }
            }
            for (pi , p) in r.scope.places.iter().enumerate() {
                div { key: "{pi}", class: "row",
                    match p.clone() {
                        RulePlace::Location { location, path } => rsx! {
                            select {
                                onchange: move |e| {
                                    if let Some(RulePlace::Location { location, .. }) = draft.write().scope.places.get_mut(pi) {
                                        *location = e.value();
                                    }
                                },
                                {select_opts(&lists.locations, &location, "a location…")}
                            }
                            input {
                                placeholder: "folder inside (empty: all of it)",
                                value: "{path}",
                                oninput: move |e| {
                                    if let Some(RulePlace::Location { path, .. }) = draft.write().scope.places.get_mut(pi) {
                                        *path = e.value();
                                    }
                                },
                            }
                        },
                        RulePlace::Drive { path } => rsx! {
                            span { "Drive folder" }
                            input {
                                placeholder: "folder (empty: all of it)",
                                value: "{path}",
                                oninput: move |e| {
                                    if let Some(RulePlace::Drive { path }) = draft.write().scope.places.get_mut(pi) {
                                        *path = e.value();
                                    }
                                },
                            }
                        },
                        RulePlace::Project { project } => rsx! {
                            span { "Project" }
                            select {
                                onchange: move |e| {
                                    if let Some(RulePlace::Project { project }) = draft.write().scope.places.get_mut(pi) {
                                        *project = e.value();
                                    }
                                },
                                {select_opts(&lists.projects, &project, "a project…")}
                            }
                        },
                        RulePlace::Tag { tag } => rsx! {
                            span { "Tagged" }
                            select {
                                onchange: move |e| {
                                    if let Some(RulePlace::Tag { tag }) = draft.write().scope.places.get_mut(pi) {
                                        *tag = e.value();
                                    }
                                },
                                {select_opts(&lists.tags, &tag, "a tag…")}
                            }
                        },
                    }
                    button {
                        class: "x",
                        onclick: move |_| {
                            let mut rule = draft.write();
                            if pi < rule.scope.places.len() {
                                rule.scope.places.remove(pi);
                            }
                        },
                        "×"
                    }
                }
            }
            crate::shell::AddPick {
                class: "add",
                prompt: "+ Place…",
                on_pick: move |v: String| {
                    let p = match v.as_str() {
                        "location" => Some(RulePlace::Location { location: String::new(), path: String::new() }),
                        "drive" => Some(RulePlace::Drive { path: String::new() }),
                        "project" => Some(RulePlace::Project { project: String::new() }),
                        "tag" => Some(RulePlace::Tag { tag: String::new() }),
                        _ => None,
                    };
                    if let Some(p) = p {
                        draft.write().scope.places.push(p);
                    }
                },
                option { value: "location", "A location (a computer's folder)" }
                option { value: "drive", "A Drive folder" }
                option { value: "project", "A project" }
                option { value: "tag", "Things with a tag" }
            }
        }
    }
}

fn text_editor(text: Text, mut set: impl FnMut(Text) + Clone + 'static) -> Element {
    let t2 = text.clone();
    let mut set2 = set.clone();
    rsx! {
        select {
            onchange: move |e| {
                let mut t = text.clone();
                t.op = TextOp::parse(&e.value());
                set(t);
            },
            for op in TextOp::ALL {
                option { key: "{op.id()}", value: "{op.id()}", selected: t2.op == op, "{op.label()}" }
            }
        }
        input {
            value: "{t2.value}",
            placeholder: if t2.op == TextOp::Matches { "regular expression" } else { "text" },
            oninput: {
                let t2 = t2.clone();
                move |e: FormEvent| {
                    let mut t = t2.clone();
                    t.value = e.value();
                    set2(t);
                }
            },
        }
    }
}

const BYTES_PER_MIB: u128 = 1 << 20;
const MIB_DECIMAL_PLACES: usize = 20;

fn size_in_mib(bytes: u64) -> String {
    let bytes = u128::from(bytes);
    let whole = bytes / BYTES_PER_MIB;
    let remainder = bytes % BYTES_PER_MIB;
    if remainder == 0 {
        return whole.to_string();
    }
    // A fraction with denominator 2^20 has at most 20 decimal places.
    let decimal = remainder * 5_u128.pow(MIB_DECIMAL_PLACES as u32);
    let fraction = format!("{decimal:0MIB_DECIMAL_PLACES$}");
    format!("{whole}.{}", fraction.trim_end_matches('0'))
}

fn bytes_from_mib(value: &str) -> Option<u64> {
    let (whole, fraction) = value.split_once('.').unwrap_or((value, ""));
    if value.is_empty()
        || value == "."
        || fraction.len() > MIB_DECIMAL_PLACES
        || !whole
            .bytes()
            .chain(fraction.bytes())
            .all(|b| b.is_ascii_digit())
    {
        return None;
    }
    let whole = if whole.is_empty() {
        0
    } else {
        u128::from(whole.parse::<u64>().ok()?)
    };
    let partial = if fraction.is_empty() {
        0
    } else {
        let scale = 10_u128.pow(fraction.len() as u32);
        (fraction.parse::<u128>().ok()? * BYTES_PER_MIB + scale / 2) / scale
    };
    u64::try_from(whole * BYTES_PER_MIB + partial).ok()
}

#[component]
fn ConditionRow(
    draft: Signal<Rule>,
    gi: usize,
    ci: usize,
    cond: Condition,
    lists: Lists,
) -> Element {
    let mut put = move |t: Test| {
        if let Some(c) = draft
            .write()
            .when
            .groups
            .get_mut(gi)
            .and_then(|g| g.conditions.get_mut(ci))
        {
            c.test = t;
        }
    };
    let body = match cond.test.clone() {
        Test::Name { text } => text_editor(text, move |text| put(Test::Name { text })),
        Test::Path { text } => text_editor(text, move |text| put(Test::Path { text })),
        Test::Extension { any } => rsx! {
            input {
                placeholder: "wav, aif, mp3",
                value: "{any.join(\", \")}",
                onchange: move |e| put(Test::Extension {
                    any: e.value().split([',', ' ']).map(|s| s.trim().trim_start_matches('.').to_string()).filter(|s| !s.is_empty()).collect(),
                }),
            }
        },
        Test::Kind { kind, family } => rsx! {
            select {
                onchange: move |e| put(Test::Kind { kind: e.value(), family }),
                {select_opts(&lists.kinds, &kind, "a kind…")}
            }
            label { class: "check",
                input {
                    r#type: "checkbox",
                    checked: family,
                    onchange: {
                        let kind = kind.clone();
                        move |e: FormEvent| put(Test::Kind { kind: kind.clone(), family: e.checked() })
                    },
                }
                "or under it"
            }
        },
        Test::Size { cmp, bytes } => rsx! {
            select {
                onchange: move |e| put(Test::Size { cmp: Cmp::parse(&e.value()), bytes }),
                for c in Cmp::ALL {
                    option { key: "{c.id()}", value: "{c.id()}", selected: c == cmp, "{c.label()}" }
                }
            }
            input {
                class: "num",
                r#type: "number",
                min: "0",
                step: "any",
                value: "{size_in_mib(bytes)}",
                onchange: move |e| {
                    if let Some(bytes) = bytes_from_mib(&e.value()) {
                        put(Test::Size { cmp, bytes });
                    }
                },
            }
            span { "MiB" }
        },
        Test::Date { of, range } => {
            let (kind, n, unit, day) = match &range {
                Range::InLast { n, unit } => ("in-last", *n, *unit, String::new()),
                Range::OlderThan { n, unit } => ("older-than", *n, *unit, String::new()),
                Range::Before { day } => ("before", 0, Unit::Days, day.clone()),
                Range::After { day } => ("after", 0, Unit::Days, day.clone()),
            };
            let mk = move |kind: &str, n: u32, unit: Unit, day: String| match kind {
                "in-last" => Range::InLast { n, unit },
                "before" => Range::Before { day },
                "after" => Range::After { day },
                _ => Range::OlderThan { n, unit },
            };
            let day2 = day.clone();
            let day3 = day.clone();
            let kind_s = kind.to_string();
            let kind_s2 = kind.to_string();
            let kind_s3 = kind.to_string();
            rsx! {
                select {
                    onchange: {
                        let range = range.clone();
                        move |e: FormEvent| put(Test::Date { of: DateOf::parse(&e.value()), range: range.clone() })
                    },
                    for o in DateOf::ALL {
                        option { key: "{o.id()}", value: "{o.id()}", selected: o == of, "{o.id()}" }
                    }
                }
                select {
                    onchange: move |e| put(Test::Date { of, range: mk(&e.value(), n.max(1), unit, day2.clone()) }),
                    option { value: "in-last", selected: kind == "in-last", "in the last" }
                    option { value: "older-than", selected: kind == "older-than", "more than … ago" }
                    option { value: "before", selected: kind == "before", "before" }
                    option { value: "after", selected: kind == "after", "on or after" }
                }
                if kind == "in-last" || kind == "older-than" {
                    input {
                        class: "num",
                        r#type: "number",
                        min: "1",
                        value: "{n}",
                        oninput: move |e| put(Test::Date { of, range: mk(&kind_s, e.value().parse().unwrap_or(1), unit, String::new()) }),
                    }
                    select {
                        onchange: move |e| put(Test::Date { of, range: mk(&kind_s2, n, Unit::parse(&e.value()), String::new()) }),
                        for u in Unit::ALL {
                            option { key: "{u.id()}", value: "{u.id()}", selected: u == unit, "{u.id()}" }
                        }
                    }
                } else {
                    input {
                        r#type: "date",
                        value: "{day3}",
                        oninput: move |e| put(Test::Date { of, range: mk(&kind_s3, 0, unit, e.value()) }),
                    }
                }
            }
        }
        Test::Tag { tag } => rsx! {
            select {
                onchange: move |e| put(Test::Tag { tag: e.value() }),
                {select_opts(&lists.tags, &tag, "a tag…")}
            }
        },
        Test::Field { name, text } => {
            let n2 = name.clone();
            let t1 = text.clone();
            rsx! {
                input {
                    class: "short",
                    placeholder: "field",
                    value: "{name}",
                    oninput: move |e: FormEvent| put(Test::Field { name: e.value(), text: t1.clone() }),
                }
                {text_editor(text, move |text| put(Test::Field { name: n2.clone(), text }))}
            }
        }
        Test::Project { project } => rsx! {
            select {
                onchange: move |e| put(Test::Project { project: e.value() }),
                {select_opts(&lists.projects, &project, "any project")}
            }
        },
        Test::Copies { cmp, n } => rsx! {
            select {
                onchange: move |e| put(Test::Copies { cmp: Cmp::parse(&e.value()), n }),
                for c in Cmp::ALL {
                    option { key: "{c.id()}", value: "{c.id()}", selected: c == cmp, "{c.label()}" }
                }
            }
            input {
                class: "num",
                r#type: "number",
                min: "0",
                value: "{n}",
                oninput: move |e| put(Test::Copies { cmp, n: e.value().parse().unwrap_or(0) }),
            }
        },
        Test::HeldBy { device } => rsx! {
            select {
                onchange: move |e| put(Test::HeldBy { device: e.value() }),
                {select_opts(&lists.devices, &device, "a device…")}
            }
        },
    };
    let kind = test_id(&cond.test);
    rsx! {
        div { class: "cond",
            select {
                class: "not",
                onchange: move |e| {
                    if let Some(c) = draft.write().when.groups.get_mut(gi).and_then(|g| g.conditions.get_mut(ci)) {
                        c.not = e.value() == "not";
                    }
                },
                option { value: "is", selected: !cond.not, "is" }
                option { value: "not", selected: cond.not, "not" }
            }
            select {
                onchange: move |e| put(new_test(&e.value())),
                for (id , label) in TESTS {
                    option { key: "{id}", value: "{id}", selected: kind == id, "{label}" }
                }
            }
            {body}
            button {
                class: "x",
                onclick: move |_| {
                    if let Some(g) = draft.write().when.groups.get_mut(gi)
                        && ci < g.conditions.len()
                    {
                        g.conditions.remove(ci);
                    }
                },
                "×"
            }
        }
    }
}

fn dest_editor(to: Dest, lists: &Lists, mut set: impl FnMut(Dest) + Clone + 'static) -> Element {
    let (where_, location, path) = match &to {
        Dest::Location { location, path } => ("location", location.clone(), path.clone()),
        Dest::Drive { path } => ("drive", String::new(), path.clone()),
    };
    let mut set2 = set.clone();
    let mut set3 = set.clone();
    let (l2, p2, p3) = (location.clone(), path.clone(), path.clone());
    rsx! {
        select {
            onchange: move |e| set(if e.value() == "drive" {
                Dest::Drive { path: p2.clone() }
            } else {
                Dest::Location { location: String::new(), path: p2.clone() }
            }),
            option { value: "drive", selected: where_ == "drive", "the Drive" }
            option { value: "location", selected: where_ == "location", "a location" }
        }
        if where_ == "location" {
            select {
                onchange: move |e| set2(Dest::Location { location: e.value(), path: p3.clone() }),
                {select_opts(&lists.locations, &location, "a location…")}
            }
        }
        input {
            placeholder: "folder, e.g. Sorted/{{year}}/{{month}}",
            value: "{path}",
            oninput: move |e| set3(if where_ == "drive" {
                Dest::Drive { path: e.value() }
            } else {
                Dest::Location { location: l2.clone(), path: e.value() }
            }),
        }
    }
}

fn tag_picker(
    tags: Vec<String>,
    lists: &Lists,
    set: impl FnMut(Vec<String>) + Clone + 'static,
) -> Element {
    let mut set2 = set.clone();
    let t2 = tags.clone();
    rsx! {
        for t in tags.iter() {
            {
                let (t, tags) = (t.clone(), tags.clone());
                let mut set = set.clone();
                let label = lists.tags.iter().find(|(i, _)| *i == t).map_or(t.clone(), |(_, n)| n.clone());
                rsx! {
                    span { key: "{t}", class: "chip",
                        "{label} "
                        button { class: "x", onclick: move |_| set(tags.iter().filter(|x| **x != t).cloned().collect()), "×" }
                    }
                }
            }
        }
        crate::shell::AddPick {
            prompt: "+ tag…",
            on_pick: move |v: String| {
                if !t2.contains(&v) {
                    let mut n = t2.clone();
                    n.push(v);
                    set2(n);
                }
            },
            for (id , name) in lists.tags.iter() {
                option { key: "{id}", value: "{id}", "{name}" }
            }
        }
    }
}

#[component]
fn ActionRow(draft: Signal<Rule>, ai: usize, action: Action, lists: Lists) -> Element {
    let mut put = move |a: Action| {
        if let Some(x) = draft.write().actions.get_mut(ai) {
            *x = a;
        }
    };
    let label = ACTIONS
        .iter()
        .find(|(id, _)| *id == action.kind())
        .map_or("", |(_, l)| *l);
    let body = match action.clone() {
        Action::AddTags { tags } => {
            tag_picker(tags, &lists, move |tags| put(Action::AddTags { tags }))
        }
        Action::RemoveTags { tags } => {
            tag_picker(tags, &lists, move |tags| put(Action::RemoveTags { tags }))
        }
        Action::SetField { name, value } => {
            let (n2, v2) = (name.clone(), value.clone());
            rsx! {
                input { class: "short", placeholder: "field", value: "{name}", oninput: move |e| put(Action::SetField { name: e.value(), value: v2.clone() }) }
                input { placeholder: "value (a template)", value: "{value}", oninput: move |e| put(Action::SetField { name: n2.clone(), value: e.value() }) }
            }
        }
        Action::AddToProject { project } => rsx! {
            select {
                onchange: move |e| put(Action::AddToProject { project: e.value() }),
                {select_opts(&lists.projects, &project, "a project…")}
            }
        },
        Action::Copy { to, name } | Action::Move { to, name } => {
            let copy = matches!(action, Action::Copy { .. });
            let (n2, to2) = (name.clone(), to.clone());
            let mk = move |to: Dest, name: String| {
                if copy {
                    Action::Copy { to, name }
                } else {
                    Action::Move { to, name }
                }
            };
            rsx! {
                {dest_editor(to, &lists, move |to| put(mk(to, n2.clone())))}
                input {
                    placeholder: "new name (empty: keep it)",
                    value: "{name}",
                    oninput: move |e| put(mk(to2.clone(), e.value())),
                }
            }
        }
        Action::Rename { name } => rsx! {
            input { value: "{name}", oninput: move |e| put(Action::Rename { name: e.value() }) }
        },
        Action::Sidecars {
            thumbnail,
            waveform,
        } => rsx! {
            label { class: "check",
                input { r#type: "checkbox", checked: thumbnail, onchange: move |e| put(Action::Sidecars { thumbnail: e.checked(), waveform }) }
                "thumbnail"
            }
            label { class: "check",
                input { r#type: "checkbox", checked: waveform, onchange: move |e| put(Action::Sidecars { thumbnail, waveform: e.checked() }) }
                "waveform"
            }
        },
        Action::HighlyAvailable => rsx! {
            span { class: "muted", "on the node, and public storage once there is one" }
        },
        Action::Trash => rsx! {
            span { class: "muted", "a folder, never a deletion; not offered for a computer's files" }
        },
    };
    let n = draft().actions.len();
    rsx! {
        div { class: "act",
            span { class: "ord", "{ai + 1}." }
            span { class: "albl", "{label}" }
            {body}
            div { class: "moves",
                button {
                    class: "x",
                    disabled: ai == 0,
                    onclick: move |_| {
                        let mut rule = draft.write();
                        if ai > 0 && ai < rule.actions.len() {
                            rule.actions.swap(ai, ai - 1);
                        }
                    },
                    "↑"
                }
                button {
                    class: "x",
                    disabled: ai + 1 >= n,
                    onclick: move |_| {
                        let mut rule = draft.write();
                        if ai + 1 < rule.actions.len() {
                            rule.actions.swap(ai, ai + 1);
                        }
                    },
                    "↓"
                }
                button {
                    class: "x",
                    onclick: move |_| {
                        let mut w = draft.write();
                        if ai < w.actions.len() {
                            w.actions.remove(ai);
                        }
                    },
                    "×"
                }
            }
        }
    }
}

/// The sidebar's entry.
#[component]
pub fn SidebarRules() -> Element {
    let space = use_drive().org.clone();
    let rules_doc = use_doc(doc::rules_doc(&space));
    let n = doc::rules(&rules_doc.loro())
        .iter()
        .filter(|r| r.enabled)
        .count();
    rsx! {
        crate::shell::PlaceRow {
            at: crate::nav::At { place: Place::Rules, within: None },
            label: "Rules",
            icon: "Sync",
            note: if n > 0 { n.to_string() } else { String::new() },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn size_input_round_trips_bytes() {
        for bytes in [0, 1, 512, 524_288, 1_048_576, 1_048_577, u64::MAX] {
            assert_eq!(bytes_from_mib(&size_in_mib(bytes)), Some(bytes));
        }
        assert_eq!(size_in_mib(524_288), "0.5");
        assert_eq!(bytes_from_mib(".25"), Some(262_144));
    }

    #[test]
    fn size_input_rejects_out_of_range_or_invalid_values() {
        for value in [
            "17592186044416",
            "18446744073709551615",
            "-1",
            ".",
            "",
            "NaN",
        ] {
            assert_eq!(bytes_from_mib(value), None, "{value}");
        }
    }
}

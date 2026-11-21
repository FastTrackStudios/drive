//! The evaluator: a file's facts and a rule in, whether it matches and a
//! plan (what each action would do) out. Pure: the app's dry run, the
//! agents' runs and the node's all decide with this one function, so what
//! the preview shows is what a live run does.

use drive_files::projects::Project;
use drive_spine::kinds::Kinds;
use drive_spine::tags::{Library, OnTarget, Target};
use regex::Regex;
use serde::{Deserialize, Serialize};

use crate::model::{
    Action, Condition, Dest, Devices, Group, Place, Range, Rule, Test, Text, TextOp,
};
use crate::names::{clean_path, split_ext};
use crate::template::{self, Tokens};

/// A file (or a bundle) a rule may take: where it is.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", tag = "in")]
pub enum Item {
    /// In a location of a device; `path` relative to its root.
    Location {
        device: String,
        location: String,
        path: String,
    },
    /// In the Drive tree: its tree id and path.
    Drive { id: String, path: String },
}

impl Item {
    /// A stable key (loop protection, the journal).
    pub fn key(&self) -> String {
        match self {
            Item::Location { location, path, .. } => format!("loc:{location}:{path}"),
            Item::Drive { id, .. } => format!("drive:{id}"),
        }
    }

    pub fn path(&self) -> &str {
        match self {
            Item::Location { path, .. } | Item::Drive { path, .. } => path,
        }
    }

    pub fn name(&self) -> &str {
        let p = self.path();
        p.rsplit('/').next().unwrap_or(p)
    }

    /// The folder it is in (`""`: the top).
    pub fn folder(&self) -> &str {
        self.path().rsplit_once('/').map_or("", |(f, _)| f)
    }
}

/// What is known about a file.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct Facts {
    pub item: Option<Item>,
    /// A bundle (a folder that is one thing: `.logicx`, `.app`).
    pub bundle: bool,
    pub size: u64,
    /// Milliseconds.
    pub modified: i64,
    pub created: Option<i64>,
    pub added: Option<i64>,
    /// The content key tags hang on: a location file's sampled hash (a
    /// Drive file is tagged by its tree id).
    pub content: String,
    /// How many locations hold the same content.
    pub copies: u32,
    /// The devices holding a copy.
    pub holders: Vec<String>,
}

impl Facts {
    pub fn item(&self) -> &Item {
        self.item.as_ref().expect("facts name their item")
    }

    /// What its tags and fields are kept on.
    pub fn target(&self) -> Option<Target> {
        match self.item() {
            Item::Drive { id, .. } => Some(Target::File(id.clone())),
            Item::Location { .. } if !self.content.is_empty() => {
                Some(Target::Content(self.content.clone()))
            }
            Item::Location { .. } => None,
        }
    }
}

/// A device, as rules see it.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct DeviceInfo {
    pub id: String,
    pub name: String,
    pub roles: Vec<String>,
}

/// A location, as rules see it.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct LocInfo {
    pub id: String,
    pub name: String,
    pub device: String,
    /// In a storage the device runs (S3, WebDAV), not on its disk.
    pub in_storage: bool,
}

/// Who is running the rule.
#[derive(Clone, Debug, PartialEq)]
pub enum Runner {
    /// A device's agent, over its own locations. `moves_allowed`: its
    /// config's `allow_rule_moves` (off unless turned on by hand).
    Device { id: String, moves_allowed: bool },
    /// The node, over the Drive tree.
    Node,
    /// The app's preview: plans as the device would, with its own switch.
    Preview { moves_allowed: bool },
}

/// Everything else the evaluator needs, read once per pass.
pub struct World<'a> {
    pub now: i64,
    pub kinds: &'a Kinds,
    pub tags: &'a Library,
    pub devices: &'a [DeviceInfo],
    pub locations: &'a [LocInfo],
    pub projects: &'a [Project],
    pub runner: Runner,
}

impl World<'_> {
    fn location(&self, id: &str) -> Option<&LocInfo> {
        self.locations.iter().find(|l| l.id == id)
    }

    fn device_name(&self, id: &str) -> String {
        self.devices
            .iter()
            .find(|d| d.id == id)
            .map_or_else(|| id.to_string(), |d| d.name.clone())
    }

    fn on(&self, f: &Facts) -> OnTarget {
        f.target().map(|t| self.tags.on(&t)).unwrap_or_default()
    }

    /// The projects a file is in (by their parts: folders of locations).
    pub fn projects_of(&self, item: &Item) -> Vec<&Project> {
        let Item::Location { location, path, .. } = item else {
            return Vec::new();
        };
        self.projects
            .iter()
            .filter(|p| {
                p.parts
                    .iter()
                    .any(|part| part.location == *location && part.contains(path))
            })
            .collect()
    }

    fn has_tag(&self, on: &OnTarget, tag: &str) -> bool {
        on.tags
            .iter()
            .any(|a| a.tag == tag || self.tags.ancestors(&a.tag).contains(tag))
    }
}

/// Why a rule cannot be used as written.
#[derive(Clone, Debug, PartialEq)]
pub enum RuleError {
    /// A regular expression that does not compile.
    Regex { value: String, error: String },
    /// A day that is not `YYYY-MM-DD`.
    Day(String),
}

impl std::fmt::Display for RuleError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RuleError::Regex { value, error } => {
                write!(f, "/{value}/ is not a regular expression: {error}")
            }
            RuleError::Day(d) => write!(f, "{d} is not a day (YYYY-MM-DD)"),
        }
    }
}

/// Whether a rule runs on a device at all (its device scope).
pub fn runs_on(rule: &Rule, device: &DeviceInfo) -> bool {
    match &rule.scope.devices {
        Devices::All => true,
        Devices::Roles(roles) => {
            let mine: Vec<&str> = if device.roles.iter().any(|r| r == "storage-server") {
                vec!["storage-server"]
            } else {
                vec!["workstation"]
            };
            roles
                .iter()
                .any(|r| mine.contains(&r.as_str()) || device.roles.contains(r))
        }
        Devices::Named(ids) => ids.contains(&device.id),
    }
}

/// Whether a rule looks at the Drive tree (the node runs those).
pub fn covers_drive(rule: &Rule) -> bool {
    rule.scope.places.is_empty()
        || rule
            .scope
            .places
            .iter()
            .any(|p| matches!(p, Place::Drive { .. } | Place::Tag { .. }))
}

/// Whether a rule looks at a location at all (so a device can skip
/// reading it).
pub fn covers_location(rule: &Rule, location: &str, projects: &[Project]) -> bool {
    rule.scope.places.is_empty()
        || rule.scope.places.iter().any(|p| match p {
            Place::Location { location: l, .. } => l == location,
            Place::Tag { .. } => true,
            Place::Project { project } => projects
                .iter()
                .any(|pr| pr.id == *project && pr.parts.iter().any(|pa| pa.location == location)),
            Place::Drive { .. } => false,
        })
}

fn within(path: &str, folder: &str) -> bool {
    let folder = folder.trim_matches('/');
    folder.is_empty() || path == folder || path.starts_with(&format!("{folder}/"))
}

/// A rule, ready to test files (its regular expressions compiled once).
pub struct Matcher<'r> {
    pub rule: &'r Rule,
    regexes: Vec<(String, Regex)>,
    /// The rule's actions, one tag per tag action.
    actions: Vec<Action>,
}

impl<'r> Matcher<'r> {
    pub fn new(rule: &'r Rule) -> Result<Matcher<'r>, RuleError> {
        let mut regexes = Vec::new();
        for g in &rule.when.groups {
            for c in &g.conditions {
                let text = match &c.test {
                    Test::Name { text } | Test::Path { text } | Test::Field { text, .. } => {
                        Some(text)
                    }
                    Test::Date {
                        range: Range::Before { day } | Range::After { day },
                        ..
                    } => {
                        if template::day_ms(day).is_none() {
                            return Err(RuleError::Day(day.clone()));
                        }
                        None
                    }
                    _ => None,
                };
                if let Some(t) = text
                    && t.op == TextOp::Matches
                    && !regexes.iter().any(|(v, _)| *v == t.value)
                {
                    let re = Regex::new(&t.value).map_err(|e| RuleError::Regex {
                        value: t.value.clone(),
                        error: e.to_string(),
                    })?;
                    regexes.push((t.value.clone(), re));
                }
            }
        }
        Ok(Matcher {
            rule,
            regexes,
            actions: one_tag_per_action(&rule.actions),
        })
    }

    /// Whether the file is in the rule's places.
    pub fn in_scope(&self, f: &Facts, w: &World) -> bool {
        let places = &self.rule.scope.places;
        if places.is_empty() {
            return true;
        }
        let item = f.item();
        places.iter().any(|p| match (p, item) {
            (
                Place::Location { location, path },
                Item::Location {
                    location: l,
                    path: ip,
                    ..
                },
            ) => location == l && within(ip, path),
            (Place::Drive { path }, Item::Drive { path: ip, .. }) => within(ip, path),
            (Place::Project { project }, _) => w.projects_of(item).iter().any(|p| p.id == *project),
            (Place::Tag { tag }, _) => w.has_tag(&w.on(f), tag),
            _ => false,
        })
    }

    fn text(&self, t: &Text, s: &str) -> bool {
        let (v, s_low) = (t.value.to_lowercase(), s.to_lowercase());
        match t.op {
            TextOp::Is => s_low == v,
            TextOp::Contains => s_low.contains(&v),
            TextOp::StartsWith => s_low.starts_with(&v),
            TextOp::EndsWith => s_low.ends_with(&v),
            TextOp::Matches => self
                .regexes
                .iter()
                .find(|(val, _)| *val == t.value)
                .is_some_and(|(_, re)| re.is_match(s)),
        }
    }

    /// One test: `None` when it cannot be known (a date the file system
    /// does not record): such a condition never holds, negated or not.
    fn test(&self, t: &Test, f: &Facts, w: &World, on: &OnTarget) -> Option<bool> {
        let name = f.item().name();
        Some(match t {
            Test::Name { text } => self.text(text, name),
            Test::Path { text } => self.text(text, f.item().path()),
            Test::Extension { any } => {
                let (_, ext) = split_ext(name);
                any.iter()
                    .any(|e| e.trim_start_matches('.').eq_ignore_ascii_case(ext))
            }
            Test::Kind { kind, family } => {
                let k = w.kinds.of_file(name, f.bundle);
                k.id == *kind || (*family && k.parent == *kind)
            }
            Test::Size { cmp, bytes } => cmp.holds(f.size, *bytes),
            Test::Date { of, range } => {
                let at = match of {
                    crate::model::DateOf::Created => f.created?,
                    crate::model::DateOf::Modified => f.modified,
                    crate::model::DateOf::Added => f.added?,
                };
                match range {
                    Range::InLast { n, unit } => at >= w.now - i64::from(*n) * unit.ms(),
                    Range::OlderThan { n, unit } => at < w.now - i64::from(*n) * unit.ms(),
                    Range::Before { day } => at < template::day_ms(day)?,
                    Range::After { day } => at >= template::day_ms(day)?,
                }
            }
            Test::Tag { tag } => w.has_tag(on, tag),
            Test::Field { name, text } => on
                .fields
                .iter()
                .any(|fl| fl.name.eq_ignore_ascii_case(name.trim()) && self.text(text, &fl.value)),
            Test::Project { project } => {
                let ps = w.projects_of(f.item());
                if project.is_empty() {
                    !ps.is_empty()
                } else {
                    ps.iter().any(|p| p.id == *project)
                }
            }
            Test::Copies { cmp, n } => cmp.holds(u64::from(f.copies), u64::from(*n)),
            Test::HeldBy { device } => f.holders.iter().any(|h| h == device),
        })
    }

    fn condition(&self, c: &Condition, f: &Facts, w: &World, on: &OnTarget) -> bool {
        match self.test(&c.test, f, w, on) {
            Some(v) => v != c.not,
            None => false,
        }
    }

    fn group(&self, g: &Group, f: &Facts, w: &World, on: &OnTarget) -> bool {
        if g.conditions.is_empty() {
            return true;
        }
        if g.all {
            g.conditions.iter().all(|c| self.condition(c, f, w, on))
        } else {
            g.conditions.iter().any(|c| self.condition(c, f, w, on))
        }
    }

    /// Whether the file is in scope and meets the conditions.
    pub fn matches(&self, f: &Facts, w: &World) -> bool {
        if !self.in_scope(f, w) {
            return false;
        }
        let when = &self.rule.when;
        if when.groups.is_empty() {
            return true;
        }
        let on = w.on(f);
        if when.all {
            when.groups.iter().all(|g| self.group(g, f, w, &on))
        } else {
            when.groups.iter().any(|g| self.group(g, f, w, &on))
        }
    }

    /// What the rule would do to a file it matches, step by step. A move
    /// or rename that goes ahead changes where the later steps act.
    pub fn plan(&self, f: &Facts, w: &World) -> Plan {
        let mut cur = f.item().clone();
        let on = w.on(f);
        let mut steps = Vec::new();
        for (i, a) in self.actions.iter().enumerate() {
            let tokens = tokens(&cur, f, w);
            let step = plan_one(i, a, &cur, f, w, &on, &tokens);
            if step.blocked.is_none()
                && !step.noop
                && let Effect::Move { to } = &step.effect
            {
                cur = to.as_item(&cur);
            }
            steps.push(step);
        }
        Plan {
            rule: self.rule.id.clone(),
            item: f.item().clone(),
            steps,
        }
    }
}

/// Where a copy or move lands (before a clash adds " (2)").
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", tag = "in")]
pub enum Spot {
    Location {
        device: String,
        location: String,
        folder: String,
        name: String,
    },
    Drive {
        folder: String,
        name: String,
    },
}

impl Spot {
    pub fn path(&self) -> String {
        let (folder, name) = match self {
            Spot::Location { folder, name, .. } | Spot::Drive { folder, name } => (folder, name),
        };
        if folder.is_empty() {
            name.clone()
        } else {
            format!("{folder}/{name}")
        }
    }

    pub fn name(&self) -> &str {
        match self {
            Spot::Location { name, .. } | Spot::Drive { name, .. } => name,
        }
    }

    pub fn folder(&self) -> &str {
        match self {
            Spot::Location { folder, .. } | Spot::Drive { folder, .. } => folder,
        }
    }

    /// The item it would be (a Drive item keeps its tree id when moved).
    pub fn as_item(&self, from: &Item) -> Item {
        match self {
            Spot::Location {
                device, location, ..
            } => Item::Location {
                device: device.clone(),
                location: location.clone(),
                path: self.path(),
            },
            Spot::Drive { .. } => Item::Drive {
                id: match from {
                    Item::Drive { id, .. } => id.clone(),
                    Item::Location { .. } => String::new(),
                },
                path: self.path(),
            },
        }
    }
}

/// One action's planned effect.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", tag = "effect")]
pub enum Effect {
    Tag {
        tag: String,
        on: bool,
    },
    Field {
        name: String,
        value: String,
    },
    Project {
        project: String,
    },
    Copy {
        to: Spot,
    },
    /// A move or a rename.
    Move {
        to: Spot,
    },
    Sidecars {
        kinds: Vec<String>,
    },
    Available,
    Trash,
    /// A step that cannot be worked out (always blocked).
    Nothing,
}

impl Effect {
    pub fn kind(&self) -> &'static str {
        match self {
            Effect::Tag { on: true, .. } => "tag",
            Effect::Tag { on: false, .. } => "untag",
            Effect::Field { .. } => "field",
            Effect::Project { .. } => "project",
            Effect::Copy { .. } => "copy",
            Effect::Move { .. } => "move",
            Effect::Sidecars { .. } => "sidecars",
            Effect::Available => "available",
            Effect::Trash => "trash",
            Effect::Nothing => "none",
        }
    }
}

/// One step of a plan.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Step {
    /// The action's index (tag actions split one tag per step).
    pub action: usize,
    pub effect: Effect,
    /// In words, for the preview and the journal.
    pub says: String,
    /// Why it will not be done (a safety rule, a missing place).
    pub blocked: Option<String>,
    /// Already so: nothing to do.
    pub noop: bool,
}

/// What a rule would do to one file.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Plan {
    pub rule: String,
    pub item: Item,
    pub steps: Vec<Step>,
}

impl Plan {
    /// Whether any step would change something.
    pub fn does_something(&self) -> bool {
        self.steps.iter().any(|s| s.blocked.is_none() && !s.noop)
    }
}

/// Moves of files on a computer are off unless the device says otherwise.
pub const MOVES_OFF: &str =
    "moves are off on this device (turn on allow_rule_moves in its config to allow them)";
/// Trash is never offered for files on a computer.
pub const NO_TRASH: &str = "Trash is not offered for files on a computer";

fn tokens(cur: &Item, f: &Facts, w: &World) -> Tokens {
    let name = cur.name();
    let (stem, ext) = split_ext(name);
    Tokens {
        name: stem.to_string(),
        ext: ext.to_string(),
        date: f.created.unwrap_or(f.modified),
        kind: w.kinds.of_file(name, f.bundle).name.clone(),
        project: w
            .projects_of(cur)
            .first()
            .map_or_else(|| "No project".into(), |p| p.name.clone()),
        device: match cur {
            Item::Location { device, .. } => w.device_name(device),
            Item::Drive { .. } => "Drive".into(),
        },
    }
}

fn step(action: usize, effect: Effect, says: String) -> Step {
    Step {
        action,
        effect,
        says,
        blocked: None,
        noop: false,
    }
}

fn spot(dest: &Dest, name: &str, w: &World, t: &Tokens) -> Result<Spot, String> {
    match dest {
        Dest::Location { location, path } => {
            let l = w
                .location(location)
                .ok_or_else(|| format!("there is no location {location}"))?;
            Ok(Spot::Location {
                device: l.device.clone(),
                location: l.id.clone(),
                folder: clean_path(&template::render(path, t)),
                name: name.to_string(),
            })
        }
        Dest::Drive { path } => Ok(Spot::Drive {
            folder: clean_path(&template::render(path, t)),
            name: name.to_string(),
        }),
    }
}

fn spot_label(s: &Spot, w: &World) -> String {
    match s {
        Spot::Location {
            device, location, ..
        } => format!(
            "{} on {}: {}",
            w.location(location)
                .map_or(location.as_str(), |l| l.name.as_str()),
            w.device_name(device),
            s.path()
        ),
        Spot::Drive { .. } => format!("the Drive: {}", s.path()),
    }
}

fn moves_allowed(w: &World) -> bool {
    match w.runner {
        Runner::Device { moves_allowed, .. } | Runner::Preview { moves_allowed } => moves_allowed,
        Runner::Node => true,
    }
}

fn plan_one(
    i: usize,
    a: &Action,
    cur: &Item,
    f: &Facts,
    w: &World,
    on: &OnTarget,
    t: &Tokens,
) -> Step {
    let drive = matches!(cur, Item::Drive { .. });
    match a {
        Action::AddTags { tags } | Action::RemoveTags { tags } => {
            let add = matches!(a, Action::AddTags { .. });
            // One step per tag (so the journal can undo each).
            let tag = tags.first().cloned().unwrap_or_default();
            let mut s = step(
                i,
                Effect::Tag {
                    tag: tag.clone(),
                    on: add,
                },
                format!(
                    "{} {}",
                    if add { "tag" } else { "untag" },
                    w.tags.get(&tag).map_or(tag.as_str(), |t| t.name.as_str())
                ),
            );
            if w.tags.get(&tag).is_none() {
                s.blocked = Some(format!("there is no tag {tag}"));
            } else if f.target().is_none() {
                s.blocked = Some("it has no content hash yet".into());
            } else {
                s.noop = on.tags.iter().any(|x| x.tag == tag) == add;
            }
            s
        }
        Action::SetField { name, value } => {
            let value = template::render(value, t);
            let mut s = step(
                i,
                Effect::Field {
                    name: name.trim().to_string(),
                    value: value.clone(),
                },
                format!("set {} to “{value}”", name.trim()),
            );
            if name.trim().is_empty() {
                s.blocked = Some("the field needs a name".into());
            } else if f.target().is_none() {
                s.blocked = Some("it has no content hash yet".into());
            } else {
                s.noop = on
                    .fields
                    .iter()
                    .any(|fl| fl.name.eq_ignore_ascii_case(name.trim()) && fl.value == value);
            }
            s
        }
        Action::AddToProject { project } => {
            let name = w
                .projects
                .iter()
                .find(|p| p.id == *project)
                .map(|p| p.name.clone());
            let mut s = step(
                i,
                Effect::Project {
                    project: project.clone(),
                },
                format!("add to {}", name.as_deref().unwrap_or(project)),
            );
            if name.is_none() {
                s.blocked = Some(format!("there is no project {project}"));
            } else if drive {
                s.blocked = Some(
                    "projects hold folders of computers' locations, not Drive files yet".into(),
                );
            } else {
                s.noop = w.projects_of(cur).iter().any(|p| p.id == *project);
            }
            s
        }
        Action::Copy { to, name } | Action::Move { to, name } => {
            let copy = matches!(a, Action::Copy { .. });
            let new_name = if name.trim().is_empty() {
                cur.name().to_string()
            } else {
                template::file_name(name, t)
            };
            let target = match spot(to, &new_name, w, t) {
                Ok(s) => s,
                Err(e) => {
                    let mut s = step(
                        i,
                        Effect::Nothing,
                        format!("{} it", if copy { "copy" } else { "move" }),
                    );
                    s.blocked = Some(e);
                    return s;
                }
            };
            let says = format!(
                "{} to {}",
                if copy { "copy" } else { "move" },
                spot_label(&target, w)
            );
            if copy {
                let mut s = step(i, Effect::Copy { to: target.clone() }, says);
                if let Spot::Location { location, .. } = &target
                    && w.location(location).is_some_and(|l| l.in_storage)
                {
                    s.blocked = Some("copies into a storage's location are not built yet".into());
                }
                s
            } else {
                move_step(i, target, says, cur, w)
            }
        }
        Action::Rename { name } => {
            let new_name = template::file_name(name, t);
            let target = match cur {
                Item::Location {
                    device, location, ..
                } => Spot::Location {
                    device: device.clone(),
                    location: location.clone(),
                    folder: cur.folder().to_string(),
                    name: new_name.clone(),
                },
                Item::Drive { .. } => Spot::Drive {
                    folder: cur.folder().to_string(),
                    name: new_name.clone(),
                },
            };
            move_step(i, target, format!("rename to “{new_name}”"), cur, w)
        }
        Action::Sidecars {
            thumbnail,
            waveform,
        } => {
            let mut kinds = Vec::new();
            if *thumbnail {
                kinds.push("thumb".to_string());
            }
            if *waveform {
                kinds.push("peaks".to_string());
            }
            let mut s = step(
                i,
                Effect::Sidecars {
                    kinds: kinds.clone(),
                },
                format!("make {}", kinds.join(" and ")),
            );
            if kinds.is_empty() {
                s.noop = true;
            } else if drive {
                s.blocked = Some("sidecars of Drive files are made by the node's own pass".into());
            }
            s
        }
        Action::HighlyAvailable => {
            let mut s = step(i, Effect::Available, "keep it highly available".into());
            s.noop = drive;
            s
        }
        Action::Trash => {
            let mut s = step(i, Effect::Trash, "move it to the Drive's Trash".into());
            if !drive {
                s.blocked = Some(NO_TRASH.into());
            }
            s
        }
    }
}

fn move_step(i: usize, target: Spot, says: String, cur: &Item, w: &World) -> Step {
    let noop = target.as_item(cur).path() == cur.path()
        && match (&target, cur) {
            (Spot::Location { location, .. }, Item::Location { location: l, .. }) => location == l,
            (Spot::Drive { .. }, Item::Drive { .. }) => true,
            _ => false,
        };
    let mut s = step(i, Effect::Move { to: target.clone() }, says);
    s.noop = noop;
    match (cur, &target) {
        (Item::Drive { .. }, Spot::Drive { .. }) => {}
        (
            Item::Location { device, .. },
            Spot::Location {
                device: to_dev,
                location,
                ..
            },
        ) => {
            if device != to_dev {
                s.blocked =
                    Some("a move to another computer is not built yet: copy instead".into());
            } else if w.location(location).is_some_and(|l| l.in_storage) {
                s.blocked = Some("moves into a storage's location are not built yet".into());
            } else if !moves_allowed(w) {
                s.blocked = Some(MOVES_OFF.into());
            }
        }
        _ => {
            s.blocked = Some(
                "a move between a computer and the Drive is not built yet: copy instead".into(),
            );
        }
    }
    s
}

/// Rules that would feed themselves: a copy or move into a place the rule
/// itself looks at. Allowed (the agents keep a rule off what it made), but
/// the app warns.
pub fn feeds_itself(rule: &Rule) -> bool {
    rule.actions.iter().any(|a| {
        let to = match a {
            Action::Copy { to, .. } | Action::Move { to, .. } => to,
            _ => return false,
        };
        rule.scope.places.is_empty()
            || rule.scope.places.iter().any(|p| match (p, to) {
                (
                    Place::Location { location, path },
                    Dest::Location {
                        location: l,
                        path: dp,
                    },
                ) => location == l && within(dp, path),
                (Place::Drive { path }, Dest::Drive { path: dp }) => within(dp, path),
                (Place::Tag { .. } | Place::Project { .. }, _) => true,
                _ => false,
            })
    })
}

/// Expand an action list so tag actions with several tags are one step
/// per tag (what [`Matcher::plan`] expects).
pub fn one_tag_per_action(actions: &[Action]) -> Vec<Action> {
    actions
        .iter()
        .flat_map(|a| match a {
            Action::AddTags { tags } if tags.len() > 1 => tags
                .iter()
                .map(|t| Action::AddTags {
                    tags: vec![t.clone()],
                })
                .collect(),
            Action::RemoveTags { tags } if tags.len() > 1 => tags
                .iter()
                .map(|t| Action::RemoveTags {
                    tags: vec![t.clone()],
                })
                .collect(),
            other => vec![other.clone()],
        })
        .collect()
}

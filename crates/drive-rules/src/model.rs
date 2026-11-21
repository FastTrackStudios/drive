//! What a rule is: where it runs (scope), when (trigger), which files it
//! takes (conditions) and what it does to them (actions, in order).
//!
//! Places are never absolute paths: a location (on whichever device has
//! it) and a path inside it, a folder of the Drive tree, a project, a tag.
//! So one rule means the same thing on every device.

use serde::{Deserialize, Serialize};

/// Whether a rule only says what it would do, or does it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Mode {
    /// Every new rule: the agents work out what it would do and show it;
    /// nothing changes.
    #[default]
    DryRun,
    /// Switched on by a person, after looking at the dry run.
    Live,
}

impl Mode {
    pub fn label(self) -> &'static str {
        match self {
            Mode::DryRun => "Dry run",
            Mode::Live => "Live",
        }
    }
}

/// A rule.
#[derive(Clone, Debug, PartialEq, Default, Serialize, Deserialize)]
pub struct Rule {
    pub id: String,
    pub name: String,
    /// Off: not run at all (not even as a dry run).
    pub enabled: bool,
    pub mode: Mode,
    pub scope: Scope,
    pub trigger: Trigger,
    pub when: When,
    pub actions: Vec<Action>,
    /// Milliseconds; who made it.
    pub created: i64,
    pub by: String,
}

/// Which devices run a rule, and over which places.
#[derive(Clone, Debug, PartialEq, Default, Serialize, Deserialize)]
pub struct Scope {
    pub devices: Devices,
    /// Empty: everything (every location of the devices in scope, and the
    /// Drive tree).
    pub places: Vec<Place>,
}

#[derive(Clone, Debug, PartialEq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", tag = "by", content = "list")]
pub enum Devices {
    #[default]
    All,
    /// Devices with one of these roles (`workstation`, `storage-server`).
    Roles(Vec<String>),
    /// These devices, by id.
    Named(Vec<String>),
}

/// A place a rule looks in.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", tag = "kind")]
pub enum Place {
    /// A location (a folder a device indexes), or a folder inside it.
    Location { location: String, path: String },
    /// A folder of the Drive tree (`""`: all of it).
    Drive { path: String },
    /// Every part of a project.
    Project { project: String },
    /// Everything carrying a tag (or a tag under it).
    Tag { tag: String },
}

/// When a rule runs. "Run now" (from the app or `drive-agent rules run`)
/// works whatever this says.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Trigger {
    /// When a file in scope changes (the agent's watcher; the Drive tree's
    /// changes on the node).
    pub on_change: bool,
    /// Every N minutes (0: not on a schedule).
    pub every_minutes: u32,
}

impl Default for Trigger {
    fn default() -> Self {
        Trigger {
            on_change: true,
            every_minutes: 0,
        }
    }
}

/// The conditions: groups, each all-of or any-of its conditions, and the
/// groups taken all-of or any-of. No groups: every file in scope matches.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct When {
    pub all: bool,
    pub groups: Vec<Group>,
}

impl Default for When {
    fn default() -> Self {
        When {
            all: true,
            groups: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Group {
    pub all: bool,
    pub conditions: Vec<Condition>,
}

impl Default for Group {
    fn default() -> Self {
        Group {
            all: true,
            conditions: Vec::new(),
        }
    }
}

/// One test, or its opposite.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Condition {
    #[serde(default)]
    pub not: bool,
    pub test: Test,
}

impl Condition {
    pub fn is(test: Test) -> Condition {
        Condition { not: false, test }
    }

    pub fn not(test: Test) -> Condition {
        Condition { not: true, test }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", tag = "test")]
pub enum Test {
    /// The file's name (with its extension).
    Name {
        text: Text,
    },
    /// Its path inside its location or the Drive.
    Path {
        text: Text,
    },
    /// Its extension is one of these (no dot, any case).
    Extension {
        any: Vec<String>,
    },
    /// Its Drive kind (`audio`); `family`: or a kind under it (MIDI under audio).
    Kind {
        kind: String,
        family: bool,
    },
    Size {
        cmp: Cmp,
        bytes: u64,
    },
    Date {
        of: DateOf,
        range: Range,
    },
    /// It carries this tag, or one under it.
    Tag {
        tag: String,
    },
    /// It has a field of this name whose value matches.
    Field {
        name: String,
        text: Text,
    },
    /// It is inside a part of this project (`""`: of any project).
    Project {
        project: String,
    },
    /// How many copies of its content Drive knows of (every location).
    Copies {
        cmp: Cmp,
        n: u32,
    },
    /// This device holds a copy of it (a location of its, or its cache).
    HeldBy {
        device: String,
    },
}

/// Text to compare against. Case is ignored, except in a regular
/// expression (write `(?i)` there to ignore it).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Text {
    pub op: TextOp,
    pub value: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TextOp {
    Is,
    Contains,
    StartsWith,
    EndsWith,
    Matches,
}

impl TextOp {
    pub const ALL: [TextOp; 5] = [
        TextOp::Contains,
        TextOp::Is,
        TextOp::StartsWith,
        TextOp::EndsWith,
        TextOp::Matches,
    ];

    pub fn id(self) -> &'static str {
        match self {
            TextOp::Is => "is",
            TextOp::Contains => "contains",
            TextOp::StartsWith => "starts-with",
            TextOp::EndsWith => "ends-with",
            TextOp::Matches => "matches",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            TextOp::Is => "is",
            TextOp::Contains => "contains",
            TextOp::StartsWith => "starts with",
            TextOp::EndsWith => "ends with",
            TextOp::Matches => "matches (regex)",
        }
    }

    pub fn parse(s: &str) -> TextOp {
        TextOp::ALL
            .into_iter()
            .find(|o| o.id() == s)
            .unwrap_or(TextOp::Contains)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Cmp {
    Less,
    More,
    Exactly,
}

impl Cmp {
    pub const ALL: [Cmp; 3] = [Cmp::More, Cmp::Less, Cmp::Exactly];

    pub fn id(self) -> &'static str {
        match self {
            Cmp::Less => "less",
            Cmp::More => "more",
            Cmp::Exactly => "exactly",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Cmp::Less => "less than",
            Cmp::More => "more than",
            Cmp::Exactly => "exactly",
        }
    }

    pub fn parse(s: &str) -> Cmp {
        Cmp::ALL
            .into_iter()
            .find(|o| o.id() == s)
            .unwrap_or(Cmp::More)
    }

    pub fn holds(self, a: u64, b: u64) -> bool {
        match self {
            Cmp::Less => a < b,
            Cmp::More => a > b,
            Cmp::Exactly => a == b,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum DateOf {
    Created,
    Modified,
    /// When Drive first saw it (its first index).
    Added,
}

impl DateOf {
    pub const ALL: [DateOf; 3] = [DateOf::Modified, DateOf::Created, DateOf::Added];

    pub fn id(self) -> &'static str {
        match self {
            DateOf::Created => "created",
            DateOf::Modified => "modified",
            DateOf::Added => "added",
        }
    }

    pub fn parse(s: &str) -> DateOf {
        DateOf::ALL
            .into_iter()
            .find(|o| o.id() == s)
            .unwrap_or(DateOf::Modified)
    }
}

/// A date range, relative (to now) or fixed.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", tag = "range")]
pub enum Range {
    /// In the last N units.
    InLast { n: u32, unit: Unit },
    /// More than N units ago.
    OlderThan { n: u32, unit: Unit },
    /// Before a day (`YYYY-MM-DD`, UTC).
    Before { day: String },
    /// On or after a day.
    After { day: String },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Unit {
    Minutes,
    Hours,
    Days,
    Weeks,
    /// Thirty days.
    Months,
    /// 365 days.
    Years,
}

impl Unit {
    pub const ALL: [Unit; 6] = [
        Unit::Minutes,
        Unit::Hours,
        Unit::Days,
        Unit::Weeks,
        Unit::Months,
        Unit::Years,
    ];

    pub fn ms(self) -> i64 {
        const MIN: i64 = 60_000;
        match self {
            Unit::Minutes => MIN,
            Unit::Hours => 60 * MIN,
            Unit::Days => 24 * 60 * MIN,
            Unit::Weeks => 7 * 24 * 60 * MIN,
            Unit::Months => 30 * 24 * 60 * MIN,
            Unit::Years => 365 * 24 * 60 * MIN,
        }
    }

    pub fn id(self) -> &'static str {
        match self {
            Unit::Minutes => "minutes",
            Unit::Hours => "hours",
            Unit::Days => "days",
            Unit::Weeks => "weeks",
            Unit::Months => "months",
            Unit::Years => "years",
        }
    }

    pub fn parse(s: &str) -> Unit {
        Unit::ALL
            .into_iter()
            .find(|o| o.id() == s)
            .unwrap_or(Unit::Days)
    }
}

/// What a rule does to each file it matches, in order. Templates (a new
/// name, a folder, a field's value) may use `{name}` (without the
/// extension), `{ext}`, `{year}`, `{month}`, `{day}` (of its creation, or
/// its modification when that is unknown), `{kind}`, `{project}` and
/// `{device}`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", tag = "do")]
pub enum Action {
    AddTags {
        tags: Vec<String>,
    },
    RemoveTags {
        tags: Vec<String>,
    },
    SetField {
        name: String,
        value: String,
    },
    /// Make it a part of a project (a file of a computer's location).
    AddToProject {
        project: String,
    },
    /// Copy it to a place: a folder of a location (this device's or
    /// another's, through the node) or of the Drive. `name`: a template,
    /// empty to keep its name. A copy never overwrites: a name that is
    /// taken gets " (2)", " (3)", ….
    Copy {
        to: Dest,
        name: String,
    },
    /// Move it (gated: see the safety rules in docs/rules.md).
    Move {
        to: Dest,
        name: String,
    },
    /// Rename it where it is, from a template (gated like a move).
    Rename {
        name: String,
    },
    /// Make its sidecars now.
    Sidecars {
        thumbnail: bool,
        waveform: bool,
    },
    /// Keep it highly available (on the node, and the public storage when
    /// one is set up, #13).
    HighlyAvailable,
    /// Into the Drive's Trash. Only for files of the Drive tree: never
    /// offered for files on a computer.
    Trash,
}

/// Where a copy or a move goes. `path` is a template too
/// (`Sorted/{year}/{month}`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", tag = "kind")]
pub enum Dest {
    Location { location: String, path: String },
    Drive { path: String },
}

impl Action {
    /// A short word for the journal and the app.
    pub fn kind(&self) -> &'static str {
        match self {
            Action::AddTags { .. } => "tag",
            Action::RemoveTags { .. } => "untag",
            Action::SetField { .. } => "field",
            Action::AddToProject { .. } => "project",
            Action::Copy { .. } => "copy",
            Action::Move { .. } => "move",
            Action::Rename { .. } => "rename",
            Action::Sidecars { .. } => "sidecars",
            Action::HighlyAvailable => "available",
            Action::Trash => "trash",
        }
    }
}

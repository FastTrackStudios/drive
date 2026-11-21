//! The file browser: every place a space keeps files, in one window.
//!
//! The sidebar lists the places: the Drive (the space's own synced tree),
//! each computer and its locations (from their indexes, so a computer that
//! is off can still be browsed), storages, kinds (all audio, every sample
//! instrument, …), and the apps' documents. The middle shows a place as a
//! grid or a list; the inspector shows the picked item: what it is, where
//! its copies are, and what can be done with it.

use dioxus::prelude::*;
use drive_dioxus::ui::{Avatars, ConnectionStatus};
use drive_dioxus::{Doc, use_doc, use_docs};
use drive_files::{files_doc, space, space_doc, tree};
use drive_spine as spine;
use drive_spine::kinds::{Kind, Kinds};
use serde_json::Value;

const ICONS: Asset = asset!("/assets/icons");

/// An icon of the set, by name (`Audio`, `Folder`, `Laptop`, …).
pub fn icon(name: &str) -> String {
    format!("{ICONS}/{name}.webp")
}

// ── navigation ──────────────────────────────────────────────────────────

/// Where the browser is.
#[derive(Clone, Debug, PartialEq)]
pub enum Place {
    Overview,
    /// The space's synced tree, at a folder (`None`: its root).
    Drive(Option<String>),
    /// A location's index, at a path inside it.
    Location {
        loc: String,
        path: String,
    },
    /// Every file of a kind, on every computer.
    Kind(String),
    /// An app's documents.
    App(String),
    /// The files a policy finds short of copies.
    AtRisk(String),
    /// A project: its parts, wherever they are, and its documents.
    Project(String),
    /// Every org you are in, at a glance.
    AllOrgs,
    Protection,
    Devices,
    Inbox,
    Calendar,
    Notes,
    Everything,
}

#[derive(Clone, Copy, PartialEq)]
pub enum View {
    Grid,
    List,
}

#[derive(Clone, Copy, PartialEq)]
pub enum Sort {
    Name,
    Size,
    Modified,
}

/// The browser's state, shared by its parts.
#[derive(Clone, Copy)]
pub struct Nav {
    pub place: Signal<Place>,
    back: Signal<Vec<Place>>,
    fwd: Signal<Vec<Place>>,
    pub selected: Signal<Option<Item>>,
    pub query: Signal<String>,
    pub view: Signal<View>,
    pub sort: Signal<(Sort, bool)>,
    pub sidebar: Signal<bool>,
    pub inspector: Signal<bool>,
    pub debug: Signal<bool>,
    /// Another org whose project (shared with this one) is being viewed;
    /// `None`: this org.
    pub within: Signal<Option<String>>,
}

impl Nav {
    fn new() -> Nav {
        Nav {
            place: Signal::new(Place::Drive(None)),
            back: Signal::new(Vec::new()),
            fwd: Signal::new(Vec::new()),
            selected: Signal::new(None),
            query: Signal::new(String::new()),
            view: Signal::new(View::Grid),
            sort: Signal::new((Sort::Name, false)),
            sidebar: Signal::new(false),
            inspector: Signal::new(true),
            debug: Signal::new(false),
            within: Signal::new(None),
        }
    }

    pub fn go(mut self, p: Place) {
        if *self.place.peek() != p {
            let cur = self.place.peek().clone();
            self.back.write().push(cur);
            self.fwd.write().clear();
            self.place.set(p);
        }
        self.selected.set(None);
        self.query.set(String::new());
        self.sidebar.set(false);
    }

    /// Go somewhere in this org (the sidebar: leaves any other org's project).
    pub fn go_home(mut self, p: Place) {
        self.go(p);
        self.within.set(None);
    }

    /// Go somewhere in another org (a project it shared with this one);
    /// moving around inside it (its parts, their folders) stays there.
    pub fn go_in(mut self, org: &str, p: Place) {
        self.go(p);
        self.within.set(Some(org.to_string()));
    }

    fn step(mut self, forward: bool) {
        let (mut from, mut to) = if forward {
            (self.fwd, self.back)
        } else {
            (self.back, self.fwd)
        };
        let Some(p) = from.write().pop() else { return };
        let cur = self.place.peek().clone();
        to.write().push(cur);
        self.place.set(p);
        self.selected.set(None);
    }
}

pub fn use_nav() -> Nav {
    use_context::<Nav>()
}

// ── items ───────────────────────────────────────────────────────────────

/// Where an item comes from.
#[derive(Clone, Debug, PartialEq)]
pub enum Src {
    /// The space's synced tree.
    Tree { id: String, hash: String },
    /// A location's index on the node.
    Index {
        location: String,
        path: String,
        sampled: String,
    },
    /// An app's document.
    Doc { id: String, peers: u32 },
}

/// A file, folder or document, as the browser shows it.
#[derive(Clone, Debug, PartialEq)]
pub struct Item {
    pub name: String,
    pub dir: bool,
    pub size: u64,
    pub mtime: i64,
    pub kind: Kind,
    pub src: Src,
    /// Where it is, for results from many places: `Studio PC · Samples · /Drums`.
    pub at: String,
    /// Whether its bytes can be read now (a holder is online).
    pub online: bool,
    pub copies: usize,
    /// For at-risk files: what it has (`copies`, `media`, `offsite`).
    pub kept: Option<Value>,
    /// Its bytes on the node (their hash): it can be had, and shared,
    /// while its computer is off.
    pub available: Option<String>,
}

impl Item {
    fn key(&self) -> String {
        match &self.src {
            Src::Tree { id, .. } => format!("t{id}"),
            Src::Index { location, path, .. } => format!("i{location}/{path}"),
            Src::Doc { id, .. } => format!("d{id}"),
        }
    }
}

pub fn s(v: &Value, k: &str) -> String {
    v[k].as_str().unwrap_or_default().to_string()
}

fn parent_of(path: &str) -> &str {
    path.rsplit_once('/').map_or("", |(p, _)| p)
}

/// A hit from the node's catalog (search, a folder, at-risk files).
pub fn hit_item(h: &Value, kinds: &Kinds) -> Item {
    let name = s(h, "name");
    let dir = h["dir"].as_bool() == Some(true);
    let path = s(h, "path");
    Item {
        kind: kinds.of_file(&name, dir).clone(),
        size: h["size"].as_u64().unwrap_or(0),
        mtime: h["mtime"].as_i64().unwrap_or(0),
        at: format!(
            "{} · {} · /{}",
            s(h, "device_name"),
            s(h, "location_name"),
            parent_of(&path)
        ),
        online: h["online"].as_bool() == Some(true),
        copies: h["copies"].as_u64().unwrap_or(1) as usize,
        kept: h.get("kept").cloned().filter(|v| !v.is_null()),
        available: h["available"].as_str().map(str::to_string),
        src: Src::Index {
            location: s(h, "location"),
            path,
            sampled: s(h, "sampled"),
        },
        name,
        dir,
    }
}

/// Devices whose agent is connected now (its presence in the registry).
pub fn online(reg: &Doc) -> Vec<String> {
    reg.others()
        .into_iter()
        .filter_map(|p| {
            p.data
                .get("device")
                .and_then(|d| d.as_str())
                .map(str::to_string)
        })
        .collect()
}

fn tree_item(
    e: &tree::Entry,
    path: &str,
    reg: &loro::LoroDoc,
    up: &[String],
    kinds: &Kinds,
) -> Item {
    let holders = if e.dir {
        Vec::new()
    } else {
        space::holders(reg, &e.hash)
    };
    Item {
        kind: kinds.of_file(&e.name, e.dir).clone(),
        size: e.size,
        mtime: e.mtime,
        at: format!("Drive · /{}", parent_of(path)),
        online: holders.iter().any(|(w, _)| w == "node" || up.contains(w)),
        copies: holders.len(),
        kept: None,
        available: holders
            .iter()
            .any(|(w, _)| w == "node")
            .then(|| e.hash.clone()),
        src: Src::Tree {
            id: tree::id_string(e.id),
            hash: e.hash.clone(),
        },
        name: e.name.clone(),
        dir: e.dir,
    }
}

// ── helpers ─────────────────────────────────────────────────────────────

pub async fn get(url: String) -> Option<Value> {
    let mut req = gloo_net::http::Request::get(&url);
    if let Some(b) = drive_dioxus::auth::bearer() {
        req = req.header("authorization", &b);
    }
    let r = req.send().await.ok()?;
    if !r.ok() {
        return None;
    }
    serde_json::from_str(&r.text().await.ok()?).ok()
}

pub fn enc(s: &str) -> String {
    js_sys::encode_uri_component(s).into()
}

pub fn human(n: u64) -> String {
    match n {
        0..1024 => format!("{n} B"),
        1024..1_048_576 => format!("{:.1} KB", n as f64 / 1024.0),
        1_048_576..1_073_741_824 => format!("{:.1} MB", n as f64 / 1_048_576.0),
        1_073_741_824..1_099_511_627_776 => format!("{:.1} GB", n as f64 / 1_073_741_824.0),
        _ => format!("{:.2} TB", n as f64 / 1_099_511_627_776.0),
    }
}

pub fn date(ms: i64) -> String {
    if ms <= 0 {
        return String::new();
    }
    let d = js_sys::Date::new(&(ms as f64).into());
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    format!(
        "{} {}, {}",
        MONTHS[d.get_month() as usize],
        d.get_date(),
        d.get_full_year()
    )
}

pub fn app_label(app: &str) -> String {
    match app {
        "studio" => "Studio".into(),
        "meals" => "Meals".into(),
        "frame" => "Frame".into(),
        "drive" => "Drive".into(),
        other => {
            let mut c = other.chars();
            c.next().map_or(String::new(), |f| {
                f.to_uppercase().collect::<String>() + c.as_str()
            })
        }
    }
}

/// A device's icon: a server, a phone, a laptop or a desktop.
fn device_icon(d: &space::Device) -> &'static str {
    let name = d.name.to_lowercase();
    if drive_files::placement::Role::of(&d.roles) == drive_files::placement::Role::StorageServer
        || d.kind == "server"
    {
        "Server"
    } else if d.kind == "phone" || name.contains("iphone") || name.contains("phone") {
        "Mobile"
    } else if d.kind == "tablet" || name.contains("ipad") {
        "Tablet"
    } else if name.contains("laptop") || name.contains("macbook") || name.contains("voyager") {
        "Laptop"
    } else {
        "PC"
    }
}

/// A kind's name for many of it (`Charts`; `Audio` stays `Audio`).
pub fn plural(name: &str) -> String {
    match name {
        "Audio" | "MIDI" | "Code" => name.to_string(),
        _ => format!("{name}s"),
    }
}

/// A project's icon, by its schema.org type.
pub fn project_icon(kind: &str) -> &'static str {
    match kind {
        "MusicRecording" | "MusicComposition" | "MusicAlbum" => "Audio",
        "Movie" | "VideoObject" => "Video",
        "Collection" => "Collection",
        _ => "Package",
    }
}

/// The kinds shown in the sidebar, in order.
const SIDEBAR_KINDS: [&str; 7] = [
    "audio",
    "instrument",
    "project",
    "chart",
    "video",
    "image",
    "document",
];

/// A colour per top-level kind, for the overview's breakdown.
pub fn kind_hue(id: &str) -> u32 {
    match id {
        "audio" => 160,
        "instrument" => 290,
        "project" => 25,
        "chart" => 85,
        "video" => 265,
        "image" => 220,
        "document" => 60,
        "code" => 0,
        "archive" => 245,
        "folder" => 210,
        _ => 300,
    }
}

// ── the shell ───────────────────────────────────────────────────────────

#[component]
pub fn Browser() -> Element {
    use_context_provider(Nav::new);
    let ctx = drive_dioxus::use_drive();
    // The space is the org this browser is in; the others the node keeps
    // can be switched to.
    let orgs = use_resource(|| async {
        get("/orgs".to_string())
            .await
            .and_then(|v| v["orgs"].as_array().cloned())
            .unwrap_or_default()
            .iter()
            .filter_map(|o| o.as_str().map(str::to_string))
            .collect::<Vec<String>>()
    });
    let space = ctx.org.clone();
    let mut list = orgs().unwrap_or_default();
    if !list.contains(&space) {
        list.push(space.clone());
    }
    let switch = ctx.clone();
    rsx! {
        Shell { space: space.clone(), spaces: list, on_space: move |s: String| switch.set_org(&s) }
    }
}

#[component]
fn Shell(space: String, spaces: Vec<String>, on_space: EventHandler<String>) -> Element {
    let nav = use_nav();
    // A project another org shared is shown in its own org's registry.
    let view = (nav.within)().unwrap_or_else(|| space.clone());
    let open = (nav.sidebar)();
    let has_sel = (nav.selected)().is_some();
    let shell = format!(
        "shell{}{}{}",
        if open { " drawer" } else { "" },
        if (nav.inspector)() { " inspecting" } else { "" },
        if has_sel { " picked" } else { "" }
    );
    rsx! {
        div { class: "{shell}",
            div { class: "scrim", onclick: move |_| nav.sidebar.clone().set(false) }
            Sidebar { space: space.clone(), spaces, on_space }
            div { class: "main",
                Toolbar {}
                div { class: "content",
                    Content { key: "{view}", space: view.clone() }
                }
            }
            Inspector { key: "{view}", space: view.clone() }
        }
    }
}

#[component]
fn Sidebar(space: String, spaces: Vec<String>, on_space: EventHandler<String>) -> Element {
    let nav = use_nav();
    let reg = use_doc(space_doc(&space));
    let spine_doc = use_doc(spine::SPINE);
    let docs = use_docs("");
    let r = reg.loro();
    let devices = space::devices(&r);
    let locations = space::locations(&r);
    let storages = space::storages(&r);
    let up = online(&reg);
    let kinds = Kinds::load(&spine_doc.loro());
    let projects = drive_files::projects::projects(&r);
    let mut new_project = use_signal(String::new);
    let place = (nav.place)();
    let mut apps: Vec<String> = docs()
        .iter()
        .filter_map(|d| d.id.split(':').next().map(str::to_string))
        .filter(|a| a != "drive")
        .collect();
    apps.sort();
    apps.dedup();
    let here = place.clone();
    let item = move |p: Place, label: String, ic: String, extra: Option<(bool, String)>| {
        let on = here == p;
        rsx! {
            button {
                class: if on { "side on" } else { "side" },
                onclick: move |_| nav.go_home(p.clone()),
                img { class: "sicon", src: icon(&ic), alt: "" }
                span { class: "slabel", "{label}" }
                if let Some((live, note)) = extra {
                    if !note.is_empty() {
                        span { class: "snote", "{note}" }
                    }
                    span { class: if live { "dot on" } else { "dot" } }
                }
            }
        }
    };
    rsx! {
        aside { class: "sidebar",
            div { class: "brand",
                span { class: "mark", "D" }
                b { "Drive" }
                if spaces.len() > 1 {
                    select {
                        class: "spacepick",
                        onchange: move |e| {
                            if e.value() == "*" {
                                nav.go_home(Place::AllOrgs);
                            } else {
                                on_space.call(e.value());
                            }
                        },
                        for sp in spaces.iter() {
                            option { key: "{sp}", value: "{sp}", selected: *sp == space && place != Place::AllOrgs, "{sp}" }
                        }
                        option { value: "*", selected: place == Place::AllOrgs, "All orgs" }
                    }
                }
            }
            nav { class: "places",
                {item(Place::Overview, "Overview".into(), "Globe".into(), None)}
                {item(Place::Drive(None), if space.is_empty() { "Drive".into() } else { space.clone() }, "Home".into(), None)}
                {item(Place::AllOrgs, "All orgs".into(), "Devices".into(), None)}
                h3 { "Projects" }
                for p in projects.iter() {
                    Fragment { key: "{p.id}",
                        {item(Place::Project(p.id.clone()), p.name.clone(), project_icon(&p.kind).into(), None)}
                    }
                }
                SharedWithUs { org: space.clone() }
                input {
                    class: "sidenew",
                    placeholder: "+ New project",
                    value: "{new_project}",
                    oninput: move |e| new_project.set(e.value()),
                    onkeydown: {
                        let reg = reg.clone();
                        move |e: KeyboardEvent| {
                            let name = new_project().trim().to_string();
                            if e.key().to_string() == "Enter" && !name.is_empty() {
                                let id = reg.edit(|d| drive_files::projects::add_project(d, &name, "Project").ok());
                                new_project.set(String::new());
                                if let Some(id) = id {
                                    nav.go_home(Place::Project(id));
                                }
                            }
                        }
                    },
                }
                if !devices.is_empty() {
                    h3 { "Computers" }
                }
                for d in devices.iter() {
                    {
                        let live = up.contains(&d.id);
                        let locs: Vec<&space::Location> = locations.iter().filter(|l| l.device == d.id && l.storage.is_empty()).collect();
                        let first = locs.first().map(|l| Place::Location { loc: l.id.clone(), path: String::new() });
                        rsx! {
                            div { key: "{d.id}", class: "devgroup",
                                {item(first.unwrap_or(Place::Devices), d.name.clone(), device_icon(d).into(), Some((live, String::new())))}
                                for l in locs {
                                    {
                                        let p = Place::Location { loc: l.id.clone(), path: String::new() };
                                        let on = matches!(&place, Place::Location { loc, .. } if *loc == l.id);
                                        let ic = if l.mirror_of.is_empty() { "Location" } else { "LocationReplica" };
                                        rsx! {
                                            button {
                                                key: "{l.id}",
                                                class: if on { "side sub on" } else { "side sub" },
                                                onclick: move |_| nav.go_home(p.clone()),
                                                img { class: "sicon", src: icon(ic), alt: "" }
                                                span { class: "slabel", "{l.name}" }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
                if !storages.is_empty() {
                    h3 { "Storages" }
                }
                for st in storages.iter() {
                    {
                        let live = up.contains(&st.device);
                        let locs: Vec<&space::Location> = locations.iter().filter(|l| l.storage == st.id).collect();
                        let ic = match st.scheme.as_str() {
                            "s3" => "Drive-AmazonS3",
                            "webdav" => "Drive-DAV",
                            _ => "HDD",
                        };
                        let first = locs.first().map(|l| Place::Location { loc: l.id.clone(), path: String::new() });
                        rsx! {
                            div { key: "{st.id}", class: "devgroup",
                                {item(first.unwrap_or(Place::Devices), st.name.clone(), ic.into(), Some((live, String::new())))}
                                for l in locs {
                                    {
                                        let p = Place::Location { loc: l.id.clone(), path: String::new() };
                                        let on = matches!(&place, Place::Location { loc, .. } if *loc == l.id);
                                        rsx! {
                                            button {
                                                key: "{l.id}",
                                                class: if on { "side sub on" } else { "side sub" },
                                                onclick: move |_| nav.go_home(p.clone()),
                                                img { class: "sicon", src: icon("LocationReplica"), alt: "" }
                                                span { class: "slabel", "{l.name}" }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
                h3 { "Kinds" }
                for k in SIDEBAR_KINDS {
                    {
                        let kind = kinds.get(k);
                        rsx! {
                            Fragment { key: "{k}",
                                {item(Place::Kind(k.into()), plural(&kind.name), kind.icon.clone(), None)}
                            }
                        }
                    }
                }
                if !apps.is_empty() {
                    h3 { "Apps" }
                }
                for a in apps {
                    Fragment { key: "{a}",
                        {item(Place::App(a.clone()), app_label(&a), "Widget".into(), None)}
                    }
                }
                h3 { "Space" }
                {item(Place::Protection, "Protection".into(), "Lock".into(), None)}
                {item(Place::Devices, "Devices & storage".into(), "Devices".into(), None)}
                h3 { "Spine" }
                {item(Place::Inbox, "Inbox".into(), "Document".into(), None)}
                {item(Place::Calendar, "Calendar".into(), "Book".into(), None)}
                {item(Place::Notes, "Notes".into(), "Text_txt".into(), None)}
                if (nav.debug)() {
                    {item(Place::Everything, "Raw documents".into(), "Database".into(), None)}
                }
            }
        }
    }
}

fn title_of(p: &Place) -> &'static str {
    match p {
        Place::Overview => "Overview",
        Place::Drive(_) => "Drive",
        Place::Location { .. } => "Computers",
        Place::Kind(_) => "Kind",
        Place::App(_) => "App",
        Place::AtRisk(_) => "At risk",
        Place::Project(_) => "Project",
        Place::AllOrgs => "All orgs",
        Place::Protection => "Protection",
        Place::Devices => "Devices & storage",
        Place::Inbox => "Inbox",
        Place::Calendar => "Calendar",
        Place::Notes => "Notes",
        Place::Everything => "Raw documents",
    }
}

fn browsable(p: &Place) -> bool {
    matches!(
        p,
        Place::Drive(_)
            | Place::Location { .. }
            | Place::Kind(_)
            | Place::App(_)
            | Place::AtRisk(_)
            | Place::Project(_)
    )
}

#[component]
fn Toolbar() -> Element {
    let mut nav = use_nav();
    let spine_doc = use_doc(spine::SPINE);
    let place = (nav.place)();
    let files = browsable(&place) || place == Place::Overview || !(nav.query)().is_empty();
    rsx! {
        header { class: "toolbar",
            button { class: "tb menu", title: "Places", onclick: move |_| nav.sidebar.set(true), "☰" }
            button { class: "tb", title: "Back", disabled: nav.back.read().is_empty(), onclick: move |_| nav.step(false), "‹" }
            button { class: "tb", title: "Forward", disabled: nav.fwd.read().is_empty(), onclick: move |_| nav.step(true), "›" }
            span { class: "tbtitle", "{title_of(&place)}" }
            if files {
                input {
                    class: "search",
                    r#type: "search",
                    placeholder: "Search every computer",
                    value: "{nav.query}",
                    oninput: move |e| {
                        nav.query.set(e.value());
                        nav.selected.set(None);
                    },
                }
                div { class: "seg",
                    button {
                        class: if (nav.view)() == View::Grid { "on" } else { "" },
                        title: "Grid",
                        onclick: move |_| nav.view.set(View::Grid),
                        "▦"
                    }
                    button {
                        class: if (nav.view)() == View::List { "on" } else { "" },
                        title: "List",
                        onclick: move |_| nav.view.set(View::List),
                        "☰"
                    }
                }
                select {
                    class: "sortpick",
                    title: "Sort",
                    onchange: move |e| {
                        let s = match e.value().as_str() {
                            "size" => (Sort::Size, true),
                            "modified" => (Sort::Modified, true),
                            _ => (Sort::Name, false),
                        };
                        nav.sort.set(s);
                    },
                    option { value: "name", "Name" }
                    option { value: "size", "Size" }
                    option { value: "modified", "Modified" }
                }
                button {
                    class: if (nav.inspector)() { "tb on wide" } else { "tb wide" },
                    title: "Details",
                    onclick: move |_| {
                        let v = !(nav.inspector)();
                        nav.inspector.set(v);
                    },
                    "ⓘ"
                }
            }
            div { class: "who",
                ConnectionStatus {}
                Avatars { doc: spine_doc.clone() }
            }
        }
    }
}

#[component]
fn Content(space: String) -> Element {
    let nav = use_nav();
    let place = (nav.place)();
    let q = (nav.query)();
    if space.is_empty() && browsable(&place) && !matches!(place, Place::App(_)) {
        return rsx! {
            div { class: "pane",
                p { class: "empty",
                    "No space yet. On a computer: "
                    code { "drive-agent init --node <this node>/sync --space home --name \"My PC\"" }
                }
            }
        };
    }
    if !q.trim().is_empty() && (browsable(&place) || place == Place::Overview) {
        return rsx! {
            Search { space, q }
        };
    }
    match place {
        Place::Overview => rsx! {
            crate::catalog::Overview { space }
        },
        Place::Drive(folder) => rsx! {
            DriveFolder { space, folder }
        },
        Place::Location { loc, path } => rsx! {
            LocationFolder { space, loc, path }
        },
        Place::Kind(kind) => rsx! {
            KindView { space, kind }
        },
        Place::App(app) => rsx! {
            AppDocs { app }
        },
        Place::AtRisk(policy) => rsx! {
            crate::catalog::AtRisk { space, policy }
        },
        Place::Project(id) => rsx! {
            crate::projects::ProjectView { space, id }
        },
        Place::AllOrgs => rsx! {
            crate::orgs::AllOrgs { here: space }
        },
        Place::Protection => rsx! {
            crate::catalog::Protection { space }
        },
        Place::Devices => rsx! {
            crate::files::DevicesPage { space }
        },
        Place::Inbox => rsx! {
            crate::Inbox {}
        },
        Place::Calendar => rsx! {
            crate::Calendar {}
        },
        Place::Notes => rsx! {
            crate::notes::Notes {}
        },
        Place::Everything => rsx! {
            crate::Everything {}
        },
    }
}

// ── the views ───────────────────────────────────────────────────────────

/// Items as a grid or a list, sorted, folders first.
#[component]
pub fn Items(items: Vec<Item>, show_at: bool, empty: String, loading: bool) -> Element {
    let mut nav = use_nav();
    let (sort, desc) = (nav.sort)();
    let mut items = items;
    items.sort_by(|a, b| {
        b.dir.cmp(&a.dir).then_with(|| {
            let o = match sort {
                Sort::Name => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
                Sort::Size => a.size.cmp(&b.size),
                Sort::Modified => a.mtime.cmp(&b.mtime),
            };
            if desc { o.reverse() } else { o }
        })
    });
    let sel = (nav.selected)().map(|i| i.key());
    let mut open = move |i: Item| match &i.src {
        Src::Tree { id, .. } if i.dir => nav.go(Place::Drive(Some(id.clone()))),
        Src::Index { location, path, .. } if i.dir => nav.go(Place::Location {
            loc: location.clone(),
            path: path.clone(),
        }),
        _ => nav.selected.set(Some(i)),
    };
    if items.is_empty() {
        return rsx! {
            p { class: "empty", if loading { "Loading…" } else { "{empty}" } }
        };
    }
    let grid = (nav.view)() == View::Grid;
    rsx! {
        div { class: if grid { "grid" } else { "list" },
            if !grid {
                div { class: "lhead",
                    span { class: "lname", "Name" }
                    span { class: "lkind", "Kind" }
                    span { class: "lsize", "Size" }
                    span { class: "ldate", "Modified" }
                    span { class: "lcopies", "Copies" }
                }
            }
            for i in items {
                {
                    let key = i.key();
                    let on = sel.as_deref() == Some(key.as_str());
                    let it = i.clone();
                    let it2 = i.clone();
                    let ic = icon(&i.kind.icon);
                    let copies = copies_label(&i);
                    rsx! {
                        div {
                            key: "{key}",
                            class: format!("{}{}{}", if grid { "cell" } else { "row" }, if on { " on" } else { "" }, if !i.online && i.available.is_none() && !i.dir && !matches!(i.src, Src::Doc { .. }) { " away" } else { "" }),
                            onclick: move |_| open(it.clone()),
                            oncontextmenu: move |e| {
                                e.prevent_default();
                                nav.selected.set(Some(it2.clone()));
                            },
                            img { class: "kicon", src: "{ic}", alt: "", draggable: false }
                            if grid {
                                span { class: "cname", "{i.name}" }
                                span { class: "csub",
                                    if i.dir { "{i.kind.name}" } else { "{human(i.size)}" }
                                }
                                if show_at {
                                    span { class: "cat", "{i.at}" }
                                }
                            } else {
                                span { class: "lname",
                                    span { class: "nm", "{i.name}" }
                                    if show_at {
                                        span { class: "cat", "{i.at}" }
                                    }
                                }
                                span { class: "lkind", "{i.kind.name}" }
                                span { class: "lsize", if !i.dir { "{human(i.size)}" } }
                                span { class: "ldate", "{date(i.mtime)}" }
                                span { class: "lcopies",
                                    if let Some((label, cls)) = copies {
                                        span { class: "chip {cls}", "{label}" }
                                    }
                                    if i.available.is_some() && matches!(i.src, Src::Index { .. }) {
                                        span { class: "chip any", title: "Kept on the node: reachable while its computer is off", "anywhere" }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

fn copies_label(i: &Item) -> Option<(String, &'static str)> {
    if i.dir || matches!(i.src, Src::Doc { .. }) {
        return None;
    }
    if let Some(k) = &i.kept {
        return Some((
            format!(
                "{} cop{} · {} offsite",
                k["copies"],
                if k["copies"] == 1 { "y" } else { "ies" },
                k["offsite"]
            ),
            "warn",
        ));
    }
    Some(match i.copies {
        0 => ("uploading".into(), "warn"),
        1 => ("1 copy".into(), "warn"),
        n => (format!("{n} copies"), "ok"),
    })
}

#[component]
fn Crumbs(parts: Vec<(String, Option<Place>)>) -> Element {
    let nav = use_nav();
    rsx! {
        div { class: "crumbs",
            for (i , (name , p)) in parts.into_iter().enumerate() {
                Fragment { key: "{i}",
                    if i > 0 {
                        span { class: "sep", "›" }
                    }
                    match p {
                        Some(p) => rsx! {
                            button { onclick: move |_| nav.go(p.clone()), "{name}" }
                        },
                        None => rsx! {
                            span { class: "crumbtext", "{name}" }
                        },
                    }
                }
            }
        }
    }
}

/// The space's synced tree at a folder: upload here, make folders.
#[component]
fn DriveFolder(space: String, folder: Option<String>) -> Element {
    let files = use_doc(files_doc(&space));
    let reg = use_doc(space_doc(&space));
    let spine_doc = use_doc(spine::SPINE);
    let mut status = use_signal(String::new);
    let mut new_folder = use_signal(String::new);
    let f = files.loro();
    let r = reg.loro();
    let kinds = Kinds::load(&spine_doc.loro());
    let up = online(&reg);
    let cur = folder.as_deref().and_then(tree::parse_id);
    let base = cur.map(|c| tree::path_of(&f, c)).unwrap_or_default();
    let items: Vec<Item> = tree::children(&f, cur)
        .iter()
        .map(|e| {
            tree_item(
                e,
                &if base.is_empty() {
                    e.name.clone()
                } else {
                    format!("{base}/{}", e.name)
                },
                &r,
                &up,
                &kinds,
            )
        })
        .collect();
    let mut crumbs: Vec<(String, Option<Place>)> = Vec::new();
    let mut c = cur;
    while let Some(id) = c {
        if let Some(e) = tree::entry(&f, id) {
            crumbs.push((e.name, Some(Place::Drive(Some(tree::id_string(id))))));
        }
        c = tree::parent_of(&f, id);
    }
    crumbs.push((space.clone(), Some(Place::Drive(None))));
    crumbs.reverse();

    let upload = {
        let files = files.clone();
        let reg = reg.clone();
        move |e: FormEvent| {
            let files = files.clone();
            let reg = reg.clone();
            async move {
                for file in e.files() {
                    let name = file.name();
                    status.set(format!("Uploading {name}…"));
                    let Ok(bytes) = file.read_bytes().await else {
                        continue;
                    };
                    let hash = drive_files::hash::of_bytes(&bytes);
                    let body = js_sys::Uint8Array::from(bytes.as_ref());
                    let mut put = gloo_net::http::Request::put(&format!("/blobs/{hash}"));
                    if let Some(b) = drive_dioxus::auth::bearer() {
                        put = put.header("authorization", &b);
                    }
                    let sent = put.body(body);
                    match sent {
                        Ok(req) => match req.send().await {
                            Ok(r) if r.ok() => {
                                files.edit(|d| {
                                    let id = match tree::lookup(d, cur, &name) {
                                        Some(x) if !x.dir => Some(x.id),
                                        Some(_) => None,
                                        None => tree::create_file(d, cur, &name).ok(),
                                    };
                                    if let Some(id) = id {
                                        let _ = tree::set_content(d, id, bytes.len() as u64, &hash);
                                    }
                                });
                                reg.edit(|d| {
                                    let _ = space::set_held(d, "node", &hash, Some("stored"));
                                });
                                status.set(format!("Uploaded {name}"));
                            }
                            _ => status.set(format!("The node refused {name}")),
                        },
                        Err(_) => status.set(format!("Could not send {name}")),
                    }
                }
            }
        }
    };
    rsx! {
        div { class: "viewhead",
            Crumbs { parts: crumbs }
            div { class: "actions",
                input {
                    class: "newfolder",
                    placeholder: "New folder",
                    value: "{new_folder}",
                    oninput: move |e| new_folder.set(e.value()),
                    onkeydown: {
                        let files = files.clone();
                        move |e: KeyboardEvent| {
                            if e.key().to_string() == "Enter" && !new_folder().trim().is_empty() {
                                let n = new_folder().trim().to_string();
                                files.edit(|d| {
                                    let _ = tree::mkdir(d, cur, &n);
                                });
                                new_folder.set(String::new());
                            }
                        }
                    },
                }
                label { class: "primary uploadbtn",
                    "Upload"
                    input { r#type: "file", multiple: true, onchange: upload }
                }
            }
        }
        if !status().is_empty() {
            p { class: "status", "{status}" }
        }
        Items {
            items,
            show_at: false,
            empty: "Empty. Upload here, or drop files into the Drive folder on a computer.",
            loading: !files.synced(),
        }
    }
}

/// A folder of a location's index (the computer may be off).
#[component]
fn LocationFolder(space: String, loc: String, path: String) -> Element {
    let reg = use_doc(space_doc(&space));
    let spine_doc = use_doc(spine::SPINE);
    let r = reg.loro();
    let kinds = Kinds::load(&spine_doc.loro());
    let locations = space::locations(&r);
    let devices = space::devices(&r);
    let storages = space::storages(&r);
    let l = locations.iter().find(|x| x.id == loc).cloned();
    // Refetched whenever the location's index changes.
    let index = l.as_ref().map(|l| l.index.clone()).unwrap_or_default();
    let (sp, lc, pt) = (space.clone(), loc.clone(), path.clone());
    let rows = use_resource(use_reactive!(|(sp, lc, pt, index)| async move {
        let _ = index;
        get(format!(
            "/list?space={}&location={}&path={}",
            enc(&sp),
            enc(&lc),
            enc(&pt)
        ))
        .await
    }));
    let items: Vec<Item> = rows()
        .flatten()
        .and_then(|v| v["entries"].as_array().cloned())
        .unwrap_or_default()
        .iter()
        .map(|h| hit_item(h, &kinds))
        .collect();
    let Some(l) = l else {
        return rsx! {
            p { class: "empty", "This location is gone." }
        };
    };
    let owner = if l.storage.is_empty() {
        devices
            .iter()
            .find(|d| d.id == l.device)
            .map_or(String::new(), |d| d.name.clone())
    } else {
        storages
            .iter()
            .find(|s| s.id == l.storage)
            .map_or(String::new(), |s| s.name.clone())
    };
    let holder_up = online(&reg).contains(&l.device);
    let mut crumbs = vec![
        (owner, None),
        (
            l.name.clone(),
            Some(Place::Location {
                loc: loc.clone(),
                path: String::new(),
            }),
        ),
    ];
    let mut acc = String::new();
    for p in path.split('/').filter(|p| !p.is_empty()) {
        acc = if acc.is_empty() {
            p.to_string()
        } else {
            format!("{acc}/{p}")
        };
        crumbs.push((
            p.to_string(),
            Some(Place::Location {
                loc: loc.clone(),
                path: acc.clone(),
            }),
        ));
    }
    let folder_name = path
        .rsplit('/')
        .next()
        .filter(|x| !x.is_empty())
        .map_or(l.name.clone(), str::to_string);
    rsx! {
        div { class: "viewhead",
            Crumbs { parts: crumbs }
            div { class: "actions",
                crate::projects::AddToProject { reg: reg.clone(), location: loc.clone(), path: path.clone(), label: folder_name }
                span { class: if holder_up { "badge on" } else { "badge" },
                    if holder_up { "online" } else { "offline · browsing its index" }
                }
                span { class: "count", "{l.files} files · {human(l.bytes.max(0) as u64)}" }
            }
        }
        Items {
            items,
            show_at: false,
            empty: if l.index.is_empty() { "Not indexed yet." } else { "Empty." },
            loading: rows().is_none(),
        }
    }
}

/// Every file of a kind, on every computer and in the Drive.
#[component]
fn KindView(space: String, kind: String) -> Element {
    let spine_doc = use_doc(spine::SPINE);
    let files = use_doc(files_doc(&space));
    let reg = use_doc(space_doc(&space));
    let kinds = Kinds::load(&spine_doc.loro());
    let exts = kinds.extensions_of(&kind).join(",");
    let (sp, ex) = (space.clone(), exts.clone());
    let rows = use_resource(use_reactive!(|(sp, ex)| async move {
        get(format!(
            "/search?space={}&exts={}&limit=500",
            enc(&sp),
            enc(&ex)
        ))
        .await
    }));
    let f = files.loro();
    let r = reg.loro();
    let up = online(&reg);
    let family: Vec<String> = kinds.family(&kind).iter().map(|k| k.id.clone()).collect();
    let mut items: Vec<Item> = tree::files(&f)
        .iter()
        .filter(|(_, e)| family.contains(&kinds.of_file(&e.name, e.dir).id))
        .map(|(p, e)| tree_item(e, p, &r, &up, &kinds))
        .collect();
    items.extend(
        rows()
            .flatten()
            .and_then(|v| v["results"].as_array().cloned())
            .unwrap_or_default()
            .iter()
            .map(|h| hit_item(h, &kinds)),
    );
    let k = kinds.get(&kind).clone();
    let n = items.len();
    rsx! {
        div { class: "viewhead",
            div { class: "kindtitle",
                img { src: icon(&k.icon), alt: "" }
                h1 { "{plural(&k.name)}" }
                span { class: "count", "{n} files" }
            }
        }
        Items {
            items,
            show_at: true,
            empty: format!("No {} files on any computer.", k.name.to_lowercase()),
            loading: rows().is_none(),
        }
    }
}

/// Search the Drive and every computer's index.
#[component]
fn Search(space: String, q: String) -> Element {
    let spine_doc = use_doc(spine::SPINE);
    let files = use_doc(files_doc(&space));
    let reg = use_doc(space_doc(&space));
    let kinds = Kinds::load(&spine_doc.loro());
    let (sp, qq) = (space.clone(), q.clone());
    let rows = use_resource(use_reactive!(|(sp, qq)| async move {
        get(format!(
            "/search?space={}&q={}&limit=200",
            enc(&sp),
            enc(qq.trim())
        ))
        .await
    }));
    let words: Vec<String> = q.split_whitespace().map(|w| w.to_lowercase()).collect();
    let f = files.loro();
    let r = reg.loro();
    let up = online(&reg);
    let mut items: Vec<Item> = tree::files(&f)
        .iter()
        .filter(|(_, e)| {
            let n = e.name.to_lowercase();
            words.iter().all(|w| n.contains(w))
        })
        .map(|(p, e)| tree_item(e, p, &r, &up, &kinds))
        .collect();
    items.extend(
        rows()
            .flatten()
            .and_then(|v| v["results"].as_array().cloned())
            .unwrap_or_default()
            .iter()
            .map(|h| hit_item(h, &kinds)),
    );
    rsx! {
        div { class: "viewhead",
            div { class: "kindtitle",
                img { src: icon("Search"), alt: "" }
                h1 { "“{q.trim()}”" }
                span { class: "count", "{items.len()} found" }
            }
        }
        Items {
            items,
            show_at: true,
            empty: "Nothing by that name in the Drive or on any computer.",
            loading: rows().is_none(),
        }
    }
}

/// An app's documents, each with its kind and a way to open it.
#[component]
fn AppDocs(app: String) -> Element {
    let spine_doc = use_doc(spine::SPINE);
    let docs = use_docs(format!("{app}:"));
    let kinds = Kinds::load(&spine_doc.loro());
    let items: Vec<Item> = docs()
        .iter()
        .map(|d| {
            let kind = kinds.of_doc(&d.id).clone();
            let name = if kind.doc_prefix.is_empty() {
                d.id.split_once(':')
                    .map_or(d.id.clone(), |(_, r)| r.replace(':', " · "))
            } else {
                d.id.trim_start_matches(&kind.doc_prefix).replace('-', " ")
            };
            Item {
                name,
                dir: false,
                size: d.bytes,
                mtime: 0,
                at: app_label(&app),
                online: true,
                copies: 1,
                kept: None,
                available: None,
                src: Src::Doc {
                    id: d.id.clone(),
                    peers: d.peers,
                },
                kind,
            }
        })
        .collect();
    let mut by_kind: Vec<String> = items.iter().map(|i| i.kind.name.clone()).collect();
    by_kind.sort();
    by_kind.dedup();
    rsx! {
        div { class: "viewhead",
            div { class: "kindtitle",
                img { src: icon("Widget"), alt: "" }
                h1 { "{app_label(&app)}" }
                span { class: "count", "{items.len()} documents · {by_kind.join(\", \")}" }
            }
        }
        Items { items, show_at: false, empty: "No documents yet.", loading: false }
    }
}

// ── the inspector ───────────────────────────────────────────────────────

#[component]
fn Inspector(space: String) -> Element {
    let mut nav = use_nav();
    let reg = use_doc(space_doc(&space));
    let Some(item) = (nav.selected)() else {
        return rsx! {
            aside { class: "inspector",
                p { class: "empty", "Pick a file to see what it is and where its copies are." }
            }
        };
    };
    let r = reg.loro();
    let up = online(&reg);
    let (where_, open_link, download) = match &item.src {
        Src::Tree { hash, .. } => (
            item.at.clone(),
            None,
            (!hash.is_empty()).then(|| format!("/blobs/{hash}?name={}", enc(&item.name))),
        ),
        Src::Index { location, path, .. } => (
            item.at.clone(),
            None,
            match &item.available {
                Some(h) => Some(format!("/blobs/{h}?name={}", enc(&item.name))),
                None => item.online.then(|| {
                    format!(
                        "/fetch/{}/{}?path={}",
                        enc(&space),
                        enc(location),
                        enc(path)
                    )
                }),
            },
        ),
        Src::Doc { id, .. } => (app_label(&item.kind.app), item.kind.link(id), None),
    };
    rsx! {
        aside { class: "inspector",
            div { class: "ihead",
                button { class: "x", title: "Close", onclick: move |_| nav.selected.set(None), "✕" }
            }
            img { class: "ibig", src: icon(&item.kind.icon), alt: "" }
            h2 { class: "iname", "{item.name}" }
            span { class: "ikind", "{item.kind.name}" }
            div { class: "iacts",
                if let Some(href) = open_link {
                    a { class: "primary", href: "{href}", "Open in {app_label(&item.kind.app)}" }
                }
                if let Some(href) = download.clone() {
                    a { class: "primary ghost", href: "{href}", "Download" }
                }
            }
            section { class: "isec",
                h4 { "Details" }
                Row { k: "Kind", v: item.kind.name.clone() }
                if !item.dir {
                    Row { k: "Size", v: human(item.size) }
                }
                if item.mtime > 0 {
                    Row { k: "Modified", v: date(item.mtime) }
                }
                Row { k: "Where", v: where_ }
                if let Some(h) = &item.available {
                    Row { k: "Reachable", v: "anywhere (kept on the node)".to_string() }
                    ShareLink { hash: h.clone(), name: item.name.clone() }
                }
                if let Src::Doc { peers, .. } = &item.src {
                    Row { k: "Open now", v: format!("{peers}") }
                }
                if let Src::Index { sampled, .. } = &item.src {
                    if !sampled.is_empty() {
                        Row { k: "Content", v: sampled.chars().take(12).collect::<String>() + "…" }
                    }
                }
            }
            if let Src::Doc { id, .. } = &item.src {
                crate::projects::DocProjects { reg: reg.clone(), doc_id: id.clone() }
            }
            match item.src.clone() {
                Src::Index { sampled, .. } if !item.dir => rsx! {
                    IndexCopies { space: space.clone(), sampled, size: item.size }
                },
                Src::Tree { id, hash } if !item.dir => rsx! {
                    TreeCopies { reg: reg.clone(), id, hash, up: up.clone(), devices: space::devices(&r) }
                },
                _ => rsx! {},
            }
            if !item.dir {
                ItemComments { item: item.clone(), download: download.clone() }
            }
            if let Some(k) = &item.kept {
                section { class: "isec warn",
                    h4 { "Short of its policy" }
                    Row { k: "Copies", v: k["copies"].to_string() }
                    Row { k: "Media", v: k["media"].to_string() }
                    Row { k: "Offsite", v: k["offsite"].to_string() }
                }
            }
        }
    }
}

/// A link to a file kept on the node, to copy and send.
#[component]
fn ShareLink(hash: String, name: String) -> Element {
    let origin = js_sys::eval("location.origin")
        .ok()
        .and_then(|v| v.as_string())
        .unwrap_or_default();
    let url = format!("{origin}/blobs/{hash}?name={}", enc(&name));
    let mut copied = use_signal(|| false);
    let u = url.clone();
    rsx! {
        div { class: "share",
            input { readonly: true, value: "{url}" }
            button {
                class: "primary ghost",
                onclick: move |_| {
                    let _ = js_sys::eval(&format!("navigator.clipboard.writeText({})", serde_json::Value::String(u.clone())));
                    copied.set(true);
                },
                if copied() { "Copied" } else { "Copy link" }
            }
        }
    }
}

#[component]
fn Row(k: String, v: String) -> Element {
    rsx! {
        div { class: "irow",
            span { class: "ik", "{k}" }
            span { class: "iv", "{v}" }
        }
    }
}

/// Copies of an indexed file: every location holding the same content.
#[component]
fn IndexCopies(space: String, sampled: String, size: u64) -> Element {
    let rows = use_resource(use_reactive!(|(space, sampled, size)| async move {
        get(format!(
            "/copies?space={}&sampled={}&size={size}",
            enc(&space),
            enc(&sampled)
        ))
        .await
    }));
    let copies = rows()
        .flatten()
        .and_then(|v| v["copies"].as_array().cloned())
        .unwrap_or_default();
    let mut devices: Vec<String> = copies.iter().map(|c| s(c, "device_name")).collect();
    devices.dedup();
    rsx! {
        section { class: "isec",
            h4 { "Copies · {copies.len()}" }
            if rows().is_none() {
                p { class: "hint", "Looking…" }
            }
            for (i , c) in copies.iter().enumerate() {
                div { key: "{i}", class: "copy",
                    span { class: if c["online"].as_bool() == Some(true) { "dot on" } else { "dot" } }
                    div { class: "cmain",
                        b { "{s(c, \"device_name\")} · {s(c, \"location_name\")}" }
                        span { "/{s(c, \"path\")}" }
                    }
                    if !s(c, "site").is_empty() {
                        span { class: "chip", "{s(c, \"site\")}" }
                    }
                }
            }
            p { class: "hint", "Found by content (a sampled hash and the size): likely the same bytes." }
        }
    }
}

/// Where a Drive file's bytes are, and keeping it on a device.
#[component]
fn TreeCopies(
    reg: Doc,
    id: String,
    hash: String,
    up: Vec<String>,
    devices: Vec<space::Device>,
) -> Element {
    let r = reg.loro();
    let holders = space::holders(&r, &hash);
    let storages = space::storages(&r);
    let name_of = |who: &str| -> String {
        if who == "node" {
            return "This node".into();
        }
        devices
            .iter()
            .find(|d| d.id == who)
            .map(|d| d.name.clone())
            .or_else(|| {
                storages
                    .iter()
                    .find(|s| s.id == who)
                    .map(|s| s.name.clone())
            })
            .unwrap_or_else(|| who.chars().take(6).collect())
    };
    rsx! {
        section { class: "isec",
            h4 { "Copies · {holders.len()}" }
            for (who , how) in holders.iter() {
                div { key: "{who}", class: "copy",
                    span { class: if who == "node" || up.contains(who) { "dot on" } else { "dot" } }
                    div { class: "cmain",
                        b { "{name_of(who)}" }
                        span { "{how}" }
                    }
                }
            }
        }
        section { class: "isec",
            h4 { "Keep offline on" }
            for d in devices.iter() {
                {
                    let on = space::pins(&r, &d.id).contains(&id);
                    let (reg, dev, file) = (reg.clone(), d.id.clone(), id.clone());
                    rsx! {
                        label { key: "{d.id}", class: "pin",
                            input {
                                r#type: "checkbox",
                                checked: on,
                                onchange: move |e: FormEvent| {
                                    let v = e.checked();
                                    reg.edit(|doc| {
                                        let _ = space::set_pin(doc, &dev, &file, v);
                                    });
                                },
                            }
                            img { class: "sicon", src: icon(device_icon(d)), alt: "" }
                            span { "{d.name}" }
                        }
                    }
                }
            }
            p { class: "hint", "That device fetches it and keeps it, even when it is short on room." }
        }
    }
}

/// Projects other orgs have shared with this one.
#[component]
fn SharedWithUs(org: String) -> Element {
    let nav = use_nav();
    let o = org.clone();
    let shared = use_resource(use_reactive!(|o| async move {
        get(format!("/shared?org={}", enc(&o)))
            .await
            .and_then(|v| v["shared"].as_array().cloned())
            .unwrap_or_default()
    }));
    let list = shared().unwrap_or_default();
    let within = (nav.within)();
    let place = (nav.place)();
    rsx! {
        for x in list {
            {
                let owner = s(&x, "org");
                let p = &x["project"];
                let (id, name, kind) = (s(p, "id"), s(p, "name"), s(p, "kind"));
                let on = within.as_deref() == Some(owner.as_str()) && place == Place::Project(id.clone());
                rsx! {
                    button {
                        key: "{owner}/{id}",
                        class: if on { "side on" } else { "side" },
                        title: "Shared by {owner}",
                        onclick: move |_| nav.go_in(&owner, Place::Project(id.clone())),
                        img { class: "sicon", src: icon(project_icon(&kind)), alt: "" }
                        span { class: "slabel", "{name}" }
                        span { class: "snote", "{s(&x, \"org\")}" }
                    }
                }
            }
        }
    }
}

/// An item's comments, with a player for audio and video so a note can
/// be pinned to a moment (and a moment played back).
#[component]
fn ItemComments(item: Item, download: Option<String>) -> Element {
    let (target, version) = match &item.src {
        Src::Tree { id, hash } => (
            drive_spine::comments::Target::File(id.clone()),
            hash.clone(),
        ),
        Src::Index {
            location,
            path,
            sampled,
        } => (
            drive_spine::comments::Target::Indexed {
                location: location.clone(),
                path: path.clone(),
            },
            sampled.clone(),
        ),
        Src::Doc { id, .. } => (
            drive_spine::comments::Target::Doc(id.clone()),
            String::new(),
        ),
    };
    let family = if item.kind.parent.is_empty() {
        item.kind.id.clone()
    } else {
        item.kind.parent.clone()
    };
    let media = matches!(family.as_str(), "audio" | "video") && download.is_some();
    let player = media.then(|| "drive-review-player".to_string());
    rsx! {
        section { class: "isec",
            if media {
                if family == "video" {
                    video { id: "drive-review-player", class: "iplayer", controls: true, preload: "none", src: download.clone().unwrap_or_default() }
                } else {
                    audio { id: "drive-review-player", class: "iplayer", controls: true, preload: "none", src: download.clone().unwrap_or_default() }
                }
            }
            drive_dioxus::comments::CommentsPanel { key: "{item.key()}", target, version, player }
        }
    }
}

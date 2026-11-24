//! The file browser: every place a space keeps files, in one window.
//!
//! The sidebar lists the places: the Drive (the space's own synced tree),
//! each computer and its locations (from their indexes, so a computer that
//! is off can still be browsed), storages, kinds (all audio, every sample
//! instrument, …), and the apps' documents. The middle shows a place as a
//! grid or a list; the inspector shows the picked item: what it is, where
//! its copies are, and what can be done with it.

use dioxus::prelude::*;
use drive_dioxus::{Doc, use_doc, use_docs};
use drive_files::{adopt, files_doc, space, space_doc, tree};
use drive_spine as spine;
use drive_spine::kinds::{Kind, Kinds};
use serde_json::Value;

const ICONS: Asset = asset!("/assets/icons");

/// An icon of the set, by name (`Audio`, `Folder`, `Laptop`, …).
pub fn icon(name: &str) -> String {
    format!("{ICONS}/{name}.webp")
}

pub use crate::nav::{Nav, Place, Sort, View, use_nav};
use crate::shell::browsable;

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
    /// A folder of the Drive tree by its path, where adoptions put one:
    /// an adopted folder, one inside it, or one only on the way to them.
    Logical { path: String },
    /// A file or folder on a computer's disk, browsed live.
    Browse {
        device: String,
        root: String,
        path: String,
    },
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
    pub fn key(&self) -> String {
        match &self.src {
            Src::Tree { id, .. } => format!("t{id}"),
            Src::Index { location, path, .. } => format!("i{location}/{path}"),
            Src::Doc { id, .. } => format!("d{id}"),
            Src::Browse { device, root, path } => format!("b{device}:{root}/{path}"),
            Src::Logical { path } => format!("l{path}"),
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

pub fn tree_item(
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
    let (status, bytes) = crate::host::request("GET", &url, None, None).await?;
    if !(200..300).contains(&status) {
        return None;
    }
    serde_json::from_slice(&bytes).ok()
}

pub fn enc(s: &str) -> String {
    crate::host::enc(s)
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
    let (month, day, year) = crate::host::date_parts(ms as f64);
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    format!("{} {day}, {year}", MONTHS[month as usize % 12])
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
pub fn device_icon(d: &space::Device) -> &'static str {
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

#[component]
pub fn Content(space: String) -> Element {
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
            crate::tags::SearchView { space, q }
        };
    }
    match place {
        Place::Overview => rsx! {
            crate::catalog::Overview { space }
        },
        Place::Drive(folder) => rsx! {
            DriveFolder { space, folder }
        },
        Place::Adopted(path) => rsx! {
            AdoptedFolder { key: "{path}", space, path }
        },
        Place::Location { loc, path } => rsx! {
            LocationFolder { space, loc, path }
        },
        Place::Volume { device, root, path } => rsx! {
            crate::browse::VolumeFolder { key: "{device}{root}/{path}", space, device, root, path }
        },
        Place::Kind(kind) => rsx! {
            KindView { space, kind }
        },
        Place::App(app) => rsx! {
            AppDocs { key: "{app}", app }
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
        Place::Sites => rsx! {
            crate::sites::SitesPage { space }
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
        Place::Tag(id) => rsx! {
            crate::tags::TagView { space, id }
        },
        Place::Tags => rsx! {
            crate::tags::TagManager { space }
        },
        Place::Rules => rsx! {
            crate::rules::RulesPage { space }
        },
        Place::Rule(id) => rsx! {
            crate::rules::RulePage { key: "{id}", space, id }
        },
    }
}

// ── the views ───────────────────────────────────────────────────────────

/// Items as a grid or a list, sorted, folders first.
#[component]
pub fn Items(items: Vec<Item>, show_at: bool, empty: String, loading: bool) -> Element {
    let mut nav = use_nav();
    // The space the items are in (a project another org shared: that org).
    let space = (nav.within)().unwrap_or_else(|| drive_dioxus::use_drive().org.clone());
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
    nav.visible.set(items.clone());
    let shell = app_shell::ui::use_shell();
    // A folder in a new tab (the context menu, or ⌘/Ctrl-click).
    let in_tab = move |i: &Item| {
        if let Some(place) = folder_place(i) {
            let within = nav.within.peek().clone();
            crate::shell::open_tab(shell, crate::nav::At { place, within }, &i.name);
        }
    };
    let mut open = move |i: Item| match folder_place(&i) {
        Some(p) => nav.go(p),
        None => nav.selected.set(Some(i)),
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
                            onclick: move |e: MouseEvent| {
                                let m = e.modifiers();
                                if it.dir && (m.meta() || m.ctrl()) {
                                    in_tab(&it);
                                } else {
                                    open(it.clone());
                                }
                            },
                            ondoubleclick: {
                                let it = i.clone();
                                move |_| {
                                    if !it.dir && viewable(&it) {
                                        nav.selected.set(Some(it.clone()));
                                        nav.preview.set(true);
                                    }
                                }
                            },
                            oncontextmenu: move |e: MouseEvent| {
                                e.prevent_default();
                                let p = e.client_coordinates();
                                let it = it2.clone();
                                nav.selected.set(Some(it.clone()));
                                let mut rows = Vec::new();
                                if it.dir {
                                    let (a, b) = (it.clone(), it.clone());
                                    let mut open = open;
                                    rows.push(app_shell::ui::MenuItem::call("Open", Callback::new(move |()| open(a.clone()))).icon(app_shell::ui::icons::FOLDER));
                                    rows.push(app_shell::ui::MenuItem::call("Open in new tab", Callback::new(move |()| in_tab(&b))).icon(app_shell::ui::icons::PLUS));
                                } else if viewable(&it) {
                                    rows.push(app_shell::ui::MenuItem::call("Quick Look", Callback::new(move |()| nav.preview.set(true))).icon(app_shell::ui::icons::MAXIMIZE));
                                }
                                if !rows.is_empty() {
                                    shell.menu(p.x, p.y, rows);
                                }
                            },
                            KindIcon { icon: ic.clone(), thumb: sidecar_url(&i, &space, drive_sidecars::Kind::Thumb256) }
                            if grid {
                                span { class: "cname", "{i.name}" }
                                span { class: "csub",
                                    if i.dir { "{i.kind.name}" } else { "{human(i.size)}" }
                                }
                                if show_at || matches!(i.src, Src::Logical { .. }) && !i.at.is_empty() {
                                    span { class: "cat", "{i.at}" }
                                }
                            } else {
                                span { class: "lname",
                                    span { class: "nm", "{i.name}" }
                                    if show_at || matches!(i.src, Src::Logical { .. }) && !i.at.is_empty() {
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

/// Where a folder goes when opened (`None`: not a folder).
fn folder_place(i: &Item) -> Option<Place> {
    match &i.src {
        Src::Tree { id, .. } if i.dir => Some(Place::Drive(Some(id.clone()))),
        Src::Index { location, path, .. } if i.dir => Some(Place::Location {
            loc: location.clone(),
            path: path.clone(),
        }),
        Src::Logical { path } if i.dir => Some(Place::Adopted(path.clone())),
        Src::Browse { device, root, path } if i.dir => Some(Place::Volume {
            device: device.clone(),
            root: root.clone(),
            path: path.clone(),
        }),
        _ => None,
    }
}

fn copies_label(i: &Item) -> Option<(String, &'static str)> {
    if i.dir
        || matches!(
            i.src,
            Src::Doc { .. } | Src::Browse { .. } | Src::Logical { .. }
        )
    {
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
pub fn Crumbs(parts: Vec<(String, Option<Place>)>) -> Element {
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
    // The folders adoptions put here (docs/adopt.md): an adopted folder
    // hides a tree entry of the same name, as in the mount.
    let added = adopt::names_in(&adopt::adoptions(&r), &base);
    let mut items: Vec<Item> = items
        .into_iter()
        .filter(|i| !added.iter().any(|(n, point)| *point && *n == i.name))
        .collect();
    for (name, point) in added {
        if items.iter().any(|i| i.dir && i.name == name) {
            continue;
        }
        let path = adopt::join(&base, &name);
        let at = if point {
            adopted_where(&r, &path).map_or(String::new(), |w| format!("on {w}"))
        } else {
            String::new()
        };
        items.push(logical_folder(name, path, at, &kinds));
    }

    // Through the shared uploads: their progress is the phone's accessory
    // too (crate::mobile).
    let uploads = crate::mobile::use_uploads();
    let upload = {
        let files = files.clone();
        let reg = reg.clone();
        move |e: FormEvent| {
            crate::mobile::upload_files(files.clone(), reg.clone(), cur, e.files(), uploads)
        }
    };
    let up_state = (uploads.0)();
    if up_state.busy() {
        let line = format!("Uploading {}…", up_state.current);
        if status() != line {
            status.set(line);
        }
    } else if !up_state.said.is_empty() && status() != up_state.said {
        status.set(up_state.said.clone());
    }
    rsx! {
        div { class: "viewhead",
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

/// Where an adopted folder's files are: `Studio PC · Songs` (its
/// device, or storage, and its location).
fn adopted_where(r: &loro::LoroDoc, at: &str) -> Option<String> {
    let a = adopt::adoptions(r).into_iter().find(|a| a.at == at)?;
    let l = space::locations(r)
        .into_iter()
        .find(|l| l.id == a.location)?;
    let holder = if l.storage.is_empty() {
        space::devices(r)
            .into_iter()
            .find(|d| d.id == l.device)
            .map_or(l.device.clone(), |d| d.name)
    } else {
        space::storages(r)
            .into_iter()
            .find(|s| s.id == l.storage)
            .map_or(l.storage.clone(), |s| s.name)
    };
    Some(format!("{holder} · {}", l.name))
}

/// A folder of the Drive tree by its path (adopted, or on the way).
fn logical_folder(name: String, path: String, at: String, kinds: &Kinds) -> Item {
    Item {
        kind: kinds.of_file(&name, true).clone(),
        size: 0,
        mtime: 0,
        at,
        online: true,
        copies: 0,
        kept: None,
        available: None,
        src: Src::Logical { path },
        name,
        dir: true,
    }
}

/// A folder of the Drive tree that adoptions put there (docs/adopt.md),
/// listed by the node: an adopted folder's files come from its location's
/// index and are read through that location, so they open, preview and
/// download as a computer's files do.
#[component]
fn AdoptedFolder(space: String, path: String) -> Element {
    let reg = use_doc(space_doc(&space));
    let spine_doc = use_doc(spine::SPINE);
    let kinds = Kinds::load(&spine_doc.loro());
    let (sp, pt) = (space.clone(), path.clone());
    // Refetched when the space document moves (an index came in, an
    // adoption changed).
    let version = format!("{:?}", reg.loro().oplog_vv());
    let listing = use_resource(use_reactive!(|(sp, pt, version)| async move {
        let _ = version;
        get(format!("/tree?space={}&path={}", enc(&sp), enc(&pt))).await
    }));
    let v = listing().flatten();
    let items: Vec<Item> = v
        .as_ref()
        .and_then(|v| v["entries"].as_array().cloned())
        .unwrap_or_default()
        .iter()
        .map(|e| {
            let name = s(e, "name");
            let logical = s(e, "logical");
            let dir = e["dir"].as_bool() == Some(true);
            match (s(e, "source").as_str(), e["location"].is_string()) {
                // A location's file or folder (inside an adopted folder).
                ("adopted", true) => {
                    let mut i = hit_item(e, &kinds);
                    if dir {
                        i.src = Src::Logical { path: logical };
                    }
                    i
                }
                ("tree", _) => Item {
                    kind: kinds.of_file(&name, dir).clone(),
                    size: e["size"].as_u64().unwrap_or(0),
                    mtime: e["mtime"].as_i64().unwrap_or(0),
                    at: format!("Drive · /{path}"),
                    online: true,
                    copies: 1,
                    kept: None,
                    available: None,
                    src: Src::Tree {
                        id: s(e, "id"),
                        hash: s(e, "hash"),
                    },
                    name,
                    dir,
                },
                // An adopted folder, or one on the way to adoptions.
                _ => {
                    let w = &e["where"];
                    let at = if w.is_object() {
                        format!("on {} · {}", s(w, "device_name"), s(w, "location_name"))
                    } else {
                        String::new()
                    };
                    logical_folder(name, logical, at, &kinds)
                }
            }
        })
        .collect();
    // Where the adopted folder's files are, for its badge.
    let here = v
        .as_ref()
        .map(|v| v["where"].clone())
        .filter(Value::is_object)
        .map(|w| {
            (
                format!("on {} · {}", s(&w, "device_name"), s(&w, "location_name")),
                s(&w, "real"),
                w["online"] == true,
            )
        });
    let failed = listing().is_some() && v.is_none();
    rsx! {
        if let Some((label, real, up)) = here {
            div { class: "viewhead",
                div { class: "actions",
                    span { class: "badge", title: "{real}", "{label}" }
                    span { class: if up { "badge on" } else { "badge" },
                        if up { "online" } else { "offline · browsing its index" }
                    }
                }
            }
        }
        Items {
            items,
            show_at: false,
            empty: if failed { "This folder is not there (or its index has not come in yet)." } else { "Empty." },
            loading: listing().is_none(),
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
    let holder_up = online(&reg).contains(&l.device);
    let folder_name = path
        .rsplit('/')
        .next()
        .filter(|x| !x.is_empty())
        .map_or(l.name.clone(), str::to_string);
    rsx! {
        div { class: "viewhead",
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

/// `url` when it is safe to put in an `href`: http(s) or a same-origin
/// path, never `javascript:`, `data:` or a protocol-relative URL.
pub(crate) fn safe_href(url: String) -> Option<String> {
    let t = url.trim();
    if t.starts_with("https://")
        || t.starts_with("http://")
        || (t.starts_with('/') && !t.starts_with("//"))
    {
        Some(t.to_string())
    } else {
        None
    }
}

#[component]
pub fn Inspector(space: String) -> Element {
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
    let download = file_url(&item, &space);
    let (where_, open_link) = match &item.src {
        Src::Doc { id, .. } => (
            app_label(&item.kind.app),
            item.kind.link(id).and_then(safe_href),
        ),
        _ => (item.at.clone(), None),
    };
    let can_view = download.is_some() && viewable(&item);
    // Read it at once when that is cheap: from the node, or a small file
    // from a computer (asked of its agent).
    let inline = can_view
        && (matches!(item.src, Src::Tree { .. })
            || item.available.is_some()
            || item.size <= INLINE_FROM_DEVICE);
    let big_thumb = sidecar_url(&item, &space, drive_sidecars::Kind::Thumb1024);
    let peaks = sidecar_url(&item, &space, drive_sidecars::Kind::Peaks);
    rsx! {
        aside { class: "inspector",
            div { class: "ihead",
                button { class: "x", title: "Close", onclick: move |_| nav.selected.set(None), "✕" }
            }
            if inline && let Some(url) = download.clone() {
                div { class: "ipreview",
                    drive_view::Viewer {
                        key: "{url}",
                        kind: item.kind.id.clone(),
                        parent: item.kind.parent.clone(),
                        name: item.name.clone(),
                        url,
                        size: item.size,
                        bearer: drive_dioxus::auth::bearer(),
                        compact: true,
                        peaks: peaks.clone(),
                        poster: big_thumb.clone(),
                    }
                }
            } else {
                KindIcon { key: "{item.key()}", icon: icon(&item.kind.icon), thumb: big_thumb.clone(), class: "ibox" }
            }
            h2 { class: "iname", "{item.name}" }
            span { class: "ikind", "{item.kind.name}" }
            div { class: "iacts",
                if let Some(href) = open_link {
                    a { class: "primary", href: "{href}", "Open in {app_label(&item.kind.app)}" }
                }
                if can_view {
                    button { class: "primary", onclick: move |_| nav.preview.set(true), "Preview" }
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
            crate::browse::InspectorActions { key: "{item.key()}", space: space.clone(), item: item.clone() }
            crate::tags::ItemTags { key: "{item.key()}", space: space.clone(), item: item.clone() }
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

/// Files read from a computer (through its agent) for the inspector's
/// preview without asking; bigger ones wait for Preview.
const INLINE_FROM_DEVICE: u64 = 8 << 20;

/// Where a file's bytes can be read now: the node's copy, or its computer
/// through the node (`None`: nowhere reachable, or not a file).
pub fn file_url(item: &Item, space: &str) -> Option<String> {
    if item.dir {
        return None;
    }
    match &item.src {
        Src::Tree { hash, .. } => {
            (!hash.is_empty()).then(|| format!("/blobs/{hash}?name={}", enc(&item.name)))
        }
        Src::Index { location, path, .. } => match &item.available {
            Some(h) => Some(format!("/blobs/{h}?name={}", enc(&item.name))),
            None => item
                .online
                .then(|| format!("/fetch/{}/{}?path={}", enc(space), enc(location), enc(path))),
        },
        Src::Doc { .. } | Src::Logical { .. } => None,
        Src::Browse { device, root, path } => item
            .online
            .then(|| crate::browse::file_url(space, device, root, path, &item.name)),
    }
}

/// Where an item's sidecar (a thumbnail, its waveform peaks) is read:
/// by its content key, with what the node needs to have it made when it is
/// missing (the file's name; for a computer's file, where it is, and its
/// copy on the node if there is one). `None`: nothing derives from it.
pub fn sidecar_url(item: &Item, space: &str, kind: drive_sidecars::Kind) -> Option<String> {
    if item.dir || !drive_sidecars::kinds_for(&item.name).contains(&kind) {
        return None;
    }
    let name = enc(&item.name);
    match &item.src {
        Src::Tree { hash, .. } if !hash.is_empty() => Some(format!(
            "{}?name={name}",
            drive_sidecars::url(&drive_sidecars::full_key(hash), kind)
        )),
        Src::Index {
            location,
            path,
            sampled,
        } if !sampled.is_empty() => {
            let mut u = format!(
                "{}?name={name}&space={}&location={}&path={}",
                drive_sidecars::url(&drive_sidecars::sampled_key(sampled), kind),
                enc(space),
                enc(location),
                enc(path)
            );
            if let Some(h) = &item.available {
                u.push_str(&format!("&hash={h}"));
            }
            Some(u)
        }
        Src::Browse { device, root, path } => Some(crate::browse::sidecar_url(
            space, device, root, path, item, kind,
        )),
        _ => None,
    }
}

/// An item's icon, with its thumbnail over it once that has loaded (lazily:
/// only when it scrolls into view). No thumbnail, or none can be made: the
/// icon stays.
#[component]
pub fn KindIcon(icon: String, thumb: Option<String>, #[props(default)] class: String) -> Element {
    let mut loaded = use_signal(|| false);
    let mut failed = use_signal(|| false);
    // Another item in this place (a list re-sorted): start over.
    let mut was = use_signal(|| thumb.clone());
    if *was.peek() != thumb {
        was.set(thumb.clone());
        loaded.set(false);
        failed.set(false);
    }
    let shown = thumb.filter(|_| !failed());
    let has = loaded() && shown.is_some();
    rsx! {
        span { class: format!("kbox {class}{}", if has { " has" } else { "" }),
            img { class: "kicon", src: "{icon}", alt: "", draggable: false }
            if let Some(t) = shown {
                img {
                    class: "thumb",
                    src: "{t}",
                    alt: "",
                    loading: "lazy",
                    decoding: "async",
                    draggable: false,
                    onload: move |_| loaded.set(true),
                    onerror: move |_| failed.set(true),
                }
            }
        }
    }
}

/// Whether Drive has a viewer for it.
/// Quick Look, as on a Mac: Space opens the picked file full screen and
/// closes it again; the arrow keys pick the next or previous item (a row
/// at a time in the grid), and an open preview follows. Keys typed into a
/// field, a button or a link are theirs.
const QUICK_LOOK_JS: &str = r#"
const keys = [' ', 'Escape', 'ArrowUp', 'ArrowDown', 'ArrowLeft', 'ArrowRight'];
const own = (t) => t && (t.isContentEditable || ['INPUT', 'TEXTAREA', 'SELECT', 'BUTTON', 'A'].includes(t.tagName));
const columns = () => {
  const cells = document.querySelectorAll('[data-active-tab="1"] .grid > .cell');
  if (!cells.length) return 1;
  const top = cells[0].offsetTop;
  let n = 0;
  for (const c of cells) { if (c.offsetTop !== top) break; n++; }
  return Math.max(n, 1);
};
const onKey = (e) => {
  if (e.metaKey || e.ctrlKey || e.altKey || !keys.includes(e.key) || own(e.target)) return;
  if (!document.body.dataset.picked) return;
  if (e.key !== 'Escape') e.preventDefault();
  dioxus.send(e.key + ':' + columns());
  requestAnimationFrame(() => requestAnimationFrame(() => {
    const on = document.querySelector('[data-active-tab="1"] :is(.cell.on, .row.on)');
    if (on) on.scrollIntoView({ block: 'nearest' });
  }));
};
if (window.__driveQuickLook) window.removeEventListener('keydown', window.__driveQuickLook);
window.__driveQuickLook = onKey;
window.addEventListener('keydown', onKey);
await new Promise(() => {});
"#;

/// Quick Look on the active tab.
pub fn use_quick_look(navs: crate::nav::Navs) {
    use_hook(move || {
        spawn(async move {
            let mut keys = document::eval(QUICK_LOOK_JS);
            while let Ok(k) = keys.recv::<String>().await {
                let (key, cols) = k.rsplit_once(':').unwrap_or((k.as_str(), "1"));
                quick_look_key(navs.active_now(), key, cols.parse().unwrap_or(1));
            }
        });
    });
    // The page's keys act only while something is picked.
    use_effect(move || {
        let picked = (navs.active().selected)().is_some();
        crate::host::eval(if picked {
            "document.body.dataset.picked = '1'"
        } else {
            "delete document.body.dataset.picked"
        });
    });
}

fn quick_look_key(mut nav: Nav, key: &str, columns: usize) {
    let open = *nav.preview.peek();
    match key {
        " " => {
            if open {
                nav.preview.set(false);
            } else if nav.selected.peek().as_ref().is_some_and(viewable) {
                nav.preview.set(true);
            }
        }
        "Escape" => {
            if open {
                nav.preview.set(false);
            }
        }
        _ => {
            let items = nav.visible.read().clone();
            let grid = *nav.view.peek() == View::Grid;
            let step = match key {
                "ArrowUp" | "ArrowDown" if grid => columns.max(1),
                _ => 1,
            };
            let forward = matches!(key, "ArrowDown" | "ArrowRight");
            let current = nav.selected.peek().as_ref().map(Item::key);
            let Some(next) = step_to(&items, current.as_deref(), step, forward) else {
                return;
            };
            if open && !viewable(&next) {
                nav.preview.set(false);
            }
            nav.selected.set(Some(next));
        }
    }
}

/// Quick Look's next (or previous) file: what a swipe on a phone does.
pub fn quick_look_step(nav: Nav, forward: bool) {
    quick_look_key(nav, if forward { "ArrowRight" } else { "ArrowLeft" }, 1);
}

/// The item `step` places on from `current` (the first one when nothing
/// is picked), staying inside the list.
fn step_to(items: &[Item], current: Option<&str>, step: usize, forward: bool) -> Option<Item> {
    let last = items.len().checked_sub(1)?;
    let at = current.and_then(|c| items.iter().position(|i| i.key() == c));
    let next = match at {
        None => 0,
        Some(i) if forward => (i + step).min(last),
        Some(i) => i.saturating_sub(step),
    };
    items.get(next).cloned()
}

pub fn viewable(item: &Item) -> bool {
    !item.dir
        && !matches!(item.src, Src::Doc { .. })
        && drive_view::ViewKind::pick(&item.kind.id, &item.kind.parent, &item.name).is_some()
}

/// Keep bytes on the node: their hash, or why not.
async fn put_blob(bytes: &[u8]) -> Result<String, String> {
    let hash = drive_files::hash::of_bytes(bytes);
    match crate::host::request("PUT", &format!("/blobs/{hash}"), None, Some(bytes.to_vec())).await {
        Some((status, _)) if (200..300).contains(&status) => Ok(hash),
        Some((status, _)) => Err(format!("the node refused it ({status})")),
        None => Err("the node did not answer".into()),
    }
}

/// Ask the node to edit a PDF's pages: the edited file's hash and size.
async fn edit_pdf(hash: &str, edits: &[drive_view::PageEdit]) -> Result<(String, u64), String> {
    let body = serde_json::to_string(edits).map_err(|e| e.to_string())?;
    let (status, bytes) = crate::host::request(
        "POST",
        &format!("/pdf/edit/{hash}"),
        Some("application/json"),
        Some(body.into_bytes()),
    )
    .await
    .ok_or("the node did not answer")?;
    if !(200..300).contains(&status) {
        let why = crate::host::text(&bytes);
        return Err(if why.is_empty() {
            status.to_string()
        } else {
            why
        });
    }
    let v: Value = serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
    Ok((s(&v, "hash"), v["size"].as_u64().unwrap_or(0)))
}

/// The picked file, full screen: its viewer, with quick edits for files of
/// the space's own tree (a new version of the file, as an upload makes).
#[component]
pub fn Preview(space: String) -> Element {
    let mut nav = use_nav();
    let files = use_doc(files_doc(&space));
    let reg = use_doc(space_doc(&space));
    let mut status = use_signal(String::new);
    let mut anchor = use_signal(|| None::<drive_view::Anchor>);
    let Some(item) = (nav.selected)() else {
        return rsx! {};
    };
    let Some(url) = file_url(&item, &space) else {
        return rsx! {};
    };
    // A new version of a tree file: point its entry at `hash`.
    let commit = {
        let files = files.clone();
        let reg = reg.clone();
        move |item: Item, hash: String, size: u64| {
            let Src::Tree { id, .. } = &item.src else {
                return;
            };
            let Some(tid) = tree::parse_id(id) else {
                return;
            };
            files.edit(|d| {
                let _ = tree::set_content(d, tid, size, &hash);
            });
            reg.edit(|d| {
                let _ = space::set_held(d, "node", &hash, Some("stored"));
            });
            let mut next = item.clone();
            next.size = size;
            next.src = Src::Tree {
                id: id.clone(),
                hash,
            };
            let mut sel = nav.selected;
            sel.set(Some(next));
        }
    };
    let editable = matches!(item.src, Src::Tree { .. });
    let on_save = editable.then(|| {
        let item = item.clone();
        let commit = commit.clone();
        EventHandler::new(move |bytes: Vec<u8>| {
            let item = item.clone();
            let commit = commit.clone();
            spawn(async move {
                status.set("Saving…".into());
                match put_blob(&bytes).await {
                    Ok(hash) => {
                        commit(item, hash, bytes.len() as u64);
                        status.set("Saved".into());
                    }
                    Err(e) => status.set(format!("Not saved: {e}")),
                }
            });
        })
    });
    let on_pdf_edit = match &item.src {
        Src::Tree { hash, .. } if editable => {
            let item = item.clone();
            let hash = hash.clone();
            Some(EventHandler::new(
                move |edits: Vec<drive_view::PageEdit>| {
                    let item = item.clone();
                    let hash = hash.clone();
                    let commit = commit.clone();
                    spawn(async move {
                        status.set("Editing the PDF…".into());
                        match edit_pdf(&hash, &edits).await {
                            Ok((new, size)) => {
                                commit(item, new, size);
                                status.set("Saved".into());
                            }
                            Err(e) => status.set(format!("Not edited: {e}")),
                        }
                    });
                },
            ))
        }
        _ => None,
    };
    let close = move |_| nav.preview.set(false);
    let phone = app_shell::ui::use_shell().form() == app_shell::ui::FormFactor::Phone;
    let header = rsx! {
        header { class: "fvhead",
            img { class: "fvicon", src: icon(&item.kind.icon), alt: "" }
            div { class: "fvname",
                b { "{item.name}" }
                span { class: "ikind", "{item.kind.name} · {human(item.size)}" }
            }
            if !status().is_empty() {
                span { class: "fvstatus", "{status}" }
            }
            a { class: "primary ghost", href: "{url}", "Download" }
            button { class: "x", title: "Close (Esc)", onclick: close, "✕" }
        }
    };
    rsx! {
        div {
            class: if phone { "fullview phone" } else { "fullview" },
            tabindex: "0",
            onkeydown: move |e: KeyboardEvent| {
                if e.key().to_string() == "Escape" {
                    nav.preview.set(false);
                }
            },
            if phone {
                crate::mobile::QuickLookSwipe { nav, {header} }
            } else {
                {header}
            }
            div { class: "fvbody",
                drive_view::Viewer {
                    key: "{url}",
                    kind: item.kind.id.clone(),
                    parent: item.kind.parent.clone(),
                    name: item.name.clone(),
                    url: url.clone(),
                    size: item.size,
                    bearer: drive_dioxus::auth::bearer(),
                    on_anchor: (!phone).then(|| EventHandler::new(move |a| anchor.set(Some(a)))),
                    on_save: on_save.filter(|_| !phone),
                    on_pdf_edit: on_pdf_edit.filter(|_| !phone),
                    peaks: sidecar_url(&item, &space, drive_sidecars::Kind::Peaks),
                    poster: sidecar_url(&item, &space, drive_sidecars::Kind::Thumb1024),
                }
            }
            if let Some(a) = anchor() {
                footer { class: "fvfoot", title: "Where a comment would hang", "{a.describe()}" }
            }
        }
    }
}

/// A link to a file kept on the node, to copy and send.
#[component]
fn ShareLink(hash: String, name: String) -> Element {
    let origin = crate::host::origin();
    let url = format!("{origin}/blobs/{hash}?name={}", enc(&name));
    let mut copied = use_signal(|| false);
    let u = url.clone();
    rsx! {
        div { class: "share",
            input { readonly: true, value: "{url}" }
            button {
                class: "primary ghost",
                onclick: move |_| {
                    crate::host::copy_text(&u);
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
        // A browsed file has no lasting place to pin comments to.
        Src::Browse { .. } | Src::Logical { .. } => return rsx! {},
    };
    let family = if item.kind.parent.is_empty() {
        item.kind.id.clone()
    } else {
        item.kind.parent.clone()
    };
    let media = matches!(family.as_str(), "audio" | "video") && download.is_some();
    // Its own element id: several open tabs (or the phone sheet beside a
    // hidden pane) each have a player, and the comments must pin to and
    // seek the one beside them (drive#64).
    let player_id = use_hook(review_player_id);
    let player = media.then(|| player_id.clone());
    rsx! {
        section { class: "isec",
            if media {
                if family == "video" {
                    video { id: "{player_id}", class: "iplayer", controls: true, preload: "none", src: download.clone().unwrap_or_default() }
                } else {
                    audio { id: "{player_id}", class: "iplayer", controls: true, preload: "none", src: download.clone().unwrap_or_default() }
                }
            }
            drive_dioxus::comments::CommentsPanel { key: "{item.key()}", target, version, player }
        }
    }
}

/// A player element id no other mounted player has.
fn review_player_id() -> String {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    format!("drive-review-player-{n}")
}

#[cfg(test)]
mod tests {
    use super::{review_player_id, safe_href};

    #[test]
    fn each_review_player_gets_its_own_id() {
        assert_ne!(review_player_id(), review_player_id());
    }

    #[test]
    fn safe_href_keeps_web_and_local_links_only() {
        for ok in ["https://a.example/x", "http://a.example", "/notes/1", " /x "] {
            assert_eq!(safe_href(ok.into()).as_deref(), Some(ok.trim()));
        }
        for bad in [
            "javascript:alert(1)",
            " JavaScript:alert(1)",
            "data:text/html,<script>",
            "//evil.example",
            "vbscript:x",
            "",
        ] {
            assert_eq!(safe_href(bad.into()), None, "{bad}");
        }
    }
}

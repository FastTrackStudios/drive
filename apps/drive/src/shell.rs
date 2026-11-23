//! The window, on app-shell: a Spacedrive-style sidebar of places, a
//! unified top bar (back and forward, where the tab is, search, the view
//! and the inspector), and one place in the content area. A second tab
//! (Open in new tab, or ⌘/Ctrl-click or middle-click a place) brings the
//! tab bar; each tab has its own place, selection, view and history.
//!
//! The layout and the sidebar are kept in the browser, per org.

use app_shell::ui::{
    AppShell, Icon, MenuItem, SearchField, Shell, Sidebar, SidebarSection, SidebarState,
    SidebarSwitcher, TabCtx, TabKind, TabKinds, Tabs, ToolbarButton, ToolbarGroup, TopBar, icons,
    use_shell,
};
use app_shell::{PaneId, Target, Theme, Workspace};
use dioxus::prelude::*;
use drive_dioxus::ui::{Avatars, ConnectionStatus};
use drive_dioxus::{use_doc, use_docs, use_drive};
use drive_files::{files_doc, space, space_doc, tree};
use drive_spine as spine;
use drive_spine::kinds::Kinds;

use crate::browser::{
    Content, Inspector, Preview, app_label, device_icon, get, online, plural, project_icon, s,
    use_quick_look,
};
use crate::mobile::{self, MobileHost, Naming};
use crate::nav::{
    At, InTab, Navs, Place, Sort, Store, TAB_KIND, View, new_pane, storage_get, storage_set,
    use_nav,
};

/// Drive's colours on the shell's variables: the Studio prototype's dark
/// palette, laid out like Spacedrive (a sidebar a shade apart, a quiet top
/// bar, the teal accent).
fn theme() -> Theme {
    Theme::dark()
        .set("--as-bg", "#0b0b0e")
        .set("--as-surface", "#141417")
        .set("--as-raised", "#1f1f23")
        .set("--as-overlay", "rgba(0, 0, 0, 0.6)")
        .set("--as-border", "#232328")
        .set("--as-text", "#f4f4f5")
        .set("--as-muted", "#a1a1aa")
        .set("--as-heading", "#71717a")
        .set("--as-accent", "#2dd4bf")
        .set("--as-accent-soft", "rgba(45, 212, 191, 0.22)")
        .set("--as-topbar-bg", "#0b0b0e")
        .set("--as-sidebar-bg", "rgba(17, 17, 20, 0.94)")
        .set("--as-selected", "rgba(255, 255, 255, 0.08)")
        .set("--as-selected-text", "#ffffff")
        .set("--as-search-bg", "rgba(255, 255, 255, 0.05)")
        .set(
            "--as-font",
            "\"Instrument Sans\", -apple-system, system-ui, sans-serif",
        )
        .set("--as-font-size", "13px")
        .set("--as-radius", "7px")
}

const SORT: Icon = Icon(&["m3 16 4 4 4-4", "M7 20V4", "m21 8-4-4-4 4", "M17 4v16"]);
const CHECK: Icon = Icon(&["M20 6 9 17l-5-5"]);
const SIDEBAR_KEY: &str = "drive.sidebar";

/// The layout this browser kept for `key`, or one tab at the Drive's root.
fn load_layout(key: &str) -> Workspace {
    let kept = storage_get(key)
        .and_then(|json| serde_json::from_str::<Workspace>(&json).ok())
        .map(Workspace::repaired);
    let mut ws = kept.unwrap_or_default();
    ws.panes.retain(|p| p.view.0 == TAB_KIND);
    let mut ws = ws.repaired();
    if ws.panes.is_empty() {
        ws = Workspace::new();
        let _ = ws.open(new_pane(home(), "Drive"), Target::Tab);
    }
    ws
}

fn home() -> At {
    At {
        place: Place::Drive(None),
        within: None,
    }
}

fn load_sidebar() -> SidebarState {
    let mut state = SidebarState::default();
    if let Some((open, width)) = storage_get(SIDEBAR_KEY)
        .as_deref()
        .and_then(|v| v.split_once(':'))
    {
        state.open = open != "0";
        state.width = width.parse().unwrap_or(state.width);
    }
    state
}

/// Open `at` in a new tab beside the active one.
pub fn open_tab(shell: Shell, at: At, title: &str) {
    shell.open(new_pane(at, title), Target::Tab);
}

#[component]
pub fn Browser() -> Element {
    let ctx = use_drive();
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
    let key = format!("drive.layout.{space}");
    let layout = use_signal(|| load_layout(&key));
    let store = use_hook(|| Store {
        layout,
        key: CopyValue::new(key.clone()),
    });
    let navs = use_context_provider(|| Navs::new(store));
    use_quick_look(navs);
    // A phone's pill, round button and uploads (crate::mobile).
    let uploads = mobile::use_uploads_provider();
    let shell_slot = use_hook(|| CopyValue::new(None::<Shell>));
    let naming = use_signal(|| None::<Naming>);
    let destination = mobile::destination_of(&(navs.active().place)());
    let projects_space = space.clone();
    let accessory = mobile::uploads_showing(uploads).then(|| rsx! { mobile::UploadAccessory {} });
    let kinds = use_hook(|| {
        TabKinds::new().with(
            TabKind::new(TAB_KIND, "Place", |ctx: TabCtx| {
                rsx! {
                    PlaceTab { pane: ctx.pane.id.0.clone(), active: ctx.active }
                }
            })
            .icon(icons::FOLDER),
        )
    });
    let theme = use_hook(theme);
    let sidebar_state = use_hook(load_sidebar);
    let switch = ctx.clone();
    rsx! {
        div { class: "window",
            AppShell {
                layout,
                kinds,
                tabs: Tabs::Auto,
                theme,
                on_change: move |ws: Workspace| {
                    // The last tab closed: the Drive's root again.
                    if ws.panes.is_empty() {
                        let mut layout = store.layout;
                        let _ = layout.write().open(new_pane(home(), "Drive"), Target::Tab);
                    }
                    store.save();
                },
                on_new_tab: move |_| {
                    let mut layout = store.layout;
                    let _ = layout.write().open(new_pane(home(), "Drive"), Target::Tab);
                    store.save();
                },
                sidebar: rsx! {
                    DriveSidebar { space: space.clone(), spaces: list, on_space: move |s: String| switch.set_org(&s) }
                },
                sidebar_state,
                on_sidebar: move |st: SidebarState| {
                    storage_set(SIDEBAR_KEY, &format!("{}:{}", u8::from(st.open), st.width.round()));
                },
                top_bar: rsx! {
                    DriveTopBar {}
                    MobileHost { shell_slot, naming }
                },
                destinations: mobile::destinations(),
                destination,
                on_destination: move |d: String| {
                    if let Some(sh) = *shell_slot.peek() {
                        mobile::go_destination(sh, navs.active_now(), &d, mobile::projects_sheet(projects_space.clone()));
                    }
                },
                action: mobile::new_action(shell_slot, naming),
                accessory,
            }
        }
    }
}

// ── a tab ───────────────────────────────────────────────────────────────

/// One tab: its place, the inspector beside it, Quick Look over it.
#[component]
fn PlaceTab(pane: String, active: bool) -> Element {
    let navs = use_context::<Navs>();
    let nav = navs.get(&pane);
    use_context_provider(|| InTab(nav));
    let org = use_drive().org.clone();
    // A project another org shared is shown in its own org's registry.
    let view = (nav.within)().unwrap_or(org);
    let picked = (nav.selected)().is_some();
    let phone = use_shell().form() == app_shell::ui::FormFactor::Phone;
    let class = format!(
        "tab{}{}",
        if (nav.inspector)() { " inspecting" } else { "" },
        if picked { " picked" } else { "" }
    );
    rsx! {
        div { class: "{class}", "data-active-tab": if active { "1" } else { "0" },
            div { class: "content",
                Content { key: "{view}", space: view.clone() }
            }
            Inspector { key: "{view}", space: view.clone() }
            if phone && active {
                mobile::InspectorSheet { nav, space: view.clone(), open: picked && !(nav.preview)() }
            }
            if (nav.preview)() && picked {
                Preview { key: "{view}", space: view.clone() }
            }
            TabTitle { key: "{view}", pane: pane.clone(), space: view.clone() }
        }
    }
}

/// Keeps the tab's title (what its tab and the switcher show) as where it is.
#[component]
fn TabTitle(pane: String, space: String) -> Element {
    let nav = use_nav();
    let store = use_context::<Navs>().store;
    let crumbs = use_crumbs(&space, &(nav.place)());
    let title = crumbs.last().map(|c| c.0.clone()).unwrap_or_default();
    use_effect(use_reactive!(|(title, pane)| {
        let id = PaneId::new(pane.clone());
        let mut layout = store.layout;
        if layout.peek().pane(&id).is_some_and(|p| p.title != title) {
            if let Some(p) = layout.write().pane_mut(&id) {
                p.title = title.clone();
            }
            store.save();
        }
    }));
    rsx! {}
}

// ── where a place is ────────────────────────────────────────────────────

pub fn title_of(p: &Place) -> &'static str {
    match p {
        Place::Overview => "Overview",
        Place::Drive(_) | Place::Adopted(_) => "Drive",
        Place::Location { .. } | Place::Volume { .. } => "Computers",
        Place::Kind(_) => "Kind",
        Place::App(_) => "App",
        Place::AtRisk(_) => "At risk",
        Place::Project(_) => "Project",
        Place::AllOrgs => "All orgs",
        Place::Protection => "Protection",
        Place::Devices => "Devices & storage",
        Place::Sites => "Sites",
        Place::Inbox => "Inbox",
        Place::Calendar => "Calendar",
        Place::Notes => "Notes",
        Place::Everything => "Raw documents",
        Place::Tag(_) => "Tag",
        Place::Tags => "Tags",
        Place::Rules => "Rules",
        Place::Rule(_) => "Rule",
    }
}

/// The path to a place, each step with where it goes (`None`: nowhere).
fn use_crumbs(space: &str, place: &Place) -> Vec<(String, Option<Place>)> {
    let files = use_doc(files_doc(space));
    let reg = use_doc(space_doc(space));
    let tag_doc = use_doc(spine::tags::tags_doc(space));
    let rules_doc = use_doc(drive_rules::doc::rules_doc(space));
    let spine_doc = use_doc(spine::SPINE);
    match place {
        Place::Drive(folder) => {
            let f = files.loro();
            let mut crumbs = Vec::new();
            let mut c = folder.as_deref().and_then(tree::parse_id);
            while let Some(id) = c {
                if let Some(e) = tree::entry(&f, id) {
                    crumbs.push((e.name, Some(Place::Drive(Some(tree::id_string(id))))));
                }
                c = tree::parent_of(&f, id);
            }
            let root = if space.is_empty() { "Drive" } else { space };
            crumbs.push((root.to_string(), Some(Place::Drive(None))));
            crumbs.reverse();
            crumbs
        }
        Place::Adopted(path) => {
            // Each step a tree folder where the tree has one, else the
            // adopted (or on-the-way) folder by its path.
            let f = files.loro();
            let all = drive_files::adopt::adoptions(&reg.loro());
            let root = if space.is_empty() { "Drive" } else { space };
            let mut crumbs = vec![(root.to_string(), Some(Place::Drive(None)))];
            let mut acc = String::new();
            for p in path.split('/').filter(|p| !p.is_empty()) {
                acc = drive_files::adopt::join(&acc, p);
                let tree_dir = match drive_files::adopt::place(&all, &acc) {
                    drive_files::adopt::Place::Inside { .. } => None,
                    _ => tree::resolve(&f, &acc).flatten().filter(|e| e.dir),
                };
                let to = match tree_dir {
                    Some(e) => Place::Drive(Some(tree::id_string(e.id))),
                    None => Place::Adopted(acc.clone()),
                };
                crumbs.push((p.to_string(), Some(to)));
            }
            crumbs
        }
        Place::Location { loc, path } => {
            let r = reg.loro();
            let Some(l) = space::locations(&r).into_iter().find(|x| &x.id == loc) else {
                return vec![("Computers".into(), None)];
            };
            let owner = if l.storage.is_empty() {
                space::devices(&r)
                    .into_iter()
                    .find(|d| d.id == l.device)
                    .map_or(String::new(), |d| d.name)
            } else {
                space::storages(&r)
                    .into_iter()
                    .find(|st| st.id == l.storage)
                    .map_or(String::new(), |st| st.name)
            };
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
            crumbs
        }
        Place::Project(id) => {
            let name = drive_files::projects::projects(&reg.loro())
                .into_iter()
                .find(|p| &p.id == id)
                .map_or_else(|| "Project".to_string(), |p| p.name);
            vec![(name, None)]
        }
        Place::Tag(id) => {
            let lib = spine::tags::Library::load(&tag_doc.loro());
            let name = lib
                .get(id)
                .map_or_else(|| "Tag".to_string(), |t| t.name.clone());
            vec![("Tags".into(), Some(Place::Tags)), (name, None)]
        }
        Place::Rule(id) => {
            let name = drive_rules::doc::rule(&rules_doc.loro(), id)
                .map_or_else(|| "Rule".to_string(), |r| r.name);
            vec![("Rules".into(), Some(Place::Rules)), (name, None)]
        }
        Place::Kind(k) => {
            let kinds = Kinds::load(&spine_doc.loro());
            vec![(plural(&kinds.get(k).name), None)]
        }
        Place::App(a) => vec![(app_label(a), None)],
        Place::AtRisk(_) => vec![
            ("Protection".into(), Some(Place::Protection)),
            ("At risk".into(), None),
        ],
        other => vec![(title_of(other).to_string(), None)],
    }
}

/// The top bar's lead: where the active tab is, each step a way back up.
#[component]
fn PlaceCrumbs(space: String) -> Element {
    let nav = use_nav();
    let crumbs = use_crumbs(&space, &(nav.place)());
    let last = crumbs.len().saturating_sub(1);
    rsx! {
        div { class: "crumbs",
            for (i , (name , p)) in crumbs.into_iter().enumerate() {
                Fragment { key: "{i}",
                    if i > 0 {
                        span { class: "sep", "›" }
                    }
                    match p {
                        Some(p) if i < last => rsx! {
                            button { onclick: move |_| nav.go(p.clone()), "{name}" }
                        },
                        _ => rsx! {
                            span { class: if i == last { "crumbtext here" } else { "crumbtext" }, "{name}" }
                        },
                    }
                }
            }
        }
    }
}

/// Whether the place lists files (search, view and sort apply).
pub fn browsable(p: &Place) -> bool {
    matches!(
        p,
        Place::Drive(_)
            | Place::Adopted(_)
            | Place::Location { .. }
            | Place::Volume { .. }
            | Place::Kind(_)
            | Place::App(_)
            | Place::AtRisk(_)
            | Place::Project(_)
            | Place::Tag(_)
    )
}

// ── the top bar ─────────────────────────────────────────────────────────

#[component]
fn DriveTopBar() -> Element {
    let mut nav = use_nav();
    let shell = use_shell();
    let org = use_drive().org.clone();
    let spine_doc = use_doc(spine::SPINE);
    let place = (nav.place)();
    let space = (nav.within)().unwrap_or(org);
    let files = browsable(&place) || place == Place::Overview || !(nav.query)().is_empty();
    let grid = (nav.view)() == View::Grid;
    let inspecting = (nav.inspector)();
    let (sort, _) = (nav.sort)();
    let sort_menu = move |x: f64, y: f64| {
        let row = |label: &str, s: (Sort, bool)| {
            let mut item = MenuItem::call(label, Callback::new(move |()| nav.set_sort(s)));
            if s.0 == sort {
                item = item.icon(CHECK);
            }
            item
        };
        shell.menu(
            (x - 150.0).max(4.0),
            y + 16.0,
            vec![
                row("Name", (Sort::Name, false)),
                row("Size", (Sort::Size, true)),
                row("Modified", (Sort::Modified, true)),
            ],
        );
    };
    let search = files.then(|| {
        rsx! {
            SearchField {
                value: (nav.query)(),
                placeholder: "Search, or tags: drums NOT archived",
                on_input: move |v: String| {
                    nav.query.set(v);
                    nav.selected.set(None);
                },
            }
        }
    });
    rsx! {
        TopBar {
            lead: rsx! {
                PlaceCrumbs { key: "{space}", space: space.clone() }
            },
            search,
            actions: rsx! {
                if files {
                    ToolbarGroup {
                        ToolbarButton { icon: icons::GRID, label: "Grid", active: grid, on_press: move |()| nav.set_view(View::Grid) }
                        ToolbarButton { icon: icons::LIST, label: "List", active: !grid, on_press: move |()| nav.set_view(View::List) }
                    }
                    div {
                        class: "tbwrap",
                        onclick: move |e: MouseEvent| {
                            let p = e.client_coordinates();
                            sort_menu(p.x, p.y);
                        },
                        ToolbarButton { icon: SORT, label: "Sort", on_press: move |()| {} }
                    }
                    div { class: "tbwrap desk",
                        ToolbarButton {
                            icon: icons::PANEL_RIGHT,
                            label: "Inspector",
                            active: inspecting,
                            on_press: move |()| nav.inspector.set(!inspecting),
                        }
                    }
                }
                div { class: "who",
                    ConnectionStatus {}
                    Avatars { doc: spine_doc.clone() }
                }
            },
            phone_actions: rsx! {
                if files {
                    ToolbarButton {
                        icon: if grid { icons::LIST } else { icons::GRID },
                        label: if grid { "List" } else { "Grid" },
                        on_press: move |()| nav.set_view(if grid { View::List } else { View::Grid }),
                    }
                    div {
                        class: "tbwrap",
                        onclick: move |e: MouseEvent| {
                            let p = e.client_coordinates();
                            sort_menu(p.x, p.y);
                        },
                        ToolbarButton { icon: SORT, label: "Sort", on_press: move |()| {} }
                    }
                }
            },
            on_back: move |()| nav.step(false),
            on_forward: move |()| nav.step(true),
            can_back: nav.can_step(false),
            can_forward: nav.can_step(true),
        }
    }
}

// ── the sidebar ─────────────────────────────────────────────────────────

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

/// A place in the sidebar: a Spacedrive icon (or a tag's colour), its
/// name, a note or a count, whether its computer is on. A press goes there
/// in this tab; ⌘/Ctrl or the middle button opens it in a new tab, and the
/// context menu offers both.
#[component]
pub fn PlaceRow(
    at: At,
    label: String,
    /// An icon of the set (`Folder`, `Laptop`, …).
    icon: Option<String>,
    /// A tag's colour (a dot instead of an icon).
    color: Option<String>,
    note: Option<String>,
    /// Whether the place's computer is on (a dot).
    live: Option<bool>,
    #[props(default)] depth: usize,
    title: Option<String>,
) -> Element {
    let shell = use_shell();
    let nav = use_nav();
    let on = *nav.place.read() == at.place && *nav.within.read() == at.within;
    let go = {
        let at = at.clone();
        move || match &at.within {
            Some(org) => nav.go_in(org, at.place.clone()),
            None => nav.go_home(at.place.clone()),
        }
    };
    let tab = {
        let (at, label) = (at.clone(), label.clone());
        move || open_tab(shell, at.clone(), &label)
    };
    let pad = 8 + depth * 14;
    let (bg, fg) = if on {
        ("var(--as-selected)", "var(--as-selected-text)")
    } else {
        ("transparent", "var(--as-text)")
    };
    rsx! {
        div {
            class: if on { "as-sidebar-item drive-row on" } else { "as-sidebar-item as-hov drive-row" },
            role: "button",
            "aria-current": if on { "page" } else { "false" },
            title: title.unwrap_or_default(),
            style: "display: flex; align-items: center; gap: 8px; height: 28px; padding: 0 8px 0 {pad}px; border-radius: var(--as-radius); background-color: {bg}; color: {fg}; white-space: nowrap; overflow: hidden; flex: none; cursor: default;",
            onclick: {
                let (go, tab) = (go.clone(), tab.clone());
                move |e: MouseEvent| {
                    shell.set_drawer(false);
                    let m = e.modifiers();
                    if m.meta() || m.ctrl() {
                        tab();
                    } else {
                        go();
                    }
                }
            },
            onmousedown: move |e: MouseEvent| {
                // The middle button: no autoscroll, a tab on release.
                if e.trigger_button() == Some(dioxus::html::input_data::MouseButton::Auxiliary) {
                    e.prevent_default();
                }
            },
            onauxclick: {
                let tab = tab.clone();
                move |e: PointerEvent| {
                    if e.trigger_button() == Some(dioxus::html::input_data::MouseButton::Auxiliary) {
                        e.prevent_default();
                        shell.set_drawer(false);
                        tab();
                    }
                }
            },
            oncontextmenu: {
                let (go, tab) = (go.clone(), tab.clone());
                move |e: MouseEvent| {
                    e.prevent_default();
                    let p = e.client_coordinates();
                    let (go, tab) = (go.clone(), tab.clone());
                    shell.menu(
                        p.x,
                        p.y,
                        vec![
                            MenuItem::call(
                                    "Open",
                                    Callback::new(move |()| {
                                        shell.set_drawer(false);
                                        go();
                                    }),
                                )
                                .icon(icons::FOLDER),
                            MenuItem::call(
                                    "Open in new tab",
                                    Callback::new(move |()| {
                                        shell.set_drawer(false);
                                        tab();
                                    }),
                                )
                                .icon(icons::PLUS),
                        ],
                    );
                }
            },
            if let Some(c) = color {
                span { class: "tagdot", style: "--tag: {c}" }
            } else if let Some(ic) = icon {
                img { class: "sicon", src: crate::browser::icon(&ic), alt: "", draggable: false }
            }
            span { class: "slabel", "{label}" }
            if let Some(n) = note.filter(|n| !n.is_empty()) {
                span { class: "snote", "{n}" }
            }
            if let Some(live) = live {
                span { class: if live { "dot on" } else { "dot" } }
            }
        }
    }
}

fn here(place: Place) -> At {
    At {
        place,
        within: None,
    }
}

#[component]
fn DriveSidebar(space: String, spaces: Vec<String>, on_space: EventHandler<String>) -> Element {
    let nav = use_nav();
    let shell = use_shell();
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
    let mut apps: Vec<String> = docs()
        .iter()
        .filter_map(|d| d.id.split(':').next().map(str::to_string))
        .filter(|a| a != "drive")
        .collect();
    apps.sort();
    apps.dedup();
    let org_name = if space.is_empty() {
        "Drive".to_string()
    } else {
        space.clone()
    };
    let detail = if spaces.len() > 1 {
        format!("Drive · {} orgs", spaces.len())
    } else {
        "Drive".to_string()
    };
    let switch_menu = {
        let (spaces, space) = (spaces.clone(), space.clone());
        move |x: f64, y: f64| {
            let mut items: Vec<MenuItem> = spaces
                .iter()
                .map(|sp| {
                    let s2 = sp.clone();
                    let item = MenuItem::call(
                        sp.clone(),
                        Callback::new(move |()| on_space.call(s2.clone())),
                    );
                    if *sp == space { item.icon(CHECK) } else { item }
                })
                .collect();
            items.push(MenuItem::separator());
            items.push(
                MenuItem::call(
                    "All orgs",
                    Callback::new(move |()| {
                        shell.set_drawer(false);
                        nav.go_home(Place::AllOrgs);
                    }),
                )
                .icon(icons::LAYERS),
            );
            shell.menu(x, y, items);
        }
    };
    rsx! {
        Sidebar {
            header: rsx! {
                div {
                    onclick: move |e: MouseEvent| {
                        let p = e.client_coordinates();
                        switch_menu(p.x.min(40.0), p.y + 20.0);
                    },
                    SidebarSwitcher { title: org_name.clone(), detail, on_press: move |()| {} }
                }
            },
            SidebarSection { title: "Favourites",
                PlaceRow { at: here(Place::Overview), label: "Overview", icon: "Globe" }
                PlaceRow { at: here(Place::Drive(None)), label: org_name.clone(), icon: "Home" }
                PlaceRow { at: here(Place::AllOrgs), label: "All orgs", icon: "Devices" }
            }
            SidebarSection { title: "Projects",
                for p in projects.iter() {
                    PlaceRow { key: "{p.id}", at: here(Place::Project(p.id.clone())), label: p.name.clone(), icon: project_icon(&p.kind) }
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
            }
            if !devices.is_empty() {
                SidebarSection { title: "Computers",
                    for d in devices.iter() {
                        {
                            let locs: Vec<&space::Location> = locations.iter().filter(|l| l.device == d.id && l.storage.is_empty()).collect();
                            let first = locs.first().map_or(Place::Devices, |l| Place::Location { loc: l.id.clone(), path: String::new() });
                            rsx! {
                                Fragment { key: "{d.id}",
                                    PlaceRow { at: here(first), label: d.name.clone(), icon: device_icon(d), live: up.contains(&d.id) }
                                    for l in locs {
                                        PlaceRow {
                                            key: "{l.id}",
                                            at: here(Place::Location { loc: l.id.clone(), path: String::new() }),
                                            label: l.name.clone(),
                                            icon: if l.mirror_of.is_empty() { "Location" } else { "LocationReplica" },
                                            depth: 1,
                                        }
                                    }
                                    crate::browse::DeviceVolumes { space: space.clone(), device: d.id.clone(), live: up.contains(&d.id) }
                                }
                            }
                        }
                    }
                }
            }
            if !storages.is_empty() {
                SidebarSection { title: "Storages",
                    for st in storages.iter() {
                        {
                            let locs: Vec<&space::Location> = locations.iter().filter(|l| l.storage == st.id).collect();
                            let ic = match st.scheme.as_str() {
                                "s3" => "Drive-AmazonS3",
                                "webdav" => "Drive-DAV",
                                _ => "HDD",
                            };
                            let first = locs.first().map_or(Place::Devices, |l| Place::Location { loc: l.id.clone(), path: String::new() });
                            rsx! {
                                Fragment { key: "{st.id}",
                                    PlaceRow { at: here(first), label: st.name.clone(), icon: ic, live: up.contains(&st.device) }
                                    for l in locs {
                                        PlaceRow {
                                            key: "{l.id}",
                                            at: here(Place::Location { loc: l.id.clone(), path: String::new() }),
                                            label: l.name.clone(),
                                            icon: "LocationReplica",
                                            depth: 1,
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
            SidebarSection { title: "Kinds",
                for k in SIDEBAR_KINDS {
                    {
                        let kind = kinds.get(k);
                        rsx! {
                            PlaceRow { key: "{k}", at: here(Place::Kind(k.into())), label: plural(&kind.name), icon: kind.icon.clone() }
                        }
                    }
                }
            }
            crate::tags::SidebarTags { space: space.clone() }
            if !apps.is_empty() {
                SidebarSection { title: "Apps",
                    for a in apps {
                        PlaceRow { key: "{a}", at: here(Place::App(a.clone())), label: app_label(&a), icon: "Widget" }
                    }
                }
            }
            SidebarSection { title: "Space",
                PlaceRow { at: here(Place::Protection), label: "Protection", icon: "Lock" }
                PlaceRow { at: here(Place::Devices), label: "Devices & storage", icon: "Devices" }
                PlaceRow { at: here(Place::Sites), label: "Sites", icon: "Globe" }
                crate::rules::SidebarRules {}
            }
            SidebarSection { title: "Spine",
                PlaceRow { at: here(Place::Inbox), label: "Inbox", icon: "Document" }
                PlaceRow { at: here(Place::Calendar), label: "Calendar", icon: "Book" }
                PlaceRow { at: here(Place::Notes), label: "Notes", icon: "Text_txt" }
                if (nav.debug)() {
                    PlaceRow { at: here(Place::Everything), label: "Raw documents", icon: "Database" }
                }
            }
        }
    }
}

/// Projects other orgs have shared with this one.
#[component]
fn SharedWithUs(org: String) -> Element {
    let o = org.clone();
    let shared = use_resource(use_reactive!(|o| async move {
        get(format!("/shared?org={}", crate::browser::enc(&o)))
            .await
            .and_then(|v| v["shared"].as_array().cloned())
            .unwrap_or_default()
    }));
    let list = shared().unwrap_or_default();
    rsx! {
        for x in list {
            {
                let owner = s(&x, "org");
                let p = &x["project"];
                let (id, name, kind) = (s(p, "id"), s(p, "name"), s(p, "kind"));
                rsx! {
                    PlaceRow {
                        key: "{owner}/{id}",
                        at: At { place: Place::Project(id.clone()), within: Some(owner.clone()) },
                        label: name,
                        icon: project_icon(&kind),
                        note: owner.clone(),
                        title: "Shared by {owner}",
                    }
                }
            }
        }
    }
}

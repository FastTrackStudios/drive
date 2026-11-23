//! Drive on a phone, on app-shell's phone layout: the tab pill of Drive's
//! places, the round button (upload, a new folder, a new project), the
//! uploads' progress docked over them, the inspector as a bottom sheet,
//! and Quick Look full screen, swiped between files. Everything here is
//! what a phone adds; the desktop browser is `shell.rs` and `browser.rs`.

use app_shell::gestures::{Detent, Swipe};
use app_shell::ui::{
    Destination, MenuItem, PrimaryAction, Sheet, SheetSpec, Shell, Swipeable, icons, use_shell,
};
use dioxus::html::FileData;
use dioxus::prelude::*;
use drive_dioxus::{Doc, use_doc, use_drive};
use drive_files::{files_doc, space, space_doc, tree};
use loro::TreeID;

use crate::browser::{Inspector, project_icon};
use crate::nav::{InTab, Nav, Navs, Place};

/// A tab's navigation is the same as another's when it is the same tab's
/// (what a component given one compares).
impl PartialEq for Nav {
    fn eq(&self, other: &Self) -> bool {
        self.pane == other.pane
    }
}

/// A phone's look: the inspector in its sheet, Quick Look full screen,
/// the uploads' accessory, the naming and projects sheets.
pub const STYLE: Asset = asset!("/assets/mobile.css");

// ── uploads ─────────────────────────────────────────────────────────────

/// What is being uploaded, for the accessory over the tab pill (and the
/// status line beside a folder's Upload button).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct UploadState {
    /// Files in this batch, and how many are through.
    pub total: usize,
    pub done: usize,
    /// The file going now (empty between batches).
    pub current: String,
    pub failed: usize,
    /// What the last batch came to.
    pub said: String,
}

impl UploadState {
    pub fn busy(&self) -> bool {
        self.done < self.total
    }
}

/// The uploads, in context for the whole app.
#[derive(Clone, Copy)]
pub struct Uploads(pub Signal<UploadState>);

pub fn use_uploads_provider() -> Uploads {
    use_context_provider(|| Uploads(Signal::new(UploadState::default())))
}

pub fn use_uploads() -> Uploads {
    use_context::<Uploads>()
}

/// Upload `picked` into the folder `cur` of the space whose tree is
/// `files` (and registry `reg`), telling `uploads` as each goes.
pub async fn upload_files(
    files: Doc,
    reg: Doc,
    cur: Option<TreeID>,
    picked: Vec<FileData>,
    uploads: Uploads,
) {
    let mut state = uploads.0;
    state.set(UploadState {
        total: picked.len(),
        ..UploadState::default()
    });
    let mut failed = 0;
    for file in picked {
        let name = file.name();
        state.with_mut(|s| s.current = name.clone());
        let ok = match file.read_bytes().await {
            Ok(bytes) => send(&files, &reg, cur, &name, &bytes).await,
            Err(_) => false,
        };
        if !ok {
            failed += 1;
        }
        state.with_mut(|s| {
            s.done += 1;
            s.failed = failed;
        });
    }
    state.with_mut(|s| {
        s.current.clear();
        s.said = match (s.total, failed) {
            (n, 0) => format!("Uploaded {n} file{}", if n == 1 { "" } else { "s" }),
            (n, f) => format!("Uploaded {} of {n}; {f} refused", n - f),
        };
    });
}

/// One file to the node and into the tree; whether it went.
async fn send(files: &Doc, reg: &Doc, cur: Option<TreeID>, name: &str, bytes: &[u8]) -> bool {
    let hash = drive_files::hash::of_bytes(bytes);
    let sent =
        crate::host::request("PUT", &format!("/blobs/{hash}"), None, Some(bytes.to_vec())).await;
    match sent {
        Some((status, _)) if (200..300).contains(&status) => {
            files.edit(|d| {
                let id = match tree::lookup(d, cur, name) {
                    Some(x) if !x.dir => Some(x.id),
                    Some(_) => None,
                    None => tree::create_file(d, cur, name).ok(),
                };
                if let Some(id) = id {
                    let _ = tree::set_content(d, id, bytes.len() as u64, &hash);
                }
            });
            reg.edit(|d| {
                let _ = space::set_held(d, "node", &hash, Some("stored"));
            });
            true
        }
        _ => false,
    }
}

/// The uploads' progress: over the tab pill on a phone, under the content
/// on a desktop, while a batch goes and a moment after.
#[component]
pub fn UploadAccessory() -> Element {
    let s = (use_uploads().0)();
    let pct = if s.total == 0 {
        0.0
    } else {
        s.done as f64 * 100.0 / s.total as f64
    };
    let line = if s.busy() {
        format!("Uploading {} of {} · {}", s.done + 1, s.total, s.current)
    } else {
        s.said.clone()
    };
    rsx! {
        div { class: "uploads",
            img { class: "upicon", src: crate::browser::icon("Folder"), alt: "" }
            div { class: "upmid",
                span { class: "upline", "{line}" }
                if s.busy() {
                    div { class: "upbar", i { style: "width: {pct}%" } }
                }
            }
        }
    }
}

/// Whether the accessory shows: a batch going.
pub fn uploads_showing(u: Uploads) -> bool {
    u.0.read().busy()
}

/// The hidden file field the round button opens (a phone has no folder
/// toolbar to put an Upload button in).
#[component]
pub fn UploadField() -> Element {
    let navs = use_context::<Navs>();
    let space = use_drive().org.clone();
    let files = use_doc(files_doc(&space));
    let reg = use_doc(space_doc(&space));
    let uploads = use_uploads();
    rsx! {
        input {
            id: "drive-upload",
            r#type: "file",
            multiple: true,
            style: "display: none;",
            onchange: move |e: FormEvent| {
                // Into the folder on show, else the Drive's top.
                let nav = navs.active_now();
                let cur = match &*nav.place.peek() {
                    Place::Drive(Some(f)) => tree::parse_id(f),
                    _ => None,
                };
                if !matches!(&*nav.place.peek(), Place::Drive(_)) {
                    nav.go_home(Place::Drive(None));
                }
                let (files, reg) = (files.clone(), reg.clone());
                let picked = e.files();
                spawn(upload_files(files, reg, cur, picked, uploads));
            },
        }
    }
}

// ── the pill and the round button ───────────────────────────────────────

/// Drive's primary places on a phone.
///
/// Drive (the space's own files), Search (the Overview, where a query
/// looks through everything), Tags, Projects. The rest (computers,
/// kinds, apps, the spine) is in the drawer.
pub fn destinations() -> Vec<Destination> {
    vec![
        Destination::new("drive", "Drive", icons::FOLDER),
        Destination::new("search", "Search", icons::SEARCH),
        Destination::new("tags", "Tags", icons::TAG),
        Destination::new("projects", "Projects", icons::LAYERS),
    ]
}

/// Which destination a place belongs to.
pub fn destination_of(p: &Place) -> Option<String> {
    let d = match p {
        Place::Drive(_)
        | Place::Adopted(_)
        | Place::Location { .. }
        | Place::Volume { .. }
        | Place::Kind(_)
        | Place::App(_) => "drive",
        Place::Overview | Place::AllOrgs | Place::Everything => "search",
        Place::Tags | Place::Tag(_) => "tags",
        Place::Project(_) => "projects",
        _ => return None,
    };
    Some(d.to_string())
}

/// Go to a destination; Projects opens the list of projects (Drive has no
/// place that lists them: the sidebar does).
pub fn go_destination(shell: Shell, nav: Nav, id: &str, projects: SheetSpec) {
    shell.close_overlays();
    match id {
        "drive" => nav.go_home(Place::Drive(None)),
        "search" => {
            nav.go_home(Place::Overview);
            focus_search();
        }
        "tags" => nav.go_home(Place::Tags),
        "projects" => {
            shell.open_sheet(projects);
        }
        _ => {}
    }
}

fn focus_search() {
    crate::host::eval(
        "(function(){var f=function(){var el=document.querySelector('.as-search input');if(el){el.focus();return true;}return false;};if(!f()){requestAnimationFrame(function(){if(!f()){setTimeout(f,50);}});}})()",
    );
}

/// The round button: upload, a new folder, a new project (a menu: an
/// action sheet on a phone).
pub fn new_action(
    shell_slot: CopyValue<Option<Shell>>,
    mut naming: Signal<Option<Naming>>,
) -> PrimaryAction {
    PrimaryAction::new("New", icons::PLUS, move |()| {
        let Some(shell) = *shell_slot.peek() else {
            return;
        };
        shell.menu(
            0.0,
            0.0,
            vec![
                MenuItem::call(
                    "Upload files",
                    Callback::new(|()| {
                        // Synchronously, in the press, so the browser opens
                        // its picker.
                        crate::host::eval("document.getElementById('drive-upload').click()");
                    }),
                )
                .icon(icons::EXTERNAL),
                MenuItem::call(
                    "New folder",
                    Callback::new(move |()| naming.set(Some(Naming::Folder))),
                )
                .icon(icons::FOLDER),
                MenuItem::call(
                    "New project",
                    Callback::new(move |()| naming.set(Some(Naming::Project))),
                )
                .icon(icons::LAYERS),
            ],
        );
    })
}

/// What the naming sheet makes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Naming {
    Folder,
    Project,
}

/// A sheet asking for a name, then making the folder (in the folder on
/// show) or the project.
#[component]
pub fn NamingSheet(naming: Signal<Option<Naming>>) -> Element {
    let space = use_drive().org.clone();
    let files = use_doc(files_doc(&space));
    let reg = use_doc(space_doc(&space));
    let open = naming().is_some();
    let what = naming();
    let render = Callback::new(move |()| {
        let files = files.clone();
        let reg = reg.clone();
        rsx! { NameForm { naming, files, reg } }
    });
    rsx! {
        Sheet {
            open,
            title: match what { Some(Naming::Project) => "New project", _ => "New folder" },
            detent: Detent::Fit,
            detents: vec![Detent::Fit],
            render,
            on_close: move |()| naming.set(None),
        }
    }
}

#[component]
fn NameForm(naming: Signal<Option<Naming>>, files: Doc, reg: Doc) -> Element {
    let navs = use_context::<Navs>();
    let mut name = use_signal(String::new);
    let shell = use_shell();
    let mut make = {
        let (files, reg) = (files.clone(), reg.clone());
        move || {
            let n = name().trim().to_string();
            if n.is_empty() {
                return;
            }
            let nav = navs.active_now();
            match naming() {
                Some(Naming::Folder) => {
                    let cur = match &*nav.place.peek() {
                        Place::Drive(Some(f)) => tree::parse_id(f),
                        _ => None,
                    };
                    files.edit(|d| {
                        let _ = tree::mkdir(d, cur, &n);
                    });
                    if !matches!(&*nav.place.peek(), Place::Drive(_)) {
                        nav.go_home(Place::Drive(None));
                    }
                    shell.toast(format!("Made the folder {n}"));
                }
                Some(Naming::Project) => {
                    let id =
                        reg.edit(|d| drive_files::projects::add_project(d, &n, "Project").ok());
                    if let Some(id) = id {
                        nav.go_home(Place::Project(id));
                    }
                    shell.toast(format!("Made the project {n}"));
                }
                None => {}
            }
            name.set(String::new());
            naming.set(None);
        }
    };
    let mut make2 = make.clone();
    rsx! {
        div { class: "nameform",
            input {
                class: "nameinput",
                placeholder: "Name",
                value: "{name}",
                autofocus: true,
                oninput: move |e| name.set(e.value()),
                onkeydown: move |e: KeyboardEvent| {
                    if e.key().to_string() == "Enter" {
                        make();
                    }
                },
            }
            button { class: "primary wide", onclick: move |_| make2(), "Make" }
        }
    }
}

/// The projects, as a sheet's rows (the Projects destination).
pub fn projects_sheet(space: String) -> SheetSpec {
    SheetSpec::new(move |()| rsx! { ProjectList { space: space.clone() } })
        .title("Projects")
        .detents(Detent::Medium, &[Detent::Medium, Detent::Large])
}

#[component]
fn ProjectList(space: String) -> Element {
    let navs = use_context::<Navs>();
    let shell = use_shell();
    let reg = use_doc(space_doc(&space));
    let projects = drive_files::projects::projects(&reg.loro());
    rsx! {
        div { class: "sheetlist",
            if projects.is_empty() {
                p { class: "hint", "No projects yet: the round button makes one." }
            }
            for p in projects {
                button {
                    key: "{p.id}",
                    class: "sheetrow",
                    onclick: {
                        let id = p.id.clone();
                        move |_| {
                            shell.close_top_sheet();
                            navs.active_now().go_home(Place::Project(id.clone()));
                        }
                    },
                    img { class: "sicon", src: crate::browser::icon(project_icon(&p.kind)), alt: "" }
                    span { "{p.name}" }
                }
            }
        }
    }
}

// ── the inspector, Quick Look ───────────────────────────────────────────

/// The inspector as a bottom sheet while a file is picked (a phone): the
/// tab's own, rendered at the shell's root with the tab's navigation.
#[component]
pub fn InspectorSheet(nav: Nav, space: String, open: bool) -> Element {
    let render = Callback::new(move |()| {
        rsx! {
            WithTab { nav,
                div { class: "insheet", Inspector { space: space.clone() } }
            }
        }
    });
    let mut selected = nav.selected;
    rsx! {
        Sheet {
            open,
            render,
            detent: Detent::Medium,
            detents: vec![Detent::Medium, Detent::Large],
            on_close: move |()| selected.set(None),
        }
    }
}

/// Gives what is under it the tab's navigation (a sheet renders at the
/// shell's root, outside the tab).
#[component]
pub fn WithTab(nav: Nav, children: Element) -> Element {
    use_context_provider(|| InTab(nav));
    rsx! { {children} }
}

/// Quick Look's header on a phone: swipe left for the next file, right for
/// the one before, down to close.
#[component]
pub fn QuickLookSwipe(nav: Nav, children: Element) -> Element {
    let mut nav = nav;
    rsx! {
        Swipeable {
            style: "flex: none; min-width: 0; display: flex;".to_string(),
            on_swipe: move |s: Swipe| match s {
                Swipe::Left => crate::browser::quick_look_step(nav, true),
                Swipe::Right => crate::browser::quick_look_step(nav, false),
                Swipe::Down => nav.preview.set(false),
                Swipe::Up => {}
            },
            {children}
        }
    }
}

/// What lives inside the shell for the app above it: it hands the app the
/// window's [`Shell`] (the round button and the pill's callbacks are the
/// app's, given before the shell exists), and draws the naming sheet.
#[component]
pub fn MobileHost(shell_slot: CopyValue<Option<Shell>>, naming: Signal<Option<Naming>>) -> Element {
    let shell = use_shell();
    let mut slot = shell_slot;
    if slot.peek().is_none() {
        slot.set(Some(shell));
    }
    // A batch that finishes says how it went.
    let uploads = use_uploads();
    let mut was_busy = use_hook(|| CopyValue::new(false));
    use_effect(move || {
        let s = uploads.0.read();
        let busy = s.busy();
        if *was_busy.peek() && !busy && !s.said.is_empty() {
            shell.toast(s.said.clone());
        }
        was_busy.set(busy);
    });
    rsx! {
        NamingSheet { naming }
        UploadField {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_place_lights_the_destination_it_belongs_to() {
        assert_eq!(
            destination_of(&Place::Drive(None)).as_deref(),
            Some("drive")
        );
        assert_eq!(
            destination_of(&Place::Kind("audio".into())).as_deref(),
            Some("drive")
        );
        assert_eq!(
            destination_of(&Place::Volume {
                device: "d".into(),
                root: "/".into(),
                path: String::new(),
            })
            .as_deref(),
            Some("drive")
        );
        assert_eq!(destination_of(&Place::Overview).as_deref(), Some("search"));
        assert_eq!(
            destination_of(&Place::Tag("t".into())).as_deref(),
            Some("tags")
        );
        assert_eq!(
            destination_of(&Place::Project("p".into())).as_deref(),
            Some("projects")
        );
        assert_eq!(
            destination_of(&Place::Inbox),
            None,
            "the drawer's places light none"
        );
        let ids: Vec<String> = destinations().into_iter().map(|d| d.id).collect();
        assert_eq!(ids, ["drive", "search", "tags", "projects"]);
    }

    #[test]
    fn an_upload_is_busy_until_every_file_is_through() {
        let mut s = UploadState {
            total: 2,
            ..UploadState::default()
        };
        assert!(s.busy());
        s.done = 2;
        assert!(!s.busy());
        assert!(!UploadState::default().busy());
    }
}

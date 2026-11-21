//! Where each tab is: its place, its history, what is picked in it, how it
//! shows it. Every tab of the window has a [`Nav`] of its own; the sidebar
//! and the top bar act on the active tab's.
//!
//! A tab's place and view are written into its pane (`Pane::state`), so the
//! layout saved in the browser opens every tab where it was.

use std::collections::HashMap;

use app_shell::{Pane, PaneId, Workspace};
use dioxus::prelude::*;
use serde::{Deserialize, Serialize};

use crate::browser::Item;

/// Where the browser is.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Place {
    Overview,
    /// The space's synced tree, at a folder (`None`: its root).
    Drive(Option<String>),
    /// A location's index, at a path inside it.
    Location {
        loc: String,
        path: String,
    },
    /// A folder on a computer's disk, browsed live (`crate::browse`).
    Volume {
        device: String,
        root: String,
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
    /// Static sites built from folders.
    Sites,
    Inbox,
    Calendar,
    Notes,
    Everything,
    /// Everything carrying a tag (or a tag under it).
    Tag(String),
    /// The tag manager.
    Tags,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub enum View {
    Grid,
    List,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub enum Sort {
    Name,
    Size,
    Modified,
}

/// A place in an org: `within` names another org whose project (shared
/// with this one) is being viewed; `None`: this org.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct At {
    pub place: Place,
    pub within: Option<String>,
}

/// What a tab keeps in its pane: where it is and how it shows it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TabState {
    pub at: At,
    pub view: View,
    pub sort: (Sort, bool),
}

impl Default for TabState {
    fn default() -> Self {
        Self {
            at: At {
                place: Place::Drive(None),
                within: None,
            },
            view: View::Grid,
            sort: (Sort::Name, false),
        }
    }
}

impl TabState {
    pub fn parse(state: &str) -> Self {
        serde_json::from_str(state).unwrap_or_default()
    }

    pub fn encode(&self) -> String {
        serde_json::to_string(self).unwrap_or_default()
    }
}

/// The window's layout, and where it is kept (the browser's storage, per
/// org).
#[derive(Clone, Copy)]
pub struct Store {
    pub layout: Signal<Workspace>,
    pub key: CopyValue<String>,
}

impl Store {
    pub fn save(&self) {
        let json = serde_json::to_string(&*self.layout.peek()).unwrap_or_default();
        storage_set(&self.key.peek(), &json);
    }
}

pub fn storage() -> Option<web_sys::Storage> {
    web_sys::window()?.local_storage().ok()?
}

pub fn storage_get(key: &str) -> Option<String> {
    storage()?.get_item(key).ok()?
}

pub fn storage_set(key: &str, value: &str) {
    if let Some(s) = storage()
        && s.set_item(key, value).is_err()
    {
        dioxus::logger::tracing::warn!(storage.key = key, "the browser refused to keep the layout");
    }
}

/// One tab's navigation.
#[derive(Clone, Copy)]
pub struct Nav {
    /// The tab's pane.
    pub pane: CopyValue<String>,
    store: Store,
    pub place: Signal<Place>,
    back: Signal<Vec<At>>,
    fwd: Signal<Vec<At>>,
    pub selected: Signal<Option<Item>>,
    pub query: Signal<String>,
    pub view: Signal<View>,
    pub sort: Signal<(Sort, bool)>,
    pub inspector: Signal<bool>,
    /// The picked file, full screen.
    pub preview: Signal<bool>,
    /// The items on screen, in their order (what the arrow keys walk).
    pub visible: CopyValue<Vec<Item>>,
    /// Shared by every tab: the Raw documents place shows.
    pub debug: Signal<bool>,
    /// Another org whose project (shared with this one) is being viewed;
    /// `None`: this org.
    pub within: Signal<Option<String>>,
}

impl Nav {
    fn new(pane: &str, state: TabState, store: Store, debug: Signal<bool>, owner: ScopeId) -> Nav {
        Nav {
            pane: CopyValue::new_in_scope(pane.to_string(), owner),
            store,
            place: Signal::new_in_scope(state.at.place, owner),
            back: Signal::new_in_scope(Vec::new(), owner),
            fwd: Signal::new_in_scope(Vec::new(), owner),
            selected: Signal::new_in_scope(None, owner),
            query: Signal::new_in_scope(String::new(), owner),
            view: Signal::new_in_scope(state.view, owner),
            sort: Signal::new_in_scope(state.sort, owner),
            inspector: Signal::new_in_scope(true, owner),
            preview: Signal::new_in_scope(false, owner),
            visible: CopyValue::new_in_scope(Vec::new(), owner),
            debug,
            within: Signal::new_in_scope(state.at.within, owner),
        }
    }

    /// Where the tab is now.
    pub fn at(&self) -> At {
        At {
            place: self.place.peek().clone(),
            within: self.within.peek().clone(),
        }
    }

    /// What the tab keeps, as it is now.
    pub fn state(&self) -> TabState {
        TabState {
            at: self.at(),
            view: *self.view.peek(),
            sort: *self.sort.peek(),
        }
    }

    /// Write the tab's state into its pane and keep the layout.
    pub fn remember(&self) {
        let id = PaneId::new(self.pane.peek().clone());
        let state = self.state().encode();
        let mut layout = self.store.layout;
        let changed = layout.peek().pane(&id).is_some_and(|p| p.state != state);
        if changed {
            if let Some(p) = layout.write().pane_mut(&id) {
                p.state = state;
            }
            self.store.save();
        }
    }

    fn arrive(mut self, to: At) {
        let here = self.at();
        if here != to {
            self.back.write().push(here);
            self.fwd.write().clear();
            if *self.place.peek() != to.place {
                self.place.set(to.place);
            }
            if *self.within.peek() != to.within {
                self.within.set(to.within);
            }
        }
        self.selected.set(None);
        self.preview.set(false);
        if !self.query.peek().is_empty() {
            self.query.set(String::new());
        }
        self.remember();
    }

    /// Go somewhere in the org the tab is in.
    pub fn go(self, p: Place) {
        let within = self.within.peek().clone();
        self.arrive(At { place: p, within });
    }

    /// Go somewhere in this org (the sidebar: leaves any other org's project).
    pub fn go_home(self, p: Place) {
        self.arrive(At {
            place: p,
            within: None,
        });
    }

    /// Go somewhere in another org (a project it shared with this one);
    /// moving around inside it (its parts, their folders) stays there.
    pub fn go_in(self, org: &str, p: Place) {
        self.arrive(At {
            place: p,
            within: Some(org.to_string()),
        });
    }

    pub fn can_step(&self, forward: bool) -> bool {
        if forward {
            !self.fwd.read().is_empty()
        } else {
            !self.back.read().is_empty()
        }
    }

    pub fn step(mut self, forward: bool) {
        let (mut from, mut to) = if forward {
            (self.fwd, self.back)
        } else {
            (self.back, self.fwd)
        };
        let Some(next) = from.write().pop() else {
            return;
        };
        to.write().push(self.at());
        self.place.set(next.place);
        self.within.set(next.within);
        self.selected.set(None);
        self.preview.set(false);
        self.remember();
    }

    pub fn set_view(mut self, v: View) {
        self.view.set(v);
        self.remember();
    }

    pub fn set_sort(mut self, s: (Sort, bool)) {
        self.sort.set(s);
        self.remember();
    }
}

/// Every tab's [`Nav`], made when a tab is first drawn and kept for the
/// window's life (a reopened tab comes back where it was).
#[derive(Clone, Copy)]
pub struct Navs {
    pub store: Store,
    map: CopyValue<HashMap<String, Nav>>,
    owner: ScopeId,
    debug: Signal<bool>,
}

impl Navs {
    pub fn new(store: Store) -> Navs {
        Navs {
            store,
            map: CopyValue::new(HashMap::new()),
            owner: dioxus::core::current_scope_id(),
            debug: Signal::new(false),
        }
    }

    /// The tab `pane`'s navigation (made from its pane's state the first time).
    pub fn get(&self, pane: &str) -> Nav {
        if let Some(n) = self.map.peek().get(pane) {
            return *n;
        }
        let state = self
            .store
            .layout
            .peek()
            .pane(&PaneId::new(pane))
            .map(|p| TabState::parse(&p.state))
            .unwrap_or_default();
        let nav = Nav::new(pane, state, self.store, self.debug, self.owner);
        let mut map = self.map;
        map.write().insert(pane.to_string(), nav);
        nav
    }

    /// The active tab's navigation (subscribing to which tab that is).
    pub fn active(&self) -> Nav {
        let id = active_pane(&self.store.layout.read());
        self.get(&id)
    }

    /// [`Self::active`], not subscribing.
    pub fn active_now(&self) -> Nav {
        let id = active_pane(&self.store.layout.peek());
        self.get(&id)
    }
}

fn active_pane(ws: &Workspace) -> String {
    ws.active
        .clone()
        .or_else(|| ws.panes.first().map(|p| p.id.clone()))
        .map(|p| p.0)
        .unwrap_or_default()
}

/// The tab a component is drawn in, when it is in one.
#[derive(Clone, Copy)]
pub struct InTab(pub Nav);

/// The navigation a component acts on: its tab's, or (in the sidebar and the
/// top bar) the active tab's.
pub fn use_nav() -> Nav {
    match try_consume_context::<InTab>() {
        Some(InTab(nav)) => nav,
        None => consume_context::<Navs>().active(),
    }
}

/// The kind of tab Drive draws: a place.
pub const TAB_KIND: &str = "drive.place";

/// A new tab's pane, at `at`.
pub fn new_pane(at: At, title: &str) -> Pane {
    let id = format!("tab-{}", js_sys::Date::now() as u64);
    let mut pane = Pane::new(id, TAB_KIND, title);
    pane.state = TabState {
        at,
        ..TabState::default()
    }
    .encode();
    pane
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_tab_state_reads_back_what_it_wrote() {
        let state = TabState {
            at: At {
                place: Place::Location {
                    loc: "l1".into(),
                    path: "Drums/Kick".into(),
                },
                within: Some("other-org".into()),
            },
            view: View::List,
            sort: (Sort::Size, true),
        };
        assert_eq!(TabState::parse(&state.encode()), state);
    }

    #[test]
    fn a_pane_with_no_state_opens_at_the_drives_root() {
        let state = TabState::parse("");
        assert_eq!(state.at.place, Place::Drive(None));
        assert_eq!(state.at.within, None);
        assert_eq!(state.view, View::Grid);
        assert_eq!(TabState::parse("{not json"), TabState::default());
    }
}

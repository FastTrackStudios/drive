//! Drive's spine, typed: the one inbox of tasks and the one calendar that
//! every app writes to, in one document ([`SPINE`]). An app adds a task or
//! an event with a link back to its own document; the Drive app shows
//! them all.
//!
//! Layout: `inbox` is a movable list of task maps (so it can be
//! reordered); `events` is a map of event id → event map (so an app can
//! update its own event by id).

pub mod history;
pub mod kinds;
pub mod schema;

use loro::{LoroDoc, LoroMap, LoroMovableList, LoroValue, ValueOrContainer};

/// The spine document's id within an org (apps open it through
/// `use_doc`, which adds the org).
pub const SPINE: &str = "drive:spine";

/// The spine document's full id in an org.
pub fn spine_doc(org: &str) -> String {
    format!("{org}/{SPINE}")
}

/// A task in the inbox.
#[derive(Clone, Debug, PartialEq)]
pub struct Task {
    pub id: String,
    pub title: String,
    pub done: bool,
    /// The app that made it (`studio`, `meals`, `drive`, …).
    pub app: String,
    /// Where it leads: a path on the node (`/studio/set/…`), or empty.
    pub link: String,
    pub by: String,
}

/// An event in the calendar.
#[derive(Clone, Debug, PartialEq)]
pub struct Event {
    pub id: String,
    pub title: String,
    pub app: String,
    pub link: String,
    /// `YYYY-MM-DD`.
    pub date: String,
    /// Free text: `7:30 PM`, `all day`.
    pub time: String,
    pub place: String,
}

fn s(m: &LoroMap, k: &str) -> String {
    match m.get(k) {
        Some(ValueOrContainer::Value(LoroValue::String(v))) => v.to_string(),
        _ => String::new(),
    }
}

fn b(m: &LoroMap, k: &str) -> bool {
    matches!(
        m.get(k),
        Some(ValueOrContainer::Value(LoroValue::Bool(true)))
    )
}

fn inbox(doc: &LoroDoc) -> LoroMovableList {
    doc.get_movable_list("inbox")
}

fn events_map(doc: &LoroDoc) -> LoroMap {
    doc.get_map("events")
}

fn child_map(v: Option<ValueOrContainer>) -> Option<LoroMap> {
    match v {
        Some(ValueOrContainer::Container(c)) => c.into_map().ok(),
        _ => None,
    }
}

/// Every task, in order.
pub fn tasks(doc: &LoroDoc) -> Vec<Task> {
    let list = inbox(doc);
    (0..list.len())
        .filter_map(|i| child_map(list.get(i)))
        .map(|m| Task {
            id: s(&m, "id"),
            title: s(&m, "title"),
            done: b(&m, "done"),
            app: s(&m, "app"),
            link: s(&m, "link"),
            by: s(&m, "by"),
        })
        .collect()
}

/// Add a task at the top of the inbox; returns its id.
pub fn add_task(doc: &LoroDoc, title: &str, app: &str, link: &str, by: &str) -> String {
    let id = new_id(doc, "t");
    let list = inbox(doc);
    if let Ok(m) = list.insert_container(0, LoroMap::new()) {
        let _ = m.insert("id", id.as_str());
        let _ = m.insert("title", title);
        let _ = m.insert("done", false);
        let _ = m.insert("app", app);
        let _ = m.insert("link", link);
        let _ = m.insert("by", by);
    }
    id
}

fn task_index(doc: &LoroDoc, id: &str) -> Option<(usize, LoroMap)> {
    let list = inbox(doc);
    (0..list.len()).find_map(|i| {
        child_map(list.get(i))
            .filter(|m| s(m, "id") == id)
            .map(|m| (i, m))
    })
}

pub fn set_done(doc: &LoroDoc, id: &str, done: bool) {
    if let Some((_, m)) = task_index(doc, id) {
        let _ = m.insert("done", done);
    }
}

pub fn rename_task(doc: &LoroDoc, id: &str, title: &str) {
    if let Some((_, m)) = task_index(doc, id) {
        let _ = m.insert("title", title);
    }
}

pub fn remove_task(doc: &LoroDoc, id: &str) {
    if let Some((i, _)) = task_index(doc, id) {
        let _ = inbox(doc).delete(i, 1);
    }
}

/// Move a task one place up (`-1`) or down (`1`).
pub fn move_task(doc: &LoroDoc, id: &str, by: i32) {
    let list = inbox(doc);
    if let Some((i, _)) = task_index(doc, id) {
        let to = i as i32 + by;
        if to >= 0 && (to as usize) < list.len() {
            let _ = list.mov(i, to as usize);
        }
    }
}

/// Every event, by date and time.
pub fn events(doc: &LoroDoc) -> Vec<Event> {
    let map = events_map(doc);
    let mut out: Vec<Event> = map
        .keys()
        .filter_map(|k| child_map(map.get(&k)))
        .map(|m| Event {
            id: s(&m, "id"),
            title: s(&m, "title"),
            app: s(&m, "app"),
            link: s(&m, "link"),
            date: s(&m, "date"),
            time: s(&m, "time"),
            place: s(&m, "place"),
        })
        .collect();
    out.sort_by(|a, b| (a.date.as_str(), a.time.as_str()).cmp(&(b.date.as_str(), b.time.as_str())));
    out
}

/// Add or replace an event by its id (an app uses a stable id, such as
/// `studio:set:cya_2026-11-19`, so it updates its own event).
pub fn put_event(doc: &LoroDoc, e: &Event) {
    let map = events_map(doc);
    if let Ok(m) = map.ensure_mergeable_map(&e.id) {
        for (k, v) in [
            ("id", &e.id),
            ("title", &e.title),
            ("app", &e.app),
            ("link", &e.link),
            ("date", &e.date),
            ("time", &e.time),
            ("place", &e.place),
        ] {
            if s(&m, k) != *v {
                let _ = m.insert(k, v.as_str());
            }
        }
    }
}

pub fn remove_event(doc: &LoroDoc, id: &str) {
    let _ = events_map(doc).delete(id);
}

/// A new id, unique across peers: this peer's id and its next counter.
pub fn new_id(doc: &LoroDoc, prefix: &str) -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    // The counter only moves on commit, so several ids in one edit also
    // take a number of their own.
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    format!(
        "{prefix}{:x}-{:x}-{n:x}",
        doc.peer_id(),
        doc.oplog_vv().get(&doc.peer_id()).copied().unwrap_or(0)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tasks_and_events_round_trip() {
        let d = LoroDoc::new();
        let a = add_task(&d, "Book the drummer", "studio", "/studio/set/x", "Cody");
        d.commit();
        let b = add_task(&d, "Buy rice", "meals", "", "Mia");
        set_done(&d, &a, true);
        move_task(&d, &a, -1);
        let t = tasks(&d);
        assert_eq!(
            t.iter().map(|x| x.title.as_str()).collect::<Vec<_>>(),
            ["Book the drummer", "Buy rice"]
        );
        assert!(t[0].done && !t[1].done);
        assert_ne!(a, b);
        assert_ne!(new_id(&d, "x"), new_id(&d, "x"));
        put_event(
            &d,
            &Event {
                id: "e1".into(),
                title: "CYA".into(),
                app: "studio".into(),
                link: String::new(),
                date: "2026-11-19".into(),
                time: "7:30 PM".into(),
                place: "Chapel".into(),
            },
        );
        put_event(
            &d,
            &Event {
                id: "e1".into(),
                title: "CYA set".into(),
                app: "studio".into(),
                link: String::new(),
                date: "2026-11-19".into(),
                time: "7:30 PM".into(),
                place: "Chapel".into(),
            },
        );
        assert_eq!(events(&d).len(), 1);
        assert_eq!(events(&d)[0].title, "CYA set");
    }
}

//! Drive's spine, typed: the one inbox of tasks and the one calendar that
//! every app writes to, in one document ([`SPINE`]). An app adds a task or
//! an event with a link back to its own document; the Drive app shows
//! them all.
//!
//! Layout: `inbox` is a movable list of task maps (so it can be
//! reordered); `events` is a map of event id → event map (so an app can
//! update its own event by id).
//!
//! An event may repeat ([`Event::repeat`], an RRULE, [`recur`]): "every
//! Sunday, 10:30" is one event, and [`calendar`] lays out its meetings
//! over a range of dates, without the dates left out
//! ([`Event::except`]). An app's own event for one meeting (the set for
//! the 29th) names the recurring event and the date it stands for
//! ([`Event::series`], [`Event::occurrence`]) and shows in its place.

pub mod comments;
pub mod history;
pub mod kinds;
pub mod links;
pub mod pages;
pub mod recur;
pub mod schema;
pub mod tags;

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
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Event {
    pub id: String,
    pub title: String,
    pub app: String,
    pub link: String,
    /// `YYYY-MM-DD`; for a recurring event, its first date.
    pub date: String,
    /// Free text: `7:30 PM`, `all day`.
    pub time: String,
    pub place: String,
    /// How it repeats, as an RRULE (`FREQ=WEEKLY;BYDAY=SU`, [`recur`]);
    /// empty: it happens once.
    pub repeat: String,
    /// Dates of a recurring event left out (`YYYY-MM-DD`): no meeting then.
    pub except: Vec<String>,
    /// For one meeting of a recurring event: that event's id…
    pub series: String,
    /// …and the date of the meeting it stands for (it may be moved to
    /// another `date`). [`calendar`] shows it in place of that meeting.
    pub occurrence: String,
}

impl Event {
    /// A meeting [`calendar`] laid out from a recurring event (not stored:
    /// an app that wants to change it puts an event of its own with
    /// `series` and `occurrence`, or leaves the date out with
    /// [`skip_occurrence`]).
    pub fn is_generated(&self) -> bool {
        !self.series.is_empty() && self.id == occurrence_id(&self.series, &self.occurrence)
    }
}

/// The id [`calendar`] gives a recurring event's meeting.
pub fn occurrence_id(series: &str, date: &str) -> String {
    format!("{series}@{date}")
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
        .map(|m| {
            let mut except: Vec<String> = child_map(m.get("except"))
                .map(|x| x.keys().map(|k| k.to_string()).collect())
                .unwrap_or_default();
            except.sort();
            Event {
                id: s(&m, "id"),
                title: s(&m, "title"),
                app: s(&m, "app"),
                link: s(&m, "link"),
                date: s(&m, "date"),
                time: s(&m, "time"),
                place: s(&m, "place"),
                repeat: s(&m, "repeat"),
                except,
                series: s(&m, "series"),
                occurrence: s(&m, "occurrence"),
            }
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
            ("repeat", &e.repeat),
            ("series", &e.series),
            ("occurrence", &e.occurrence),
        ] {
            if s(&m, k) != *v {
                let _ = m.insert(k, v.as_str());
            }
        }
        // The dates left out, as a set (one added elsewhere meanwhile
        // merges, unless this replaces it).
        let has = child_map(m.get("except"));
        if !e.except.is_empty() || has.is_some() {
            if let Ok(x) = m.ensure_mergeable_map("except") {
                for k in x.keys().map(|k| k.to_string()).collect::<Vec<_>>() {
                    if !e.except.contains(&k) {
                        let _ = x.delete(&k);
                    }
                }
                for d in &e.except {
                    if x.get(d).is_none() {
                        let _ = x.insert(d, true);
                    }
                }
            }
        }
    }
}

/// Leave one meeting of a recurring event out (`on`), or put it back.
pub fn skip_occurrence(doc: &LoroDoc, id: &str, date: &str, on: bool) {
    let Some(m) = child_map(events_map(doc).get(id)) else {
        return;
    };
    if let Ok(x) = m.ensure_mergeable_map("except") {
        if on {
            let _ = x.insert(date, true);
        } else if x.get(date).is_some() {
            let _ = x.delete(date);
        }
    }
}

/// The calendar from `from` to `to` (`YYYY-MM-DD`, inclusive), by date and
/// time: each event once on its date, each recurring event's meetings laid
/// out ([`Event::is_generated`]) without the dates it leaves out, and an
/// app's own event for a meeting in that meeting's place. A rule that does
/// not read shows its event once, on its date.
pub fn calendar(doc: &LoroDoc, from: &str, to: &str) -> Vec<Event> {
    let all = events(doc);
    let replaced: std::collections::HashSet<(&str, &str)> = all
        .iter()
        .filter(|e| !e.series.is_empty())
        .map(|e| (e.series.as_str(), e.occurrence.as_str()))
        .collect();
    let within = |d: &str| d >= from && d <= to;
    let mut out = Vec::new();
    for e in &all {
        let rule = (!e.repeat.is_empty())
            .then(|| recur::Rule::parse(&e.repeat).ok())
            .flatten();
        let Some(rule) = rule else {
            if within(&e.date) {
                out.push(Event {
                    except: Vec::new(),
                    ..e.clone()
                });
            }
            continue;
        };
        for d in rule.dates(&e.date, from, to) {
            if e.except.contains(&d) || replaced.contains(&(e.id.as_str(), d.as_str())) {
                continue;
            }
            out.push(Event {
                id: occurrence_id(&e.id, &d),
                date: d.clone(),
                repeat: String::new(),
                except: Vec::new(),
                series: e.id.clone(),
                occurrence: d,
                ..e.clone()
            });
        }
    }
    out.sort_by(|a, b| (a.date.as_str(), a.time.as_str()).cmp(&(b.date.as_str(), b.time.as_str())));
    out
}

pub fn remove_event(doc: &LoroDoc, id: &str) {
    let _ = events_map(doc).delete(id);
}

/// Remove a recurring event and every meeting of it: the rule, and the
/// events that stand for one of its meetings (moved or renamed). Removing
/// one generated meeting only skips it ([`skip_occurrence`]).
pub fn remove_series(doc: &LoroDoc, series: &str) {
    let map = events_map(doc);
    for e in events(doc) {
        if e.id == series || e.series == series {
            let _ = map.delete(&e.id);
        }
    }
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
                ..Default::default()
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
                ..Default::default()
            },
        );
        assert_eq!(events(&d).len(), 1);
        assert_eq!(events(&d)[0].title, "CYA set");
    }

    #[test]
    fn a_recurring_event_shows_every_meeting_in_a_range() {
        let d = LoroDoc::new();
        // Sunday service, weekly from November.
        put_event(
            &d,
            &Event {
                id: "studio:setlist:main".into(),
                title: "Sunday service".into(),
                app: "studio".into(),
                date: "2026-11-01".into(),
                time: "10:30 AM".into(),
                place: "Sanctuary".into(),
                repeat: recur::Rule::weekly(&[6]).to_string(),
                except: vec!["2026-11-15".into()],
                ..Default::default()
            },
        );
        // The set for the 29th stands for that meeting, an hour earlier.
        put_event(
            &d,
            &Event {
                id: "studio:setlist:main_2026-11-29".into(),
                title: "Sunday service: Advent 1".into(),
                app: "studio".into(),
                link: "/studio/set/main_2026-11-29".into(),
                date: "2026-11-29".into(),
                time: "9:30 AM".into(),
                series: "studio:setlist:main".into(),
                occurrence: "2026-11-29".into(),
                ..Default::default()
            },
        );
        // A one-off in the range.
        put_event(
            &d,
            &Event {
                id: "e2".into(),
                title: "Rehearsal".into(),
                date: "2026-11-21".into(),
                time: "7:00 PM".into(),
                ..Default::default()
            },
        );
        d.commit();
        let cal = calendar(&d, "2026-11-08", "2026-12-06");
        let rows: Vec<(&str, &str, bool)> = cal
            .iter()
            .map(|e| (e.date.as_str(), e.title.as_str(), e.is_generated()))
            .collect();
        assert_eq!(
            rows,
            [
                ("2026-11-08", "Sunday service", true),
                // the 15th is left out
                ("2026-11-21", "Rehearsal", false),
                ("2026-11-22", "Sunday service", true),
                ("2026-11-29", "Sunday service: Advent 1", false),
                ("2026-12-06", "Sunday service", true),
            ]
        );
        assert_eq!(cal[0].id, "studio:setlist:main@2026-11-08");
        assert_eq!(cal[0].place, "Sanctuary");

        // Skip the 6th, then put the 15th back.
        skip_occurrence(&d, "studio:setlist:main", "2026-12-06", true);
        skip_occurrence(&d, "studio:setlist:main", "2026-11-15", false);
        let dates: Vec<String> = calendar(&d, "2026-11-08", "2026-12-06")
            .into_iter()
            .filter(|e| e.is_generated())
            .map(|e| e.date)
            .collect();
        assert_eq!(dates, ["2026-11-08", "2026-11-15", "2026-11-22"]);
        assert_eq!(events(&d)[0].except, ["2026-12-06"]);

        // A meeting moved to Saturday: the Sunday one is gone, the moved
        // one shows on its own date.
        put_event(
            &d,
            &Event {
                id: "moved".into(),
                title: "Service (moved)".into(),
                date: "2026-11-21".into(),
                series: "studio:setlist:main".into(),
                occurrence: "2026-11-22".into(),
                ..Default::default()
            },
        );
        let cal = calendar(&d, "2026-11-20", "2026-11-23");
        let mut titles: Vec<&str> = cal.iter().map(|e| e.title.as_str()).collect();
        titles.sort();
        assert_eq!(titles, ["Rehearsal", "Service (moved)"]);
    }

    #[test]
    fn removing_a_series_removes_every_meeting_and_keeps_the_rest() {
        let d = LoroDoc::new();
        put_event(
            &d,
            &Event {
                id: "s".into(),
                title: "Weekly".into(),
                date: "2026-11-01".into(),
                repeat: "FREQ=WEEKLY".into(),
                ..Default::default()
            },
        );
        put_event(
            &d,
            &Event {
                id: "moved".into(),
                title: "Weekly (moved)".into(),
                date: "2026-11-07".into(),
                series: "s".into(),
                occurrence: "2026-11-08".into(),
                ..Default::default()
            },
        );
        put_event(
            &d,
            &Event {
                id: "other".into(),
                title: "One-off".into(),
                date: "2026-11-10".into(),
                ..Default::default()
            },
        );
        assert!(calendar(&d, "2026-11-01", "2026-11-30").len() > 2);
        remove_series(&d, "s");
        let left: Vec<String> = calendar(&d, "2026-11-01", "2026-11-30")
            .into_iter()
            .map(|e| e.id)
            .collect();
        assert_eq!(left, ["other"]);
    }

}

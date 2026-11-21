//! The rules document: one per org, [`RULES`] (`<org>/drive:rules`), so
//! every device runs the same rules and every edit records who and when
//! (`drive_spine::history`).
//!
//! Layout: `rules`: rule id → { name, enabled, mode, scope, trigger,
//! when, actions, created, by }; scope, trigger, when and actions are JSON
//! (a rule's conditions are edited as a whole; name, on/off and mode are
//! separate keys, so switching a rule live on one device and renaming it
//! on another both land). `requests`: rule id → { at, live, by } ("Run
//! now" from the app or the command line; every device in scope answers
//! a request newer than the one it last answered).

use loro::{LoroDoc, LoroMap, LoroValue, ValueOrContainer};

use crate::model::{Action, Mode, Rule, Scope, Trigger, When};

/// The rules document's id within an org (`use_doc` adds the org).
pub const RULES: &str = "drive:rules";

pub fn rules_doc(org: &str) -> String {
    format!("{org}/{RULES}")
}

fn s(m: &LoroMap, k: &str) -> String {
    match m.get(k) {
        Some(ValueOrContainer::Value(LoroValue::String(v))) => v.to_string(),
        _ => String::new(),
    }
}

fn n(m: &LoroMap, k: &str) -> i64 {
    match m.get(k) {
        Some(ValueOrContainer::Value(LoroValue::I64(v))) => v,
        Some(ValueOrContainer::Value(LoroValue::Double(v))) => v as i64,
        _ => 0,
    }
}

fn b(m: &LoroMap, k: &str) -> bool {
    matches!(
        m.get(k),
        Some(ValueOrContainer::Value(LoroValue::Bool(true)))
    )
}

fn child(m: &LoroMap, k: &str) -> Option<LoroMap> {
    match m.get(k) {
        Some(ValueOrContainer::Container(c)) => c.into_map().ok(),
        _ => None,
    }
}

fn json<T: serde::de::DeserializeOwned + Default>(m: &LoroMap, k: &str) -> T {
    serde_json::from_str(&s(m, k)).unwrap_or_default()
}

fn read(id: &str, m: &LoroMap) -> Rule {
    Rule {
        id: id.to_string(),
        name: s(m, "name"),
        enabled: b(m, "enabled"),
        mode: if s(m, "mode") == "live" {
            Mode::Live
        } else {
            Mode::DryRun
        },
        scope: json::<Scope>(m, "scope"),
        trigger: json::<Trigger>(m, "trigger"),
        when: json::<When>(m, "when"),
        actions: json::<Vec<Action>>(m, "actions"),
        created: n(m, "created"),
        by: s(m, "by"),
    }
}

/// Every rule, oldest first.
pub fn rules(doc: &LoroDoc) -> Vec<Rule> {
    let map = doc.get_map("rules");
    let mut out: Vec<Rule> = map
        .keys()
        .filter_map(|k| child(&map, &k).map(|m| read(&k, &m)))
        .collect();
    out.sort_by(|a, b| (a.created, &a.id).cmp(&(b.created, &b.id)));
    out
}

pub fn rule(doc: &LoroDoc, id: &str) -> Option<Rule> {
    child(&doc.get_map("rules"), id).map(|m| read(id, &m))
}

/// A rule by id, or by name (any case) when one has it.
pub fn find(doc: &LoroDoc, name_or_id: &str) -> Option<Rule> {
    rule(doc, name_or_id).or_else(|| {
        rules(doc)
            .into_iter()
            .find(|r| r.name.eq_ignore_ascii_case(name_or_id.trim()))
    })
}

fn put_str(m: &LoroMap, k: &str, v: &str) -> loro::LoroResult<()> {
    if s(m, k) != v {
        m.insert(k, v)?;
    }
    Ok(())
}

/// Make a rule (always off and a dry run: a person turns it on and makes
/// it live), or save an edited one (keeping its on/off and mode). Returns
/// its id.
pub fn save(doc: &LoroDoc, r: &Rule, by: &str, now: i64) -> loro::LoroResult<String> {
    let map = doc.get_map("rules");
    let id = if r.id.is_empty() {
        crate::new_id(doc, "r", now)
    } else {
        r.id.clone()
    };
    let fresh = child(&map, &id).is_none();
    let m = map.ensure_mergeable_map(&id)?;
    put_str(&m, "name", r.name.trim())?;
    put_str(
        &m,
        "scope",
        &serde_json::to_string(&r.scope).expect("serializes"),
    )?;
    put_str(
        &m,
        "trigger",
        &serde_json::to_string(&r.trigger).expect("serializes"),
    )?;
    put_str(
        &m,
        "when",
        &serde_json::to_string(&r.when).expect("serializes"),
    )?;
    put_str(
        &m,
        "actions",
        &serde_json::to_string(&r.actions).expect("serializes"),
    )?;
    if fresh {
        m.insert("enabled", false)?;
        m.insert("mode", "dry-run")?;
        m.insert("created", now)?;
        m.insert("by", by)?;
    }
    Ok(id)
}

pub fn set_enabled(doc: &LoroDoc, id: &str, on: bool) -> loro::LoroResult<()> {
    if let Some(m) = child(&doc.get_map("rules"), id) {
        m.insert("enabled", on)?;
    }
    Ok(())
}

pub fn set_mode(doc: &LoroDoc, id: &str, mode: Mode) -> loro::LoroResult<()> {
    if let Some(m) = child(&doc.get_map("rules"), id) {
        m.insert(
            "mode",
            match mode {
                Mode::Live => "live",
                Mode::DryRun => "dry-run",
            },
        )?;
    }
    Ok(())
}

/// Forget a rule (what it already did stays in the journal).
pub fn remove(doc: &LoroDoc, id: &str) -> loro::LoroResult<()> {
    let map = doc.get_map("rules");
    if map.get(id).is_some() {
        map.delete(id)?;
    }
    let req = doc.get_map("requests");
    if req.get(id).is_some() {
        req.delete(id)?;
    }
    Ok(())
}

/// A "Run now" request.
#[derive(Clone, Debug, PartialEq)]
pub struct Request {
    pub at: i64,
    /// Do it (only honoured for a rule that is live).
    pub live: bool,
    pub by: String,
}

pub fn request_run(
    doc: &LoroDoc,
    id: &str,
    live: bool,
    by: &str,
    now: i64,
) -> loro::LoroResult<()> {
    let m = doc.get_map("requests").ensure_mergeable_map(id)?;
    m.insert("at", now)?;
    m.insert("live", live)?;
    m.insert("by", by)?;
    Ok(())
}

pub fn request(doc: &LoroDoc, id: &str) -> Option<Request> {
    child(&doc.get_map("requests"), id).map(|m| Request {
        at: n(&m, "at"),
        live: b(&m, "live"),
        by: s(&m, "by"),
    })
}

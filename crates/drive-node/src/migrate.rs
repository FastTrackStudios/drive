//! Org-scoped ids: every document id names its org (`cody/studio:library`).
//! Nodes from before kept ids without one; this renames them once, when
//! the node starts with `--org`, after a backup:
//!
//! - `drive:space:S` → `S/drive:space`, `drive:files:S` → `S/drive:files`
//!   (a space is an org);
//! - `meals:…` → `<org>/season:…` (the app's new name), with the spine's
//!   meal events and tasks pointed at Season;
//! - anything else → `<org>/<id>`.

use loro::{LoroDoc, LoroValue, ValueOrContainer};

use crate::store::Store;

/// A space's org: its name, unless the node maps it (`cody` → `codywright`,
/// the org's slug at the issuer).
fn org_for<'a>(space: &'a str, spaces: &'a [(String, String)]) -> &'a str {
    spaces
        .iter()
        .find(|(s, _)| s == space)
        .map_or(space, |(_, o)| o.as_str())
}

fn new_id(org: &str, id: &str, spaces: &[(String, String)]) -> String {
    if let Some(space) = id.strip_prefix("drive:space:") {
        format!("{}/drive:space", org_for(space, spaces))
    } else if let Some(space) = id.strip_prefix("drive:files:") {
        format!("{}/drive:files", org_for(space, spaces))
    } else if let Some(rest) = id.strip_prefix("meals:") {
        format!("{org}/season:{rest}")
    } else {
        format!("{org}/{id}")
    }
}

fn s(v: Option<ValueOrContainer>) -> String {
    match v {
        Some(ValueOrContainer::Value(LoroValue::String(x))) => x.to_string(),
        _ => String::new(),
    }
}

/// An app was renamed: its events and tasks in a spine follow (their
/// app, their links, and event ids that start with the app's name).
pub fn rename_in_spine(doc: &LoroDoc, from: &str, to: &str) {
    let (id_from, link_from) = (format!("{from}:"), format!("/{from}"));
    let events = doc.get_map("events");
    for key in events.keys().map(|k| k.to_string()).collect::<Vec<_>>() {
        let Some(ValueOrContainer::Container(c)) = events.get(&key) else {
            continue;
        };
        let Ok(m) = c.into_map() else { continue };
        if s(m.get("app")) == from {
            let _ = m.insert("app", to);
        }
        let link = s(m.get("link"));
        if let Some(rest) = link.strip_prefix(&link_from) {
            let _ = m.insert("link", format!("/{to}{rest}"));
        }
        if let Some(rest) = key.strip_prefix(&id_from) {
            // Move the event to its new id: copy every field, then drop the old.
            let new = format!("{to}:{rest}");
            if let Ok(n) = events.ensure_mergeable_map(&new) {
                for k in m.keys() {
                    if let Some(ValueOrContainer::Value(v)) = m.get(&k) {
                        let v = if &*k == "id" {
                            LoroValue::from(new.as_str())
                        } else {
                            v
                        };
                        let _ = n.insert(&k, v);
                    }
                }
                let _ = events.delete(&key);
            }
        }
    }
    let inbox = doc.get_movable_list("inbox");
    for i in 0..inbox.len() {
        let Some(ValueOrContainer::Container(c)) = inbox.get(i) else {
            continue;
        };
        let Ok(m) = c.into_map() else { continue };
        if s(m.get("app")) == from {
            let _ = m.insert("app", to);
        }
        let link = s(m.get("link"));
        if let Some(rest) = link.strip_prefix(&link_from) {
            let _ = m.insert("link", format!("/{to}{rest}"));
        }
    }
    doc.commit();
}

/// Rename an app's documents in an org (`<org>/from:…` → `<org>/to:…`)
/// and point the org's spine at the new name. Run with the node stopped.
pub async fn rename_app(store: &Store, org: &str, from: &str, to: &str) -> Result<usize, String> {
    let prefix = format!("{org}/{from}:");
    let ids: Vec<String> = store
        .list()
        .await
        .map_err(|e| e.to_string())?
        .into_iter()
        .map(|(id, _)| id)
        .filter(|id| id.starts_with(&prefix))
        .collect();
    for id in &ids {
        let new = format!("{org}/{to}:{}", &id[prefix.len()..]);
        store.rename(id, &new).await.map_err(|e| e.to_string())?;
        tracing::info!(migrate.from = %id, migrate.to = %new, "an app's document follows its new name");
    }
    respine(store, &drive_spine::spine_doc(org), from, to).await?;
    Ok(ids.len())
}

async fn respine(store: &Store, spine: &str, from: &str, to: &str) -> Result<(), String> {
    if let Some(snap) = store.load(spine).await.map_err(|e| e.to_string())? {
        let doc = LoroDoc::new();
        doc.import(&snap).map_err(|e| e.to_string())?;
        rename_in_spine(&doc, from, to);
        let out = doc
            .export(loro::ExportMode::Snapshot)
            .map_err(|e| e.to_string())?;
        store.save(spine, out).await.map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// Rename every unscoped document; returns how many moved.
pub async fn to_org_ids(
    store: &Store,
    org: &str,
    spaces: &[(String, String)],
) -> Result<usize, String> {
    for (space, to) in spaces {
        store
            .rename_space(space, to)
            .await
            .map_err(|e| e.to_string())?;
    }
    let ids: Vec<String> = store
        .list()
        .await
        .map_err(|e| e.to_string())?
        .into_iter()
        .map(|(id, _)| id)
        .filter(|id| !id.contains('/'))
        .collect();
    for id in &ids {
        let to = new_id(org, id, spaces);
        if to.starts_with('/') {
            // A registry with no space name (made by a since-fixed bug): nothing in it.
            store.delete(id).await.map_err(|e| e.to_string())?;
            continue;
        }
        store.rename(id, &to).await.map_err(|e| e.to_string())?;
        if id == "drive:spine" {
            respine(store, &to, "meals", "season").await?;
        }
        tracing::info!(migrate.from = %id, migrate.to = %to, "a document now names its org");
    }
    Ok(ids.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_name_their_org() {
        let map = [("cody".to_string(), "codywright".to_string())];
        assert_eq!(
            new_id("codywright", "drive:space:cody", &map),
            "codywright/drive:space"
        );
        assert_eq!(
            new_id("codywright", "drive:files:test-3222", &map),
            "test-3222/drive:files"
        );
        assert_eq!(
            new_id("codywright", "meals:home", &map),
            "codywright/season:home"
        );
        assert_eq!(
            new_id("codywright", "studio:song:x", &map),
            "codywright/studio:song:x"
        );
        assert_eq!(
            new_id("codywright", "drive:spine", &map),
            "codywright/drive:spine"
        );
    }

    #[test]
    fn the_spine_follows_a_renamed_app() {
        let d = LoroDoc::new();
        let e = d
            .get_map("events")
            .ensure_mergeable_map("meals:prep:2026-11-22")
            .unwrap();
        e.insert("id", "meals:prep:2026-11-22").unwrap();
        e.insert("app", "meals").unwrap();
        e.insert("link", "/meals/").unwrap();
        e.insert("title", "Prep").unwrap();
        rename_in_spine(&d, "meals", "season");
        let ev = d.get_map("events");
        assert!(ev.get("meals:prep:2026-11-22").is_none());
        let Some(ValueOrContainer::Container(c)) = ev.get("season:prep:2026-11-22") else {
            panic!("moved")
        };
        let m = c.into_map().unwrap();
        assert_eq!(s(m.get("app")), "season");
        assert_eq!(s(m.get("link")), "/season/");
        assert_eq!(s(m.get("title")), "Prep");
        assert_eq!(s(m.get("id")), "season:prep:2026-11-22");
    }
}

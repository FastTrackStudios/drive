//! Leases: what each connected app holds pinned. A lease lasts while the
//! app's connection does (or until it releases it); while any app holds a
//! file, neither the app cache nor the placement pass lets it go.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use drive_local::{Holder, How, Pin};

#[derive(Clone, Debug)]
pub struct Item {
    pub target: String,
    pub path: PathBuf,
    pub hash: Option<String>,
    pub size: u64,
    pub how: How,
    pub why: String,
    pub since: u64,
}

#[derive(Default)]
struct Inner {
    /// Connection → (its app, what it holds).
    conns: HashMap<u64, (String, Vec<Item>)>,
}

#[derive(Clone, Default)]
pub struct Leases(Arc<Mutex<Inner>>);

impl Leases {
    pub fn open(&self, conn: u64, app: &str) {
        self.0
            .lock()
            .expect("leases")
            .conns
            .insert(conn, (app.to_string(), Vec::new()));
    }

    /// Hold `item` for `conn` (once per target and path).
    pub fn hold(&self, conn: u64, item: Item) {
        let mut inner = self.0.lock().expect("leases");
        let Some((_, items)) = inner.conns.get_mut(&conn) else {
            return;
        };
        items.retain(|i| !(i.target == item.target && i.path == item.path));
        items.push(item);
    }

    /// Let go of one target (`None`: everything) of `conn`; how many went.
    pub fn release(&self, conn: u64, target: Option<&str>) -> usize {
        let mut inner = self.0.lock().expect("leases");
        let Some((_, items)) = inner.conns.get_mut(&conn) else {
            return 0;
        };
        let before = items.len();
        match target {
            Some(t) => items.retain(|i| i.target != t),
            None => items.clear(),
        }
        before - items.len()
    }

    /// The connection ended: every lease it held ends.
    pub fn end(&self, conn: u64) -> usize {
        self.0
            .lock()
            .expect("leases")
            .conns
            .remove(&conn)
            .map_or(0, |(_, items)| items.len())
    }

    /// Whether any app holds this content.
    pub fn holds_hash(&self, hash: &str) -> bool {
        self.0
            .lock()
            .expect("leases")
            .conns
            .values()
            .any(|(_, items)| items.iter().any(|i| i.hash.as_deref() == Some(hash)))
    }

    /// The apps connected now.
    pub fn apps(&self) -> Vec<String> {
        let mut v: Vec<String> = self
            .0
            .lock()
            .expect("leases")
            .conns
            .values()
            .map(|(a, _)| a.clone())
            .collect();
        v.sort();
        v.dedup();
        v
    }

    /// Everything held, one entry per file, with who holds it and why.
    pub fn pins(&self) -> Vec<Pin> {
        let inner = self.0.lock().expect("leases");
        let mut by_path: HashMap<PathBuf, Pin> = HashMap::new();
        for (app, items) in inner.conns.values() {
            for i in items {
                let p = by_path.entry(i.path.clone()).or_insert_with(|| Pin {
                    path: i.path.clone(),
                    hash: i.hash.clone(),
                    size: i.size,
                    how: i.how,
                    holders: Vec::new(),
                });
                if p.hash.is_none() {
                    p.hash = i.hash.clone();
                }
                p.holders.push(Holder {
                    app: app.clone(),
                    target: i.target.clone(),
                    why: i.why.clone(),
                    since: i.since,
                });
            }
        }
        let mut v: Vec<Pin> = by_path.into_values().collect();
        v.sort_by(|a, b| a.path.cmp(&b.path));
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(target: &str, hash: &str) -> Item {
        Item {
            target: target.into(),
            path: PathBuf::from(format!("/cache/{hash}")),
            hash: Some(hash.into()),
            size: 1,
            how: How::Cached,
            why: "open".into(),
            since: 0,
        }
    }

    #[test]
    fn a_lease_ends_with_its_connection_or_its_release() {
        let l = Leases::default();
        l.open(1, "studio");
        l.open(2, "player");
        l.hold(1, item("a", "h1"));
        l.hold(1, item("b", "h2"));
        l.hold(2, item("a2", "h1"));
        assert!(l.holds_hash("h1") && l.holds_hash("h2"));
        assert_eq!(
            l.pins()
                .iter()
                .find(|p| p.hash.as_deref() == Some("h1"))
                .unwrap()
                .holders
                .len(),
            2
        );
        assert_eq!(l.release(1, Some("b")), 1);
        assert!(!l.holds_hash("h2"));
        assert_eq!(l.end(1), 1);
        assert!(l.holds_hash("h1"));
        assert_eq!(l.end(2), 1);
        assert!(!l.holds_hash("h1") && l.pins().is_empty());
    }
}

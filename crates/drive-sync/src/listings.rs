//! One connection's live listings ([`drive_proto::ToNode::ListSubscribe`]):
//! the prefixes it listens to, and the [`Change`]s it has yet to hear.
//! Whoever answers the connection (`serve` here, a node's session) decides
//! which changes it may see; this only says which prefixes a change is
//! under.

use std::collections::BTreeSet;

use drive_proto::ToClient;
use tokio::sync::broadcast;

use crate::Rooms;
use crate::rooms::Change;

/// What a connection's listings heard.
#[derive(Debug, PartialEq)]
pub enum Heard {
    /// A change to what is listed.
    Change(Change),
    /// It fell behind and missed changes: list every prefix again.
    Missed,
}

/// The prefixes a connection listens to.
#[derive(Default)]
pub struct Listings {
    prefixes: BTreeSet<String>,
    /// Open while there is a prefix: a connection that lists nothing hears
    /// nothing.
    changes: Option<broadcast::Receiver<Change>>,
}

impl Listings {
    /// Listen to `prefix` (again is once). Listen before listing it now:
    /// a change made between the two is then heard, never lost.
    pub fn subscribe(&mut self, rooms: &Rooms, prefix: String) {
        if self.changes.is_none() {
            self.changes = Some(rooms.changes());
        }
        self.prefixes.insert(prefix);
    }

    /// Stop listening to `prefix`.
    pub fn unsubscribe(&mut self, prefix: &str) {
        self.prefixes.remove(prefix);
        if self.prefixes.is_empty() {
            self.changes = None;
        }
    }

    /// The prefixes listened to.
    pub fn prefixes(&self) -> impl Iterator<Item = &str> {
        self.prefixes.iter().map(String::as_str)
    }

    /// The next thing heard; never, while nothing is listened to. Safe to
    /// drop unfinished (in a `select!`): nothing is lost.
    pub async fn next(&mut self) -> Heard {
        let Some(rx) = self.changes.as_mut() else {
            return std::future::pending().await;
        };
        match rx.recv().await {
            Ok(c) => Heard::Change(c),
            Err(broadcast::error::RecvError::Lagged(_)) => Heard::Missed,
            // The rooms are gone: nothing more will change.
            Err(broadcast::error::RecvError::Closed) => {
                self.changes = None;
                std::future::pending().await
            }
        }
    }

    /// What to send for `change`: one [`ToClient::Listed`] per prefix it is
    /// under (none: it is not listened to).
    #[must_use]
    pub fn deltas(&self, change: &Change) -> Vec<ToClient> {
        self.prefixes
            .iter()
            .filter(|p| drive_proto::under(p, change.id()))
            .map(|prefix| {
                let (docs, removed) = match change {
                    Change::Entry(d) => (vec![d.clone()], Vec::new()),
                    Change::Gone(id) => (Vec::new(), vec![id.clone()]),
                };
                ToClient::Listed {
                    prefix: prefix.clone(),
                    docs,
                    removed,
                }
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use drive_proto::DocInfo;

    #[test]
    fn a_change_is_sent_once_for_each_prefix_it_is_under() {
        let rooms = Rooms::new(std::sync::Arc::new(crate::MemStore::default()));
        let mut l = Listings::default();
        l.subscribe(&rooms, "home/studio:".into());
        l.subscribe(&rooms, "*/studio:".into());
        l.subscribe(&rooms, "home/canvas:".into());
        let entry = Change::Entry(DocInfo {
            id: "home/studio:song:a".into(),
            peers: 1,
            bytes: 0,
        });
        let prefixes: Vec<String> = l
            .deltas(&entry)
            .into_iter()
            .map(|m| match m {
                ToClient::Listed { prefix, .. } => prefix,
                other => panic!("{other:?}"),
            })
            .collect();
        assert_eq!(prefixes, ["*/studio:", "home/studio:"]);
        assert!(l.deltas(&Change::Gone("work/canvas:x".into())).is_empty());
        l.unsubscribe("home/studio:");
        l.unsubscribe("*/studio:");
        assert!(l.deltas(&entry).is_empty());
        l.unsubscribe("home/canvas:");
        assert!(l.changes.is_none(), "nothing listened to, nothing heard");
    }
}

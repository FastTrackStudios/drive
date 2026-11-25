//! What one reconcile of a working copy must do: decided from three views
//! of the folder (its files on disk, in the Drive tree, and both as they
//! were at the last sync), without touching either.
//!
//! Paths are `/`-separated and relative to the working copy's root. Only
//! files are compared; folders follow their files (made where a file needs
//! one, and an emptied folder goes with its last file).

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

/// A file on disk, as a walk sees it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OnDisk {
    pub size: u64,
    pub mtime: i64,
}

/// A file of the Drive tree under the working copy's root.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InTree {
    pub id: String,
    pub hash: String,
    pub size: u64,
}

/// A file as it was when both sides last agreed.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Synced {
    pub id: String,
    pub hash: String,
    pub size: u64,
    /// The file's modified time on disk then (ms).
    pub mtime: i64,
    /// For a page: its version then (`drive_spine::pages::version`), so a
    /// file saved on disk since merges with the page's own edits.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub page_at: Vec<u8>,
}

/// One thing to do.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    /// New or changed on disk: hash it, keep its bytes, put it in the tree
    /// at `rel` (`id`: the tree file to update; `None`: make one).
    Up { rel: String, id: Option<String> },
    /// New or changed in the tree: write its bytes to disk at `rel`.
    Down {
        rel: String,
        id: String,
        hash: String,
    },
    /// Moved or renamed in the tree, unchanged on disk: the same on disk.
    Move {
        from: String,
        to: String,
        id: String,
    },
    /// Gone from disk, unchanged in the tree: the tree file to the Trash.
    TrashInTree { rel: String, id: String },
    /// Gone from the tree, unchanged on disk: the file to `.drive-trash`.
    TrashOnDisk { rel: String },
    /// Changed on both sides: unless the bytes turn out the same, the disk
    /// version is kept beside it under a conflict name (and goes up), and
    /// the tree's comes down at `rel`.
    Conflict {
        rel: String,
        id: String,
        hash: String,
    },
    /// Gone from both: forget it.
    Forget { rel: String },
}

/// The actions for one reconcile, in an order that is safe to run: moves
/// first (so a moved file is not also fetched again), then the rest by
/// path.
pub fn plan(
    disk: &BTreeMap<String, OnDisk>,
    tree: &BTreeMap<String, InTree>,
    last: &BTreeMap<String, Synced>,
) -> Vec<Action> {
    let mut moves = Vec::new();
    let mut rest = Vec::new();
    // A tree file now at another path than when last synced, still the
    // same on disk at its old path and nothing in its new place on disk:
    // moved in the Drive.
    let by_id: BTreeMap<&str, &str> = tree
        .iter()
        .map(|(rel, t)| (t.id.as_str(), rel.as_str()))
        .collect();
    let mut moved_from = BTreeSet::new();
    let mut moved_to = BTreeSet::new();
    for (old, s) in last {
        let Some(&new) = by_id.get(s.id.as_str()) else {
            continue;
        };
        if new == old || disk.contains_key(new) || tree.contains_key(old.as_str()) {
            continue;
        }
        if let Some(d) = disk.get(old)
            && unchanged_on_disk(d, s)
        {
            moves.push(Action::Move {
                from: old.clone(),
                to: new.to_string(),
                id: s.id.clone(),
            });
            moved_from.insert(old.clone());
            moved_to.insert(new.to_string());
        }
    }

    let paths: BTreeSet<&String> = disk.keys().chain(tree.keys()).chain(last.keys()).collect();
    for rel in paths {
        if moved_from.contains(rel) || moved_to.contains(rel) {
            continue;
        }
        let (d, t, s) = (disk.get(rel), tree.get(rel), last.get(rel));
        let rel = rel.clone();
        match (d, t, s) {
            // Both there, as last synced (or never synced but the same size:
            // the hash decides in Up, which leaves an equal file alone).
            (Some(d), Some(t), Some(s)) => {
                let disk_changed = !unchanged_on_disk(d, s);
                let tree_changed = t.hash != s.hash || t.id != s.id;
                match (disk_changed, tree_changed) {
                    (false, false) => {}
                    (true, false) => rest.push(Action::Up {
                        rel,
                        id: Some(t.id.clone()),
                    }),
                    (false, true) => rest.push(Action::Down {
                        rel,
                        id: t.id.clone(),
                        hash: t.hash.clone(),
                    }),
                    (true, true) => rest.push(Action::Conflict {
                        rel,
                        id: t.id.clone(),
                        hash: t.hash.clone(),
                    }),
                }
            }
            // On both sides, never synced (the first reconcile, or made in
            // both places at once): the hash decides.
            (Some(_), Some(t), None) => rest.push(Action::Conflict {
                rel,
                id: t.id.clone(),
                hash: t.hash.clone(),
            }),
            // Only on disk.
            (Some(_), None, None) => rest.push(Action::Up { rel, id: None }),
            (Some(d), None, Some(s)) => {
                if unchanged_on_disk(d, s) {
                    rest.push(Action::TrashOnDisk { rel });
                } else {
                    // Removed in the Drive but changed here: the change wins.
                    rest.push(Action::Up { rel, id: None });
                }
            }
            // Only in the tree.
            (None, Some(t), None) => rest.push(Action::Down {
                rel,
                id: t.id.clone(),
                hash: t.hash.clone(),
            }),
            (None, Some(t), Some(s)) => {
                if t.hash == s.hash && t.id == s.id {
                    rest.push(Action::TrashInTree {
                        rel,
                        id: t.id.clone(),
                    });
                } else {
                    // Deleted here but changed in the Drive: the change wins.
                    rest.push(Action::Down {
                        rel,
                        id: t.id.clone(),
                        hash: t.hash.clone(),
                    });
                }
            }
            (None, None, Some(_)) => rest.push(Action::Forget { rel }),
            (None, None, None) => {}
        }
    }
    moves.extend(rest);
    moves
}

/// Same size and modified time as when last synced.
fn unchanged_on_disk(d: &OnDisk, s: &Synced) -> bool {
    d.size == s.size && d.mtime == s.mtime
}

/// `name.ext` → `name (conflict DEVICE YYYY-MM-DD).ext`.
pub fn conflict_name(rel: &str, device: &str, date: &str) -> String {
    let (dir, name) = match rel.rsplit_once('/') {
        Some((d, n)) => (Some(d), n),
        None => (None, rel),
    };
    let tag = format!(" (conflict {device} {date})");
    let named = match name.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() => format!("{stem}{tag}.{ext}"),
        _ => format!("{name}{tag}"),
    };
    match dir {
        Some(d) => format!("{d}/{named}"),
        None => named,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(size: u64, mtime: i64) -> OnDisk {
        OnDisk { size, mtime }
    }
    fn t(id: &str, hash: &str, size: u64) -> InTree {
        InTree {
            id: id.into(),
            hash: hash.into(),
            size,
        }
    }
    fn s(id: &str, hash: &str, size: u64, mtime: i64) -> Synced {
        Synced {
            id: id.into(),
            hash: hash.into(),
            size,
            mtime,
            page_at: Vec::new(),
        }
    }
    fn m<V: Clone>(items: &[(&str, V)]) -> BTreeMap<String, V> {
        items
            .iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect()
    }

    #[test]
    fn the_first_reconcile_puts_every_file_on_disk_in_the_tree() {
        let disk = m(&[("a.rpp", d(3, 1)), ("Media/kick.wav", d(9, 1))]);
        let acts = plan(&disk, &BTreeMap::new(), &BTreeMap::new());
        assert_eq!(
            acts,
            vec![
                Action::Up {
                    rel: "Media/kick.wav".into(),
                    id: None
                },
                Action::Up {
                    rel: "a.rpp".into(),
                    id: None
                },
            ]
        );
    }

    #[test]
    fn nothing_changed_is_nothing_to_do() {
        let disk = m(&[("a", d(3, 1))]);
        let tree = m(&[("a", t("1@1", "h", 3))]);
        let last = m(&[("a", s("1@1", "h", 3, 1))]);
        assert!(plan(&disk, &tree, &last).is_empty());
    }

    #[test]
    fn a_change_on_one_side_goes_to_the_other() {
        let last = m(&[("a", s("1@1", "h", 3, 1)), ("b", s("2@1", "g", 3, 1))]);
        let disk = m(&[("a", d(4, 2)), ("b", d(3, 1))]);
        let tree = m(&[("a", t("1@1", "h", 3)), ("b", t("2@1", "g2", 5))]);
        assert_eq!(
            plan(&disk, &tree, &last),
            vec![
                Action::Up {
                    rel: "a".into(),
                    id: Some("1@1".into())
                },
                Action::Down {
                    rel: "b".into(),
                    id: "2@1".into(),
                    hash: "g2".into()
                },
            ]
        );
    }

    #[test]
    fn a_change_on_both_sides_is_a_conflict() {
        let last = m(&[("a", s("1@1", "h", 3, 1))]);
        let disk = m(&[("a", d(4, 2))]);
        let tree = m(&[("a", t("1@1", "h2", 5))]);
        assert_eq!(
            plan(&disk, &tree, &last),
            vec![Action::Conflict {
                rel: "a".into(),
                id: "1@1".into(),
                hash: "h2".into()
            }]
        );
    }

    #[test]
    fn a_delete_goes_to_the_other_sides_trash_unless_the_other_side_changed_it() {
        let last = m(&[
            ("gone-here", s("1@1", "h", 3, 1)),
            ("gone-there", s("2@1", "g", 3, 1)),
            ("gone-here-changed-there", s("3@1", "x", 3, 1)),
            ("gone-there-changed-here", s("4@1", "y", 3, 1)),
            ("gone-both", s("5@1", "z", 3, 1)),
        ]);
        let disk = m(&[
            ("gone-there", d(3, 1)),
            ("gone-there-changed-here", d(8, 9)),
        ]);
        let tree = m(&[
            ("gone-here", t("1@1", "h", 3)),
            ("gone-here-changed-there", t("3@1", "x2", 4)),
        ]);
        assert_eq!(
            plan(&disk, &tree, &last),
            vec![
                Action::Forget {
                    rel: "gone-both".into()
                },
                Action::TrashInTree {
                    rel: "gone-here".into(),
                    id: "1@1".into()
                },
                Action::Down {
                    rel: "gone-here-changed-there".into(),
                    id: "3@1".into(),
                    hash: "x2".into()
                },
                Action::TrashOnDisk {
                    rel: "gone-there".into()
                },
                Action::Up {
                    rel: "gone-there-changed-here".into(),
                    id: None
                },
            ]
        );
    }

    #[test]
    fn a_file_moved_in_the_drive_moves_on_disk_instead_of_coming_down_again() {
        let last = m(&[("Old/a.wav", s("1@1", "h", 3, 1))]);
        let disk = m(&[("Old/a.wav", d(3, 1))]);
        let tree = m(&[("New/b.wav", t("1@1", "h", 3))]);
        assert_eq!(
            plan(&disk, &tree, &last),
            vec![Action::Move {
                from: "Old/a.wav".into(),
                to: "New/b.wav".into(),
                id: "1@1".into()
            }]
        );
    }

    #[test]
    fn a_file_on_both_sides_never_synced_lets_the_hash_decide() {
        let disk = m(&[("a", d(3, 1))]);
        let tree = m(&[("a", t("1@1", "h", 3))]);
        assert_eq!(
            plan(&disk, &tree, &BTreeMap::new()),
            vec![Action::Conflict {
                rel: "a".into(),
                id: "1@1".into(),
                hash: "h".into()
            }]
        );
    }

    #[test]
    fn conflict_names_keep_the_extension_and_the_folder() {
        assert_eq!(
            conflict_name("Mix/song.rpp", "Laptop", "2026-11-24"),
            "Mix/song (conflict Laptop 2026-11-24).rpp"
        );
        assert_eq!(conflict_name("README", "L", "d"), "README (conflict L d)");
        assert_eq!(conflict_name(".hidden", "L", "d"), ".hidden (conflict L d)");
    }
}

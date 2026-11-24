//! The Drive tree for a system file provider (macOS File Provider,
//! docs/fileprovider.md): items, a folder a page at a time, what changed
//! since a version, a file's bytes, and the writes, all answered from the
//! tree document. The provider holds no state and touches no files of its
//! own; this is the same tree the mount shows, and writes do what the
//! mount's do.
//!
//! - **Ids** are tree ids ([`tree::id_string`]); the top is [`ROOT`].
//! - **The Trash** is not shown: what is in it reads as deleted, and
//!   deleting moves there (nothing is ever deleted).
//! - **An anchor** is the tree document's version (its frontiers, hex).
//!   What changed since one is worked out by comparing the tree at that
//!   version ([`LoroDoc::fork_at`]) with the tree now.

use std::collections::HashMap;
use std::path::Path;

use drive_files::tree::{self, Entry};
use drive_local::{FpChanges, FpFetched, FpItem, FpPage};
use loro::{Frontiers, LoroDoc, TreeID};

use crate::agent::Agent;

/// The id of the top of the tree.
pub const ROOT: &str = "root";

/// Items a page holds when the asker does not say.
const PAGE: usize = 500;

fn parse(id: &str) -> Result<Option<TreeID>, String> {
    if id == ROOT || id.is_empty() {
        return Ok(None);
    }
    tree::parse_id(id)
        .map(Some)
        .ok_or_else(|| format!("no item {id}"))
}

fn id_of(id: Option<TreeID>) -> String {
    id.map_or_else(|| ROOT.to_string(), tree::id_string)
}

/// What an item is, apart from whether its bytes are here: two items
/// that differ in this differ for the provider.
#[derive(Clone, PartialEq)]
struct Seen {
    parent: Option<TreeID>,
    entry: Entry,
}

/// Every item of a tree that is not in the Trash, by id.
fn visible(doc: &LoroDoc) -> HashMap<TreeID, Seen> {
    let mut out = HashMap::new();
    let mut stack: Vec<Option<TreeID>> = vec![None];
    while let Some(p) = stack.pop() {
        for e in tree::children(doc, p) {
            if p.is_none() && e.dir && e.name == tree::TRASH {
                continue;
            }
            if e.dir {
                stack.push(Some(e.id));
            }
            out.insert(e.id, Seen { parent: p, entry: e });
        }
    }
    out
}

fn anchor_of(doc: &LoroDoc) -> String {
    hex(&doc.oplog_frontiers().encode())
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn unhex(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok())
        .collect()
}

impl Agent {
    fn fp_item_of(&self, parent: Option<TreeID>, e: &Entry) -> FpItem {
        FpItem {
            id: tree::id_string(e.id),
            parent: id_of(parent),
            name: e.name.clone(),
            dir: e.dir,
            size: if e.dir { 0 } else { e.size },
            mtime: e.mtime,
            content: if e.dir {
                String::new()
            } else {
                format!("{}:{}", e.hash, e.size)
            },
            meta: format!("{}/{}", id_of(parent), e.name),
            here: e.dir || e.hash.is_empty() || self.is_cached(&e.hash),
        }
    }

    /// A shown item: not the top, not the Trash or in it.
    fn fp_entry(&self, id: TreeID) -> Result<(Option<TreeID>, Entry), String> {
        let e = tree::entry(&self.files, id).ok_or_else(|| format!("no item {}", tree::id_string(id)))?;
        if tree::in_trash(&self.files, id) {
            return Err(format!("no item {}", tree::id_string(id)));
        }
        Ok((tree::parent_of(&self.files, id), e))
    }

    pub fn fp_item(&self, id: &str) -> Result<FpItem, String> {
        match parse(id)? {
            None => Ok(FpItem {
                id: ROOT.into(),
                parent: ROOT.into(),
                name: String::new(),
                dir: true,
                size: 0,
                mtime: 0,
                content: String::new(),
                meta: String::new(),
                here: true,
            }),
            Some(t) => {
                let (p, e) = self.fp_entry(t)?;
                Ok(self.fp_item_of(p, &e))
            }
        }
    }

    pub fn fp_list(&self, parent: &str, after: Option<&str>, limit: Option<usize>) -> Result<FpPage, String> {
        let p = parse(parent)?;
        if let Some(t) = p {
            let (_, e) = self.fp_entry(t)?;
            if !e.dir {
                return Err(format!("{} is not a folder", e.name));
            }
        }
        let all: Vec<Entry> = tree::children(&self.files, p)
            .into_iter()
            .filter(|e| !(p.is_none() && e.dir && e.name == tree::TRASH))
            .collect();
        let start = after.and_then(|a| a.parse::<usize>().ok()).unwrap_or(0);
        let limit = limit.unwrap_or(PAGE).clamp(1, 5000);
        let items: Vec<FpItem> = all
            .iter()
            .skip(start)
            .take(limit)
            .map(|e| self.fp_item_of(p, e))
            .collect();
        let next = (start + items.len() < all.len()).then(|| (start + items.len()).to_string());
        Ok(FpPage { items, next })
    }

    pub fn fp_anchor(&self) -> String {
        anchor_of(&self.files)
    }

    /// What changed since `anchor`: every item whose name, place, bytes or
    /// kind differ (or that is new), and every id no longer shown.
    pub fn fp_changes(&self, anchor: &str) -> FpChanges {
        let now = anchor_of(&self.files);
        let expired = || FpChanges {
            anchor: now.clone(),
            expired: true,
            ..FpChanges::default()
        };
        if anchor == now {
            return FpChanges {
                anchor: now,
                ..FpChanges::default()
            };
        }
        // A version this document holds every change of: else (another
        // device's tree, a reset) it cannot be gone back to.
        let vv = self.files.oplog_vv();
        let Some(then) = unhex(anchor)
            .and_then(|b| Frontiers::decode(&b).ok())
            .filter(|f| f.iter().all(|id| vv.includes_id(id)))
            .and_then(|f| self.files.fork_at(&f).ok())
        else {
            return expired();
        };
        let (before, after) = (visible(&then), visible(&self.files));
        let updated = after
            .iter()
            .filter(|(id, s)| before.get(id) != Some(s))
            .map(|(_, s)| self.fp_item_of(s.parent, &s.entry))
            .collect();
        let deleted = before
            .keys()
            .filter(|id| !after.contains_key(id))
            .map(|id| tree::id_string(*id))
            .collect();
        FpChanges {
            updated,
            deleted,
            anchor: now,
            expired: false,
        }
    }

    /// A file's bytes, fetched here if they are not, then copied into
    /// `into` (the asker's own folder: it may not read the agent's cache).
    pub async fn fp_fetch(&self, id: &str, into: &Path) -> Result<FpFetched, String> {
        let t = parse(id)?.ok_or("the top is a folder")?;
        let (p, e) = self.fp_entry(t)?;
        if e.dir {
            return Err(format!("{} is a folder", e.name));
        }
        std::fs::create_dir_all(into).map_err(|x| format!("{}: {x}", into.display()))?;
        let safe: String = e
            .name
            .chars()
            .map(|c| if c == '/' || c == '\0' { '_' } else { c })
            .collect();
        let dest = into.join(format!("{}-{safe}", tree::id_string(t)));
        if e.hash.is_empty() {
            std::fs::write(&dest, b"").map_err(|x| x.to_string())?;
        } else {
            let src = self.fetch(&e.hash).await?;
            let tmp = dest.with_extension("drive-part");
            std::fs::copy(&src, &tmp)
                .and_then(|_| std::fs::rename(&tmp, &dest))
                .map_err(|x| format!("copying {}: {x}", e.name))?;
        }
        Ok(FpFetched {
            path: dest,
            item: self.fp_item_of(p, &e),
        })
    }

    fn fp_folder(&self, parent: &str) -> Result<Option<TreeID>, String> {
        let p = parse(parent)?;
        if let Some(t) = p {
            let (_, e) = self.fp_entry(t)?;
            if !e.dir {
                return Err(format!("{} is not a folder", e.name));
            }
        }
        Ok(p)
    }

    fn fp_name(name: &str) -> Result<&str, String> {
        if name.is_empty() || name == "." || name == ".." || name.contains('/') || name.contains('\0') {
            return Err(format!("{name:?} is not a name"));
        }
        Ok(name)
    }

    pub async fn fp_create(
        &self,
        parent: &str,
        name: &str,
        dir: bool,
        from: Option<&Path>,
    ) -> Result<FpItem, String> {
        let p = self.fp_folder(parent)?;
        let name = Self::fp_name(name)?;
        if p.is_none() && name == tree::TRASH {
            return Err(format!("{name} is the Drive's Trash"));
        }
        if tree::lookup(&self.files, p, name).is_some() {
            return Err(format!("exists: {name}"));
        }
        let id = if dir {
            tree::mkdir(&self.files, p, name)
        } else {
            tree::create_file(&self.files, p, name)
        }
        .map_err(|e| e.to_string())?;
        if let (false, Some(src)) = (dir, from) {
            let (hash, size) = self.upload(id, src).await?;
            tree::set_content(&self.files, id, size, &hash).map_err(|e| e.to_string())?;
        }
        self.commit_files();
        let (p, e) = self.fp_entry(id)?;
        Ok(self.fp_item_of(p, &e))
    }

    pub async fn fp_modify(
        &self,
        id: &str,
        name: Option<&str>,
        parent: Option<&str>,
        from: Option<&Path>,
    ) -> Result<FpItem, String> {
        let t = parse(id)?.ok_or("the top cannot be changed")?;
        let (at, e) = self.fp_entry(t)?;
        if let Some(src) = from {
            if e.dir {
                return Err(format!("{} is a folder", e.name));
            }
            let (hash, size) = self.upload(t, src).await?;
            tree::set_content(&self.files, t, size, &hash).map_err(|x| x.to_string())?;
        }
        if name.is_some() || parent.is_some() {
            let to = match parent {
                Some(p) => self.fp_folder(p)?,
                None => at,
            };
            let new = Self::fp_name(name.unwrap_or(&e.name))?;
            // Into itself or under itself: refused, as a move must be.
            let mut up = to;
            while let Some(u) = up {
                if u == t {
                    return Err(format!("{} cannot go inside itself", e.name));
                }
                up = tree::parent_of(&self.files, u);
            }
            if tree::lookup(&self.files, to, new).is_some_and(|o| o.id != t) {
                return Err(format!("exists: {new}"));
            }
            tree::rename(&self.files, t, to, new).map_err(|x| x.to_string())?;
        }
        self.commit_files();
        let (p, e) = self.fp_entry(t)?;
        Ok(self.fp_item_of(p, &e))
    }

    pub fn fp_delete(&self, id: &str) -> Result<(), String> {
        let t = parse(id)?.ok_or("the top cannot be deleted")?;
        self.fp_entry(t)?;
        if !tree::trash(&self.files, t).map_err(|e| e.to_string())? {
            return Err("already in the Trash".into());
        }
        self.commit_files();
        Ok(())
    }
}

#[cfg(test)]
mod tests;

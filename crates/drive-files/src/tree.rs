//! A space's file tree: a Loro movable tree of folders and files. Each
//! node's metadata map holds `name`, `kind` (`dir` or `file`), and for
//! files `size`, `mtime` (ms), `hash` (BLAKE3 hex, empty until uploaded)
//! and `mime`. Moves and renames from several devices merge; two files
//! given the same name at once both survive, and listings tell them apart.

use std::collections::HashMap;

use loro::{LoroDoc, LoroMap, LoroTree, LoroValue, TreeID, TreeParentId, ValueOrContainer};

/// A folder or a file.
#[derive(Clone, Debug, PartialEq)]
pub struct Entry {
    pub id: TreeID,
    pub name: String,
    pub dir: bool,
    pub size: u64,
    pub mtime: i64,
    pub hash: String,
    pub mime: String,
}

fn tree(doc: &LoroDoc) -> LoroTree {
    let t = doc.get_tree("files");
    if !t.is_fractional_index_enabled() {
        t.enable_fractional_index(0);
    }
    t
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

fn parent_id(parent: Option<TreeID>) -> TreeParentId {
    match parent {
        Some(p) => TreeParentId::Node(p),
        None => TreeParentId::Root,
    }
}

/// One entry, read.
pub fn entry(doc: &LoroDoc, id: TreeID) -> Option<Entry> {
    let t = tree(doc);
    if !t.contains(id) || t.is_node_deleted(&id).unwrap_or(true) {
        return None;
    }
    let m = t.get_meta(id).ok()?;
    Some(Entry {
        id,
        name: s(&m, "name"),
        dir: s(&m, "kind") == "dir",
        size: n(&m, "size").max(0) as u64,
        mtime: n(&m, "mtime"),
        hash: s(&m, "hash"),
        mime: s(&m, "mime"),
    })
}

/// A folder's entries (`None`: the top), folders first, by name. Two with
/// the same name (made at once on two devices) are told apart: the later
/// becomes `name (2)` in the listing.
pub fn children(doc: &LoroDoc, parent: Option<TreeID>) -> Vec<Entry> {
    let t = tree(doc);
    let ids = t.children(parent_id(parent)).unwrap_or_default();
    let mut out: Vec<Entry> = ids.into_iter().filter_map(|id| entry(doc, id)).collect();
    out.sort_by(|a, b| {
        (!a.dir, a.name.to_lowercase(), a.id.to_string()).cmp(&(
            !b.dir,
            b.name.to_lowercase(),
            b.id.to_string(),
        ))
    });
    let mut seen: HashMap<String, u32> = HashMap::new();
    for e in &mut out {
        let c = seen.entry(e.name.clone()).or_insert(0);
        *c += 1;
        if *c > 1 {
            e.name = numbered(&e.name, *c);
        }
    }
    out
}

fn numbered(name: &str, k: u32) -> String {
    match name.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() => format!("{stem} ({k}).{ext}"),
        _ => format!("{name} ({k})"),
    }
}

/// The entry with this (listed) name in a folder.
pub fn lookup(doc: &LoroDoc, parent: Option<TreeID>, name: &str) -> Option<Entry> {
    children(doc, parent).into_iter().find(|e| e.name == name)
}

/// The entry at a path (`a/b/c.wav`); `None` for the top.
pub fn resolve(doc: &LoroDoc, path: &str) -> Option<Option<Entry>> {
    let mut cur: Option<Entry> = None;
    for part in path.split('/').filter(|p| !p.is_empty()) {
        let next = lookup(doc, cur.as_ref().map(|e| e.id), part)?;
        cur = Some(next);
    }
    Some(cur)
}

/// An entry's path from the top.
pub fn path_of(doc: &LoroDoc, id: TreeID) -> String {
    let t = tree(doc);
    let mut parts = Vec::new();
    let mut cur = Some(id);
    while let Some(c) = cur {
        if let Some(e) = entry(doc, c) {
            parts.push(e.name);
        }
        cur = match t.parent(c) {
            Some(TreeParentId::Node(p)) => Some(p),
            _ => None,
        };
    }
    parts.reverse();
    parts.join("/")
}

pub fn parent_of(doc: &LoroDoc, id: TreeID) -> Option<TreeID> {
    match tree(doc).parent(id) {
        Some(TreeParentId::Node(p)) => Some(p),
        _ => None,
    }
}

/// A new folder.
pub fn mkdir(doc: &LoroDoc, parent: Option<TreeID>, name: &str) -> loro::LoroResult<TreeID> {
    let t = tree(doc);
    let id = t.create(parent_id(parent))?;
    let m = t.get_meta(id)?;
    m.insert("name", name)?;
    m.insert("kind", "dir")?;
    m.insert("mtime", crate::now_ms())?;
    Ok(id)
}

/// A new file (its bytes not yet uploaded: `hash` empty until they are).
pub fn create_file(doc: &LoroDoc, parent: Option<TreeID>, name: &str) -> loro::LoroResult<TreeID> {
    let t = tree(doc);
    let id = t.create(parent_id(parent))?;
    let m = t.get_meta(id)?;
    m.insert("name", name)?;
    m.insert("kind", "file")?;
    m.insert("size", 0i64)?;
    m.insert("mtime", crate::now_ms())?;
    m.insert("hash", "")?;
    m.insert("mime", mime_of(name))?;
    Ok(id)
}

/// A file's new content: its size and hash (the commit point of an upload).
pub fn set_content(doc: &LoroDoc, id: TreeID, size: u64, hash: &str) -> loro::LoroResult<()> {
    let m = tree(doc).get_meta(id)?;
    m.insert("size", size as i64)?;
    m.insert("hash", hash)?;
    m.insert("mtime", crate::now_ms())?;
    Ok(())
}

/// Move and/or rename.
pub fn rename(
    doc: &LoroDoc,
    id: TreeID,
    parent: Option<TreeID>,
    name: &str,
) -> loro::LoroResult<()> {
    let t = tree(doc);
    if t.parent(id) != Some(parent_id(parent)) {
        t.mov(id, parent_id(parent))?;
    }
    let m = t.get_meta(id)?;
    m.insert("name", name)?;
    m.insert("mime", mime_of(name))?;
    Ok(())
}

/// The folder at the top where removed things go.
pub const TRASH: &str = "Trash";

/// Remove an entry: move it (and everything under it) into the Trash
/// folder at the top, renamed if the Trash already holds that name.
/// Nothing in the tree is ever deleted, so every file's bytes stay
/// reachable. `false`: refused, because it is the Trash itself or already
/// in it (emptying the Trash is not something Drive does).
pub fn trash(doc: &LoroDoc, id: TreeID) -> loro::LoroResult<bool> {
    let bin = match lookup(doc, None, TRASH) {
        Some(e) if e.dir => e.id,
        _ => mkdir(doc, None, TRASH)?,
    };
    if id == bin || in_trash(doc, id) {
        return Ok(false);
    }
    let name = entry(doc, id).map(|e| e.name).unwrap_or_default();
    let mut put = name.clone();
    let mut n = 2;
    while lookup(doc, Some(bin), &put).is_some_and(|e| e.id != id) {
        put = format!("{name} ({n})");
        n += 1;
    }
    rename(doc, id, Some(bin), &put)?;
    tree(doc).get_meta(id)?.insert("trashed", crate::now_ms())?;
    Ok(true)
}

/// Whether an entry is the Trash or inside it.
pub fn in_trash(doc: &LoroDoc, id: TreeID) -> bool {
    let mut at = Some(id);
    while let Some(cur) = at {
        if parent_of(doc, cur).is_none()
            && entry(doc, cur).is_some_and(|e| e.dir && e.name == TRASH)
        {
            return true;
        }
        at = parent_of(doc, cur);
    }
    false
}

/// Every file, with its path.
pub fn files(doc: &LoroDoc) -> Vec<(String, Entry)> {
    let mut out = Vec::new();
    let mut stack: Vec<(Option<TreeID>, String)> = vec![(None, String::new())];
    while let Some((p, prefix)) = stack.pop() {
        for e in children(doc, p) {
            let path = if prefix.is_empty() {
                e.name.clone()
            } else {
                format!("{prefix}/{}", e.name)
            };
            if e.dir {
                stack.push((Some(e.id), path));
            } else {
                out.push((path, e));
            }
        }
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

/// A tree id as text, and back (for links and the file browser).
pub fn id_string(id: TreeID) -> String {
    id.to_string()
}

pub fn parse_id(s: &str) -> Option<TreeID> {
    TreeID::try_from(s).ok()
}

/// A MIME type from a file name.
pub fn mime_of(name: &str) -> &'static str {
    match name
        .rsplit_once('.')
        .map(|(_, e)| e.to_ascii_lowercase())
        .as_deref()
    {
        Some("wav") => "audio/wav",
        Some("mp3") => "audio/mpeg",
        Some("flac") => "audio/flac",
        Some("aif" | "aiff") => "audio/aiff",
        Some("m4a") => "audio/mp4",
        Some("ogg" | "oga") => "audio/ogg",
        Some("opus") => "audio/opus",
        Some("png") => "image/png",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        Some("avif") => "image/avif",
        Some("svg") => "image/svg+xml",
        Some("pdf") => "application/pdf",
        Some("txt" | "md" | "csv" | "log") => "text/plain",
        Some("json") => "application/json",
        Some("mp4" | "m4v") => "video/mp4",
        Some("mov") => "video/quicktime",
        Some("webm") => "video/webm",
        Some("html" | "htm") => "text/html",
        _ => "application/octet-stream",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn folders_files_paths_and_clashes() {
        let a = LoroDoc::new();
        let stems = mkdir(&a, None, "Stems").unwrap();
        let kick = create_file(&a, Some(stems), "kick.wav").unwrap();
        set_content(&a, kick, 12, &"a".repeat(64)).unwrap();
        a.commit();
        assert_eq!(path_of(&a, kick), "Stems/kick.wav");
        assert_eq!(resolve(&a, "Stems/kick.wav").unwrap().unwrap().size, 12);
        assert_eq!(files(&a).len(), 1);

        // Two devices make "notes.txt" at once: both survive, told apart.
        let b = LoroDoc::new();
        b.import(&a.export(loro::ExportMode::Snapshot).unwrap())
            .unwrap();
        create_file(&a, None, "notes.txt").unwrap();
        create_file(&b, None, "notes.txt").unwrap();
        a.commit();
        b.commit();
        a.import(&b.export(loro::ExportMode::Snapshot).unwrap())
            .unwrap();
        let names: Vec<String> = children(&a, None).into_iter().map(|e| e.name).collect();
        assert_eq!(names, ["Stems", "notes.txt", "notes (2).txt"]);

        // Rename and move.
        rename(&a, kick, None, "Kick In.wav").unwrap();
        assert!(resolve(&a, "Kick In.wav").unwrap().is_some());
        assert!(children(&a, Some(stems)).is_empty());
        // Removing moves into the Trash; nothing is deleted.
        assert!(trash(&a, stems).unwrap());
        assert!(resolve(&a, "Stems").is_none());
        assert!(resolve(&a, "Trash/Stems").unwrap().is_some());
        assert!(in_trash(&a, stems));
        // Removing from the Trash (or the Trash itself) is refused.
        assert!(!trash(&a, stems).unwrap());
        let bin = resolve(&a, "Trash").unwrap().unwrap().id;
        assert!(!trash(&a, bin).unwrap());
        // A second "Stems" goes in beside the first.
        let again = mkdir(&a, None, "Stems").unwrap();
        assert!(trash(&a, again).unwrap());
        assert!(resolve(&a, "Trash/Stems (2)").unwrap().is_some());
        // A trashed file keeps its bytes.
        let gone = create_file(&a, None, "mix.wav").unwrap();
        set_content(&a, gone, 9, &"b".repeat(64)).unwrap();
        assert!(trash(&a, gone).unwrap());
        assert_eq!(
            resolve(&a, "Trash/mix.wav").unwrap().unwrap().hash,
            "b".repeat(64)
        );
    }
}

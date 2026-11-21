//! Actions that are only document edits: tags and fields (the tags
//! document) and the Drive tree's moves, copies and Trash (the files
//! document). The node runs these for the Drive tree, the agents for the
//! tags of their files; each returns how to undo it.
//!
//! Nothing here deletes: a Drive "copy" is a new entry for the same blob,
//! a move keeps the entry (and its id), the Trash is a folder.

use drive_files::tree::{self, Entry};
use drive_spine::tags::{self, FieldKind, Library, Target};
use loro::{LoroDoc, TreeID};

use crate::eval::{Effect, Item, Spot};
use crate::journal::Undo;
use crate::names::free_name;

/// A Drive folder by path, made (with its parents) when missing.
/// `""`: the top.
pub fn ensure_folder(files: &LoroDoc, path: &str) -> Result<Option<TreeID>, String> {
    let mut parent = None;
    for piece in path.split('/').filter(|p| !p.is_empty()) {
        parent = Some(match tree::lookup(files, parent, piece) {
            Some(e) if e.dir => e.id,
            Some(_) => return Err(format!("{piece} is a file, not a folder")),
            None => tree::mkdir(files, parent, piece).map_err(|e| e.to_string())?,
        });
    }
    Ok(parent)
}

fn entry_of(files: &LoroDoc, item: &Item) -> Result<Entry, String> {
    let Item::Drive { id, .. } = item else {
        return Err("not a Drive file".into());
    };
    let tid = tree::parse_id(id).ok_or("not a Drive id")?;
    tree::entry(files, tid).ok_or_else(|| format!("{} is no longer in the Drive", item.path()))
}

fn folder_of(files: &LoroDoc, id: TreeID) -> String {
    tree::parent_of(files, id)
        .map(|p| tree::path_of(files, p))
        .unwrap_or_default()
}

/// Put an entry in a folder under a free name (never over another entry).
fn place(files: &LoroDoc, id: TreeID, folder: &str, name: &str) -> Result<String, String> {
    let parent = ensure_folder(files, folder)?;
    let free = free_name(name, |n| {
        tree::lookup(files, parent, n).is_some_and(|e| e.id != id)
    });
    tree::rename(files, id, parent, &free).map_err(|e| e.to_string())?;
    Ok(free)
}

/// Do a Drive-tree effect (move, copy, Trash) to a Drive file. Returns
/// where it is (or the copy) after, and how to undo it.
pub fn apply_files(files: &LoroDoc, item: &Item, effect: &Effect) -> Result<(Item, Undo), String> {
    let e = entry_of(files, item)?;
    let id = tree::id_string(e.id);
    let was_folder = folder_of(files, e.id);
    let at = |folder: &str, name: &str, id: &str| Item::Drive {
        id: id.to_string(),
        path: if folder.is_empty() {
            name.to_string()
        } else {
            format!("{folder}/{name}")
        },
    };
    match effect {
        Effect::Move {
            to: Spot::Drive { folder, name },
        } => {
            let put = place(files, e.id, folder, name)?;
            Ok((
                at(folder, &put, &id),
                Undo::TreeMove {
                    id,
                    folder: was_folder,
                    name: e.name,
                },
            ))
        }
        Effect::Copy {
            to: Spot::Drive { folder, name },
        } => {
            if e.dir {
                return Err("copying a Drive folder is not built yet".into());
            }
            if e.hash.is_empty() {
                return Err("its bytes have not arrived yet".into());
            }
            let parent = ensure_folder(files, folder)?;
            let free = free_name(name, |n| tree::lookup(files, parent, n).is_some());
            let new = tree::create_file(files, parent, &free).map_err(|e| e.to_string())?;
            tree::set_content(files, new, e.size, &e.hash).map_err(|e| e.to_string())?;
            let nid = tree::id_string(new);
            Ok((at(folder, &free, &nid), Undo::TreeCopy { id: nid }))
        }
        Effect::Trash => {
            if !tree::trash(files, e.id).map_err(|e| e.to_string())? {
                return Err("it is already in the Trash".into());
            }
            let now = tree::entry(files, e.id).map(|x| x.name).unwrap_or_default();
            Ok((
                at(tree::TRASH, &now, &id),
                Undo::TreeTrash {
                    id,
                    folder: was_folder,
                    name: e.name,
                },
            ))
        }
        other => Err(format!("{} is not a Drive tree action", other.kind())),
    }
}

/// Undo a Drive-tree action. A copy taken back goes to the Trash.
pub fn undo_files(files: &LoroDoc, undo: &Undo) -> Result<(), String> {
    match undo {
        Undo::TreeMove { id, folder, name } | Undo::TreeTrash { id, folder, name } => {
            let tid = tree::parse_id(id).ok_or("not a Drive id")?;
            tree::entry(files, tid).ok_or("it is no longer in the Drive")?;
            place(files, tid, folder, name).map(|_| ())
        }
        Undo::TreeCopy { id } => {
            let tid = tree::parse_id(id).ok_or("not a Drive id")?;
            tree::entry(files, tid).ok_or("the copy is no longer in the Drive")?;
            tree::trash(files, tid).map_err(|e| e.to_string())?;
            Ok(())
        }
        _ => Err("not a Drive tree action".into()),
    }
}

/// Do a tag or field effect on a thing. Returns how to undo it.
pub fn apply_tags(
    doc: &LoroDoc,
    target: &Target,
    effect: &Effect,
    by: &str,
    now: i64,
) -> Result<Undo, String> {
    match effect {
        Effect::Tag { tag, on } => {
            if *on {
                tags::tag(doc, target, tag, by, now).map_err(|e| e.to_string())?;
            } else {
                tags::untag(doc, target, tag).map_err(|e| e.to_string())?;
            }
            Ok(Undo::Tag {
                target: target.key(),
                tag: tag.clone(),
                on: !*on,
            })
        }
        Effect::Field { name, value } => {
            let lib = Library::load(doc);
            let had = lib
                .on(target)
                .fields
                .into_iter()
                .find(|f| f.name.eq_ignore_ascii_case(name));
            match had {
                Some(f) => {
                    tags::set_field(doc, target, &f.id, value, by, now)
                        .map_err(|e| e.to_string())?;
                    Ok(Undo::Field {
                        target: target.key(),
                        field: f.id,
                        old: Some(f.value),
                    })
                }
                None => {
                    let id = tags::add_field(doc, target, name, FieldKind::Text, value, by, now)
                        .map_err(|e| e.to_string())?;
                    Ok(Undo::Field {
                        target: target.key(),
                        field: id,
                        old: None,
                    })
                }
            }
        }
        other => Err(format!("{} is not a tag action", other.kind())),
    }
}

/// Undo a tag or field action.
pub fn undo_tags(doc: &LoroDoc, undo: &Undo, by: &str, now: i64) -> Result<(), String> {
    match undo {
        Undo::Tag { target, tag, on } => {
            let t = Target::parse(target).ok_or("not a target")?;
            if *on {
                tags::tag(doc, &t, tag, by, now).map_err(|e| e.to_string())
            } else {
                tags::untag(doc, &t, tag).map_err(|e| e.to_string())
            }
        }
        Undo::Field { target, field, old } => {
            let t = Target::parse(target).ok_or("not a target")?;
            match old {
                Some(v) => tags::set_field(doc, &t, field, v, by, now).map_err(|e| e.to_string()),
                None => tags::remove_field(doc, &t, field).map_err(|e| e.to_string()),
            }
        }
        _ => Err("not a tag action".into()),
    }
}

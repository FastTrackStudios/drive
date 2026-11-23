//! Adopted folders: a folder that lives by itself on a disk, taken over by
//! Drive in place. The folder becomes (or already is) a location, indexed
//! where it is, and an adoption maps it into the Drive tree at a logical
//! path the owner chooses (`Studio/Songs`). Nothing is copied or moved:
//! the mount shows the folder's own files at that path, and
//! `Devices/<device>/…` still shows where the bytes physically are.
//!
//! The adoptions live in the space's registry (`drive:space:<space>`),
//! under `adopted`: logical path → the location, and the folder inside it
//! (`sub`, empty for its root). A logical path holds at most one adoption,
//! and adoptions never nest: the logical tree under an adoption is the
//! folder's own.
//!
//! The same location may be adopted at several logical paths: views, not
//! copies. Removing an adoption removes the mapping only; the location,
//! its index and its files stay as they are.

use loro::{LoroDoc, LoroMap, LoroValue, ValueOrContainer};

/// The mount's own folder at the top (every device's disks and
/// locations): never a logical path's first part.
pub const DEVICES: &str = "Devices";

/// One adopted folder.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Adoption {
    /// The logical path in the Drive tree (`Studio/Songs`).
    pub at: String,
    /// The location the folder is in.
    pub location: String,
    /// The folder inside the location, `/`-separated (empty: its root).
    pub sub: String,
    /// The device that adopted it.
    pub by: String,
    /// When (ms since the epoch).
    pub added: i64,
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

/// A logical path, made canonical: `/`-separated parts with no empty,
/// `.` or `..` part, not the top, not under the Trash or the mount's
/// `Devices` folder.
pub fn logical_path(raw: &str) -> Result<String, String> {
    let parts: Vec<&str> = raw.split('/').filter(|p| !p.is_empty()).collect();
    if parts.is_empty() {
        return Err("a logical path is needed (like Studio/Songs), not the top".into());
    }
    if parts
        .iter()
        .any(|p| *p == "." || *p == ".." || p.contains('\0'))
    {
        return Err(format!("{raw}: no `.` or `..` in a logical path"));
    }
    if parts[0] == crate::tree::TRASH || parts[0] == DEVICES {
        return Err(format!("{raw}: `{}` at the top is Drive's own", parts[0]));
    }
    Ok(parts.join("/"))
}

/// A sub-path inside a location, made canonical (`""`: its root).
pub fn sub_path(raw: &str) -> Result<String, String> {
    let parts: Vec<&str> = raw.split('/').filter(|p| !p.is_empty()).collect();
    if parts.iter().any(|p| *p == "." || *p == "..") {
        return Err(format!("{raw}: a path outside the location"));
    }
    Ok(parts.join("/"))
}

/// Whether `path` is `base` or under it.
fn under(path: &str, base: &str) -> bool {
    path == base || (path.starts_with(base) && path.as_bytes().get(base.len()) == Some(&b'/'))
}

/// Why `at` cannot be adopted beside these adoptions, if it cannot: taken,
/// or above or below another one.
pub fn conflict(adoptions: &[Adoption], at: &str) -> Option<String> {
    adoptions.iter().find_map(|a| {
        if a.at == at {
            Some(format!("{at} is already adopted"))
        } else if under(at, &a.at) {
            Some(format!("{at} is inside {}, which is adopted", a.at))
        } else if under(&a.at, at) {
            Some(format!("{} is adopted inside {at}", a.at))
        } else {
            None
        }
    })
}

/// Record an adoption (its logical path must be canonical, see
/// [`logical_path`]).
pub fn adopt(doc: &LoroDoc, a: &Adoption) -> loro::LoroResult<()> {
    let m = doc.get_map("adopted").ensure_mergeable_map(&a.at)?;
    m.insert("at", a.at.as_str())?;
    m.insert("location", a.location.as_str())?;
    m.insert("sub", a.sub.as_str())?;
    m.insert("by", a.by.as_str())?;
    m.insert("added", a.added)?;
    Ok(())
}

/// Every adoption, by logical path.
pub fn adoptions(doc: &LoroDoc) -> Vec<Adoption> {
    let map = doc.get_map("adopted");
    let mut out: Vec<Adoption> = map
        .keys()
        .filter_map(|k| match map.get(&k) {
            Some(ValueOrContainer::Container(c)) => c.into_map().ok(),
            _ => None,
        })
        .map(|m| Adoption {
            at: s(&m, "at"),
            location: s(&m, "location"),
            sub: s(&m, "sub"),
            by: s(&m, "by"),
            added: n(&m, "added"),
        })
        .filter(|a| !a.at.is_empty() && !a.location.is_empty())
        .collect();
    out.sort_by(|a, b| a.at.cmp(&b.at));
    out
}

/// Remove the adoption at a logical path: the mapping only. `None`: there
/// was none.
pub fn unadopt(doc: &LoroDoc, at: &str) -> loro::LoroResult<Option<Adoption>> {
    let Ok(at) = logical_path(at) else {
        return Ok(None);
    };
    let Some(a) = adoptions(doc).into_iter().find(|a| a.at == at) else {
        return Ok(None);
    };
    doc.get_map("adopted").delete(&at)?;
    Ok(Some(a))
}

/// What a logical path is, as far as adoptions go.
#[derive(Debug, PartialEq, Eq)]
pub enum Place<'a> {
    /// Inside an adopted folder: `rel` is the path under it (`""`: the
    /// adopted folder itself).
    Inside { adoption: &'a Adoption, rel: String },
    /// A folder on the way to one or more adoptions (it may or may not
    /// also be a folder of the Drive tree).
    Above,
    /// Nothing adopted here.
    Outside,
}

impl<'a> Place<'a> {
    /// The path inside the adoption's location (`sub` joined with `rel`).
    pub fn location_path(&self) -> Option<String> {
        match self {
            Place::Inside { adoption, rel } => Some(join(&adoption.sub, rel)),
            _ => None,
        }
    }
}

/// `a/b` + `c` (either may be empty).
pub fn join(a: &str, b: &str) -> String {
    match (a.is_empty(), b.is_empty()) {
        (true, _) => b.to_string(),
        (_, true) => a.to_string(),
        _ => format!("{a}/{b}"),
    }
}

/// Where a logical path (`""`: the top) falls.
pub fn place<'a>(adoptions: &'a [Adoption], path: &str) -> Place<'a> {
    let path = path.trim_matches('/');
    for a in adoptions {
        if under(path, &a.at) {
            let rel = path[a.at.len()..].trim_start_matches('/').to_string();
            return Place::Inside { adoption: a, rel };
        }
    }
    if adoptions
        .iter()
        .any(|a| path.is_empty() || under(&a.at, path))
    {
        Place::Above
    } else {
        Place::Outside
    }
}

/// The names adoptions add to a logical folder that is not inside one:
/// (name, whether it is an adopted folder itself rather than a folder on
/// the way to one). Sorted, each once.
pub fn names_in(adoptions: &[Adoption], dir: &str) -> Vec<(String, bool)> {
    let dir = dir.trim_matches('/');
    let mut out: Vec<(String, bool)> = Vec::new();
    for a in adoptions {
        let rest = if dir.is_empty() {
            a.at.as_str()
        } else if under(&a.at, dir) && a.at != dir {
            &a.at[dir.len() + 1..]
        } else {
            continue;
        };
        let (name, point) = match rest.split_once('/') {
            Some((first, _)) => (first, false),
            None => (rest, true),
        };
        match out.iter_mut().find(|(n, _)| n == name) {
            Some(e) => e.1 |= point,
            None => out.push((name.to_string(), point)),
        }
    }
    out.sort();
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn a(at: &str, loc: &str, sub: &str) -> Adoption {
        Adoption {
            at: at.into(),
            location: loc.into(),
            sub: sub.into(),
            by: "dev".into(),
            added: 1,
        }
    }

    #[test]
    fn logical_paths_are_canonical_and_never_drives_own() {
        assert_eq!(logical_path("/Studio//Songs/").unwrap(), "Studio/Songs");
        assert!(logical_path("").is_err());
        assert!(logical_path("/").is_err());
        assert!(logical_path("Studio/../x").is_err());
        assert!(logical_path("Trash/Songs").is_err());
        assert!(logical_path("Devices").is_err());
        assert_eq!(logical_path("Studio/Devices").unwrap(), "Studio/Devices");
        assert_eq!(sub_path("a//b/").unwrap(), "a/b");
        assert!(sub_path("a/../b").is_err());
    }

    #[test]
    fn adoptions_persist_in_the_registry_and_never_nest() {
        let d = LoroDoc::new();
        adopt(&d, &a("Studio/Songs", "loc1", "")).unwrap();
        adopt(&d, &a("Archive/Sessions", "loc1", "Old")).unwrap();
        // Survives a round trip through the document's bytes.
        let bytes = d.export(loro::ExportMode::Snapshot).unwrap();
        let d2 = LoroDoc::new();
        d2.import(&bytes).unwrap();
        let all = adoptions(&d2);
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].at, "Archive/Sessions");
        assert_eq!(all[0].sub, "Old");

        assert!(conflict(&all, "Studio/Songs").is_some());
        assert!(conflict(&all, "Studio/Songs/Mine").is_some());
        assert!(conflict(&all, "Studio").is_some());
        assert!(conflict(&all, "Studio/Song").is_none());
        assert!(conflict(&all, "Studio/Songs2").is_none());

        assert_eq!(
            unadopt(&d2, "/Studio/Songs/").unwrap().unwrap().location,
            "loc1"
        );
        assert_eq!(unadopt(&d2, "Studio/Songs").unwrap(), None);
        assert_eq!(adoptions(&d2).len(), 1);
    }

    #[test]
    fn places_and_names() {
        let all = vec![
            a("Archive/Sessions", "l2", "Old"),
            a("Studio/Songs", "l1", ""),
        ];
        assert_eq!(place(&all, ""), Place::Above);
        assert_eq!(place(&all, "Studio"), Place::Above);
        assert_eq!(place(&all, "Elsewhere"), Place::Outside);
        assert_eq!(place(&all, "Studio/Song"), Place::Outside);
        let p = place(&all, "Studio/Songs/Song A/take.wav");
        assert_eq!(
            p,
            Place::Inside {
                adoption: &all[1],
                rel: "Song A/take.wav".into()
            }
        );
        assert_eq!(p.location_path().unwrap(), "Song A/take.wav");
        assert_eq!(
            place(&all, "Archive/Sessions/x").location_path().unwrap(),
            "Old/x"
        );
        assert_eq!(
            names_in(&all, ""),
            vec![("Archive".into(), false), ("Studio".into(), false)]
        );
        assert_eq!(names_in(&all, "Studio"), vec![("Songs".into(), true)]);
        assert!(names_in(&all, "Studio/Songs").is_empty());
        assert!(names_in(&all, "Elsewhere").is_empty());
    }
}

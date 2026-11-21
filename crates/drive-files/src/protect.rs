//! Copies and protection: which locations hold the same file (by sampled
//! hash and size, across every device's index, the offline ones too), and
//! whether each file meets the space's policies (3-2-1 and the like).
//!
//! Counting: a **copy** is a location holding the file; a **medium** is a
//! disk or a storage (two folders on one disk are one medium); a copy is
//! **offsite** when its holder's site differs from the file's own
//! location's site. Holders with no site set share one unnamed site.

use std::collections::{HashMap, HashSet};

use serde::Serialize;

use crate::space::{Location, Policy};

/// One file as an index has it.
#[derive(Clone, Debug)]
pub struct FileRow {
    pub location: String,
    pub path: String,
    pub size: u64,
    pub sampled: String,
}

/// How well one file is kept.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Kept {
    pub copies: usize,
    pub media: usize,
    pub offsite: usize,
}

#[derive(Clone, Debug, Serialize)]
pub struct Short {
    pub location: String,
    pub path: String,
    pub size: u64,
    pub kept: Kept,
}

#[derive(Clone, Debug, Serialize)]
pub struct LocationReport {
    pub location: String,
    pub files: usize,
    pub ok: usize,
    pub bytes: u64,
    pub bytes_ok: u64,
}

#[derive(Clone, Debug, Serialize)]
pub struct Report {
    pub policy: Policy,
    pub files: usize,
    pub ok: usize,
    pub bytes: u64,
    pub bytes_ok: u64,
    pub locations: Vec<LocationReport>,
    /// The files that fall short, largest first (at most `limit`).
    pub short: Vec<Short>,
}

/// Every location holding each content (sampled hash + size).
pub struct Copies<'a> {
    by_content: HashMap<(String, u64), Vec<&'a FileRow>>,
}

impl<'a> Copies<'a> {
    pub fn new(rows: &'a [FileRow]) -> Copies<'a> {
        let mut by_content: HashMap<(String, u64), Vec<&FileRow>> = HashMap::new();
        for r in rows.iter().filter(|r| !r.sampled.is_empty()) {
            by_content.entry((r.sampled.clone(), r.size)).or_default().push(r);
        }
        Copies { by_content }
    }

    /// Where else a file is (itself included).
    pub fn of(&self, sampled: &str, size: u64) -> &[&'a FileRow] {
        self.by_content.get(&(sampled.to_string(), size)).map_or(&[], |v| v.as_slice())
    }
}

/// How a file is kept, seen from its location `home`.
pub fn kept(home: &Location, holders: &[&FileRow], locs: &HashMap<&str, &Location>, sites: &HashMap<String, String>) -> Kept {
    let site_of = |l: &Location| sites.get(l.holder()).cloned().unwrap_or_default();
    let home_site = site_of(home);
    let mut copies = HashSet::new();
    let mut media = HashSet::new();
    let mut offsite = HashSet::new();
    for r in holders {
        let Some(l) = locs.get(r.location.as_str()) else { continue };
        copies.insert(r.location.as_str());
        let m = l.medium();
        if site_of(l) != home_site {
            offsite.insert(m.clone());
        }
        media.insert(m);
    }
    Kept { copies: copies.len(), media: media.len(), offsite: offsite.len() }
}

/// Check every policy against every file in its scope.
pub fn check(
    policies: &[Policy],
    locations: &[Location],
    sites: &HashMap<String, String>,
    rows: &[FileRow],
    limit: usize,
) -> Vec<Report> {
    let locs: HashMap<&str, &Location> = locations.iter().map(|l| (l.id.as_str(), l)).collect();
    let copies = Copies::new(rows);
    policies
        .iter()
        .map(|p| {
            let in_scope = |loc: &str| p.scope.is_empty() || p.scope.iter().any(|s| s == loc);
            let mut per: HashMap<&str, LocationReport> = HashMap::new();
            let mut short = Vec::new();
            let (mut files, mut ok, mut bytes, mut bytes_ok) = (0, 0, 0, 0);
            for r in rows.iter().filter(|r| in_scope(&r.location)) {
                let Some(home) = locs.get(r.location.as_str()) else { continue };
                let holders: Vec<&FileRow> =
                    if r.sampled.is_empty() { vec![r] } else { copies.of(&r.sampled, r.size).to_vec() };
                let k = kept(home, &holders, &locs, sites);
                let good = k.copies as i64 >= p.copies && k.media as i64 >= p.media && k.offsite as i64 >= p.offsite;
                let lr = per.entry(r.location.as_str()).or_insert_with(|| LocationReport {
                    location: r.location.clone(),
                    files: 0,
                    ok: 0,
                    bytes: 0,
                    bytes_ok: 0,
                });
                files += 1;
                bytes += r.size;
                lr.files += 1;
                lr.bytes += r.size;
                if good {
                    ok += 1;
                    bytes_ok += r.size;
                    lr.ok += 1;
                    lr.bytes_ok += r.size;
                } else {
                    short.push(Short { location: r.location.clone(), path: r.path.clone(), size: r.size, kept: k });
                }
            }
            short.sort_by(|a, b| b.size.cmp(&a.size));
            short.truncate(limit);
            let mut locations: Vec<LocationReport> = per.into_values().collect();
            locations.sort_by(|a, b| a.location.cmp(&b.location));
            Report { policy: p.clone(), files, ok, bytes, bytes_ok, locations, short }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn loc(id: &str, device: &str, volume: &str, storage: &str) -> Location {
        Location {
            id: id.into(),
            name: id.into(),
            device: device.into(),
            path: format!("/{id}"),
            volume: volume.into(),
            storage: storage.into(),
            index: String::new(),
            files: 0,
            bytes: 0,
            built: 0,
        }
    }

    fn row(location: &str, path: &str, sampled: &str) -> FileRow {
        FileRow { location: location.into(), path: path.into(), size: 100, sampled: sampled.into() }
    }

    #[test]
    fn three_two_one() {
        let locations = [
            loc("work", "studio-pc", "/", ""),
            loc("work-copy", "studio-pc", "/", ""), // same disk: no new medium
            loc("ext", "studio-pc", "/mnt/ext", ""),
            loc("deep", "nas", "/", "garage"),
        ];
        let sites: HashMap<String, String> =
            [("studio-pc", "studio"), ("garage", "offsite")].map(|(a, b)| (a.to_string(), b.to_string())).into();
        let rule = Policy {
            id: "321".into(),
            name: "3-2-1".into(),
            scope: vec!["work".into()],
            copies: 3,
            media: 2,
            offsite: 1,
        };
        let rows = vec![
            // a.wav: work, ext, deep → 3 copies, 3 media, 1 offsite.
            row("work", "a.wav", "A"),
            row("ext", "backup/a.wav", "A"),
            row("deep", "a.wav", "A"),
            // b.wav: work and its copy on the same disk only.
            row("work", "b.wav", "B"),
            row("work-copy", "b.wav", "B"),
            row("work", "c.wav", "C"),
        ];
        let r = &check(&[rule], &locations, &sites, &rows, 10)[0];
        assert_eq!((r.files, r.ok), (3, 1));
        let b = r.short.iter().find(|s| s.path == "b.wav").unwrap();
        assert_eq!(b.kept, Kept { copies: 2, media: 1, offsite: 0 });
        let c = r.short.iter().find(|s| s.path == "c.wav").unwrap();
        assert_eq!(c.kept, Kept { copies: 1, media: 1, offsite: 0 });
    }
}

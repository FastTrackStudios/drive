//! `folder_summary`'s sums, added up a page at a time (drive#156): a
//! folder of millions of files is never held, only its totals and the
//! few rows each list shows.

use std::cmp::Reverse;
use std::collections::HashMap;

use drive_spine::kinds::Kinds;
use serde_json::{Value, json};

use super::ctx::human;

/// One file or folder summed.
#[derive(Clone, Debug)]
pub struct Row {
    pub path: String,
    pub dir: bool,
    pub size: u64,
    pub mtime: i64,
    pub content: String,
}

impl From<drive_files::index::IndexEntry> for Row {
    fn from(e: drive_files::index::IndexEntry) -> Row {
        Row {
            path: e.path,
            dir: e.dir,
            size: e.size,
            mtime: e.mtime,
            content: e.sampled,
        }
    }
}

/// The `n` rows first by `K`, kept as they come.
struct Top<K: Ord + Clone> {
    n: usize,
    rows: Vec<(K, Row)>,
    /// The last of the `n` at the last trim: nothing after it can be in.
    bound: Option<K>,
}

impl<K: Ord + Clone> Top<K> {
    fn new(n: usize) -> Top<K> {
        Top {
            n,
            rows: Vec::new(),
            bound: None,
        }
    }

    fn add(&mut self, key: K, row: &Row) {
        if self.bound.as_ref().is_some_and(|b| key >= *b) {
            return;
        }
        self.rows.push((key, row.clone()));
        if self.rows.len() >= self.n * 2 {
            self.trim();
        }
    }

    fn trim(&mut self) {
        self.rows.sort_by(|a, b| a.0.cmp(&b.0));
        self.rows.truncate(self.n);
        if self.rows.len() == self.n {
            self.bound = self.rows.last().map(|(k, _)| k.clone());
        }
    }

    fn into_rows(mut self) -> Vec<Row> {
        self.trim();
        self.rows.into_iter().map(|(_, r)| r).collect()
    }
}

/// Files and bytes.
type Count = (u64, u64);

/// A folder's sums so far.
pub struct Summary<'k> {
    kinds: &'k Kinds,
    files: u64,
    folders: u64,
    bytes: u64,
    by_kind: HashMap<String, Count>,
    by_ext: HashMap<String, Count>,
    largest: Top<(Reverse<u64>, String)>,
    oldest: Top<(i64, String)>,
    newest: Top<(Reverse<i64>, Reverse<String>)>,
}

impl<'k> Summary<'k> {
    /// Sums that keep `top` rows a list.
    pub fn new(kinds: &'k Kinds, top: usize) -> Summary<'k> {
        Summary {
            kinds,
            files: 0,
            folders: 0,
            bytes: 0,
            by_kind: HashMap::new(),
            by_ext: HashMap::new(),
            largest: Top::new(top),
            oldest: Top::new(top),
            newest: Top::new(top),
        }
    }

    pub fn add(&mut self, r: &Row) {
        if r.dir {
            self.folders += 1;
            return;
        }
        self.files += 1;
        self.bytes += r.size;
        let name = r.path.rsplit('/').next().unwrap_or(&r.path);
        let kind = self.kinds.of_file(name, false).id.clone();
        tally(self.by_kind.entry(kind).or_default(), r.size);
        let ext = name
            .rsplit_once('.')
            .map(|(_, x)| x.to_ascii_lowercase())
            .unwrap_or_default();
        tally(self.by_ext.entry(ext).or_default(), r.size);
        self.largest.add((Reverse(r.size), r.path.clone()), r);
        if r.mtime > 0 {
            self.oldest.add((r.mtime, r.path.clone()), r);
            self.newest
                .add((Reverse(r.mtime), Reverse(r.path.clone())), r);
        }
    }

    /// The tool's answer for the folder called `label`.
    pub fn into_json(self, label: &str, dups: Dups) -> Value {
        let brief = |r: &Row| json!({ "path": r.path, "size": r.size, "mtime": r.mtime });
        let list = |t: Vec<Row>| t.iter().map(brief).collect::<Vec<_>>();
        json!({
            "folder": label,
            "files": self.files,
            "folders": self.folders,
            "bytes": self.bytes, "size": human(self.bytes),
            "by_kind": biggest(self.by_kind, 20),
            "by_extension": biggest(self.by_ext, 20),
            "largest": list(self.largest.into_rows()),
            "oldest": list(self.oldest.into_rows()),
            "newest": list(self.newest.into_rows()),
            "duplicates_inside": dups.into_json(),
        })
    }
}

fn tally(c: &mut Count, size: u64) {
    c.0 += 1;
    c.1 += size;
}

/// The `n` names with the most bytes.
fn biggest(m: HashMap<String, Count>, n: usize) -> Vec<Value> {
    let mut v: Vec<_> = m.into_iter().collect();
    v.sort_by_key(|x| Reverse(x.1.1));
    v.into_iter()
        .take(n)
        .map(|(k, (c, b))| json!({ "name": if k.is_empty() { "(none)".to_string() } else { k }, "files": c, "bytes": b, "size": human(b) }))
        .collect()
}

/// Contents held more than once inside a folder: how many, the bytes the
/// extra copies take, and the groups that waste most.
#[derive(Default)]
pub struct Dups {
    pub groups: u64,
    pub wasted: u64,
    /// (wasted, size, paths), most wasted first.
    pub top: Vec<(u64, u64, Vec<String>)>,
}

impl Dups {
    /// Found among `rows` (all of a folder, in memory: the Drive tree's).
    pub fn of_rows(rows: &[Row], top: usize) -> Dups {
        let mut groups: HashMap<&str, Vec<&Row>> = HashMap::new();
        for f in rows
            .iter()
            .filter(|r| !r.dir && !r.content.is_empty() && r.size > 0)
        {
            groups.entry(f.content.as_str()).or_default().push(f);
        }
        let mut found: Vec<(u64, u64, Vec<String>)> = groups
            .into_values()
            .filter(|g| g.len() > 1)
            .map(|g| {
                let wasted = g[0].size * (g.len() as u64 - 1);
                (
                    wasted,
                    g[0].size,
                    g.iter().map(|r| r.path.clone()).collect(),
                )
            })
            .collect();
        found.sort_by_key(|d| Reverse(d.0));
        Dups {
            groups: found.len() as u64,
            wasted: found.iter().map(|d| d.0).sum(),
            top: found.into_iter().take(top).collect(),
        }
    }

    fn into_json(self) -> Value {
        json!({
            "groups": self.groups,
            "wasted_bytes": self.wasted,
            "top": self.top.into_iter().map(|(w, size, paths)| json!({
                "wasted": w, "size": size, "paths": paths
            })).collect::<Vec<_>>()
        })
    }
}

/// Picks the groups that waste most while they are counted.
pub struct DupCounter {
    top: usize,
    groups: u64,
    wasted: u64,
    /// (wasted, sampled, size), most wasted first once trimmed.
    best: Vec<(u64, String, u64)>,
}

impl DupCounter {
    pub fn new(top: usize) -> DupCounter {
        DupCounter {
            top,
            groups: 0,
            wasted: 0,
            best: Vec::new(),
        }
    }

    /// A content held `count` times (a group of `size`-byte files).
    pub fn add(&mut self, sampled: String, size: u64, count: u64) {
        let wasted = size * count.saturating_sub(1);
        self.groups += 1;
        self.wasted += wasted;
        self.best.push((wasted, sampled, size));
        if self.best.len() >= self.top * 2 {
            self.trim();
        }
    }

    fn trim(&mut self) {
        self.best
            .sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
        self.best.truncate(self.top);
    }

    /// The groups to show, (wasted, sampled, size), and the counts.
    pub fn finish(mut self) -> (Vec<(u64, String, u64)>, Dups) {
        self.trim();
        let dups = Dups {
            groups: self.groups,
            wasted: self.wasted,
            top: Vec::new(),
        };
        (self.best, dups)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(path: &str, size: u64, mtime: i64, content: &str) -> Row {
        Row {
            path: path.into(),
            dir: false,
            size,
            mtime,
            content: content.into(),
        }
    }

    #[test]
    fn the_top_rows_are_kept_as_they_come() {
        let mut t: Top<(Reverse<u64>, String)> = Top::new(3);
        for i in 0..100u64 {
            let r = row(&format!("f{i:03}"), (i * 37) % 101, 0, "");
            t.add((Reverse(r.size), r.path.clone()), &r);
        }
        let sizes: Vec<u64> = t.into_rows().iter().map(|r| r.size).collect();
        assert_eq!(sizes, [100, 99, 98]);
    }

    #[test]
    fn duplicates_counted_one_group_at_a_time_match_those_found_in_memory() {
        let rows = vec![
            row("a", 10, 1, "x"),
            row("b", 10, 1, "x"),
            row("c", 10, 1, "x"),
            row("d", 5, 1, "y"),
            row("e", 5, 1, "y"),
            row("f", 7, 1, "z"),
        ];
        let mem = Dups::of_rows(&rows, 1);
        let mut c = DupCounter::new(1);
        c.add("y".into(), 5, 2);
        c.add("x".into(), 10, 3);
        let (best, counted) = c.finish();
        assert_eq!((counted.groups, counted.wasted), (mem.groups, mem.wasted));
        assert_eq!((mem.groups, mem.wasted), (2, 25));
        assert_eq!(best, [(20, "x".to_string(), 10)]);
        assert_eq!(mem.top[0].0, 20);
    }
}

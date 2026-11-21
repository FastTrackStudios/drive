//! A folder of Markdown, Obsidian's included, as dodeca content.
//!
//! - Every folder with pages under it becomes a section (`_index.md`); a
//!   folder note (`index.md`, `README.md`, or the note named like its
//!   folder) is that section's page.
//! - Names become URL slugs (`Circle of Fifths.md` → `/circle-of-fifths/`);
//!   titles stay as they were.
//! - YAML frontmatter (`---`) becomes TOML (`+++`): `title` and `weight`
//!   (or `order`) at the top, everything else (`tags`, `aliases`, `date`,
//!   `description`, …) under `[extra]`. No title: the note's first `# Heading`,
//!   or its file name. `publish: false` or `draft: true` leaves a note out.
//! - `[[Page]]`, `[[Page|label]]`, `[[Page#Heading]]` become links to the
//!   page's route; `![[image.png]]` an image; relative Markdown links to
//!   notes and files (`[x](Other%20Note.md)`) are pointed at their routes.
//!   A link that names nothing is kept as text, and reported.
//! - Callouts (`> [!note] Title`) become a quote with a bold title.
//! - Everything else beside the notes (images, PDFs) is copied into
//!   `static/` under the same slugged folders.

use std::collections::{BTreeMap, HashMap, HashSet};

use regex::Regex;

use crate::source::SourceFile;

/// The site-wide words the root section carries.
#[derive(Clone, Debug, Default)]
pub struct Settings {
    pub title: String,
    pub description: String,
}

/// A source folder, converted.
#[derive(Debug, Default)]
pub struct Converted {
    /// `content/…md` → its text.
    pub content: BTreeMap<String, String>,
    /// `static/…` → the file it is a copy of.
    pub assets: BTreeMap<String, SourceFile>,
    /// Links that named nothing: `[[Target]] in Folder/Note.md`.
    pub unresolved: Vec<String>,
    /// Pages and sections written.
    pub pages: usize,
}

fn is_markdown(path: &str) -> bool {
    path.to_ascii_lowercase().ends_with(".md")
}

fn split_dir(path: &str) -> (&str, &str) {
    match path.rsplit_once('/') {
        Some((d, f)) => (d, f),
        None => ("", path),
    }
}

fn stem(file: &str) -> &str {
    match file.rsplit_once('.') {
        Some((s, _)) if !s.is_empty() => s,
        _ => file,
    }
}

/// A URL slug: lowercase letters and digits, words joined by dashes.
pub fn slug(s: &str) -> String {
    let mut out = String::new();
    let mut dash = false;
    for c in s.chars().flat_map(char::to_lowercase) {
        if c.is_alphanumeric() {
            if dash && !out.is_empty() {
                out.push('-');
            }
            dash = false;
            out.push(c);
        } else if c == '\'' || c == '’' {
            // "Don't" → "dont"
        } else {
            dash = true;
        }
    }
    if out.is_empty() { "page".into() } else { out }
}

/// Where a page or file ends up.
#[derive(Clone, Debug)]
struct Target {
    url: String,
    /// The source path (to prefer targets in the linking note's folder).
    source: String,
}

/// One note, its frontmatter read.
struct Note {
    path: String,
    dir: String,
    /// `content/…`
    out: String,
    url: String,
    fm: serde_yaml_ng::Mapping,
    body: String,
    title: String,
    is_section: bool,
}

fn frontmatter(text: &str) -> (serde_yaml_ng::Mapping, String) {
    let t = text.strip_prefix('\u{feff}').unwrap_or(text);
    let rest = t
        .strip_prefix("---\n")
        .or_else(|| t.strip_prefix("---\r\n"));
    if let Some(rest) = rest {
        // The closing line: `---` (or `...`).
        let mut offset = 0;
        for line in rest.split_inclusive('\n') {
            let l = line.trim_end();
            if l == "---" || l == "..." {
                let yaml = &rest[..offset];
                let body = rest[offset + line.len()..].to_string();
                let fm = serde_yaml_ng::from_str::<serde_yaml_ng::Value>(yaml)
                    .ok()
                    .and_then(|v| match v {
                        serde_yaml_ng::Value::Mapping(m) => Some(m),
                        _ => None,
                    })
                    .unwrap_or_default();
                return (fm, body);
            }
            offset += line.len();
        }
    }
    (serde_yaml_ng::Mapping::new(), t.to_string())
}

fn fm_str(fm: &serde_yaml_ng::Mapping, k: &str) -> Option<String> {
    match fm.get(k)? {
        serde_yaml_ng::Value::String(s) => Some(s.clone()),
        serde_yaml_ng::Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

fn fm_bool(fm: &serde_yaml_ng::Mapping, k: &str) -> Option<bool> {
    match fm.get(k)? {
        serde_yaml_ng::Value::Bool(b) => Some(*b),
        _ => None,
    }
}

fn fm_list(fm: &serde_yaml_ng::Mapping, k: &str) -> Vec<String> {
    match fm.get(k) {
        Some(serde_yaml_ng::Value::Sequence(s)) => s
            .iter()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect(),
        Some(serde_yaml_ng::Value::String(s)) => vec![s.clone()],
        _ => Vec::new(),
    }
}

/// The first `# Heading`, if the note starts with one.
fn first_heading(body: &str) -> Option<(usize, usize, String)> {
    let mut offset = 0;
    for line in body.split_inclusive('\n') {
        let l = line.trim();
        if l.is_empty() {
            offset += line.len();
            continue;
        }
        return l
            .strip_prefix("# ")
            .map(|h| (offset, offset + line.len(), h.trim().to_string()));
    }
    None
}

fn yaml_to_toml(v: &serde_yaml_ng::Value) -> Option<toml::Value> {
    use serde_yaml_ng::Value as Y;
    Some(match v {
        Y::Null => return None,
        Y::Bool(b) => toml::Value::Boolean(*b),
        Y::Number(n) => {
            if let Some(i) = n.as_i64() {
                toml::Value::Integer(i)
            } else {
                toml::Value::Float(n.as_f64().unwrap_or(0.0))
            }
        }
        Y::String(s) => toml::Value::String(s.clone()),
        Y::Sequence(s) => toml::Value::Array(s.iter().filter_map(yaml_to_toml).collect()),
        Y::Mapping(m) => {
            let mut t = toml::Table::new();
            for (k, v) in m {
                let key = match k {
                    Y::String(s) => s.clone(),
                    other => serde_yaml_ng::to_string(other)
                        .unwrap_or_default()
                        .trim()
                        .to_string(),
                };
                if let Some(v) = yaml_to_toml(v) {
                    t.insert(key, v);
                }
            }
            toml::Value::Table(t)
        }
        Y::Tagged(t) => return yaml_to_toml(&t.value),
    })
}

/// The `+++` block for a note.
fn toml_front(title: &str, fm: &serde_yaml_ng::Mapping, root_description: Option<&str>) -> String {
    let mut top = toml::Table::new();
    top.insert("title".into(), toml::Value::String(title.to_string()));
    let weight = ["weight", "order", "nav_order"]
        .iter()
        .find_map(|k| fm.get(*k).and_then(|v| v.as_i64()));
    if let Some(w) = weight {
        top.insert("weight".into(), toml::Value::Integer(w));
    }
    if let Some(d) = root_description.filter(|d| !d.is_empty()) {
        top.insert("description".into(), toml::Value::String(d.to_string()));
    }
    let mut extra = toml::Table::new();
    for (k, v) in fm {
        let Some(k) = k.as_str() else { continue };
        if matches!(k, "title" | "weight" | "order" | "nav_order") {
            continue;
        }
        if let Some(v) = yaml_to_toml(v) {
            extra.insert(k.to_string(), v);
        }
    }
    if !extra.is_empty() {
        top.insert("extra".into(), toml::Value::Table(extra));
    }
    let text = toml::to_string(&top).unwrap_or_else(|_| format!("title = {title:?}\n"));
    format!("+++\n{}+++\n", text)
}

/// Names that are unique among their siblings: `a`, `a-2`, …
fn unique(taken: &mut HashSet<String>, base: String) -> String {
    let mut name = base.clone();
    let mut k = 2;
    while !taken.insert(name.clone()) {
        name = format!("{base}-{k}");
        k += 1;
    }
    name
}

struct Index {
    by_path: HashMap<String, Target>,
    by_name: HashMap<String, Vec<Target>>,
    by_title: HashMap<String, Target>,
}

impl Index {
    /// What `[[target]]` (or a relative link, already joined to the
    /// note's folder) names.
    fn find(&self, target: &str, from_dir: &str) -> Option<&Target> {
        let t = target.trim().trim_start_matches('/').to_lowercase();
        let no_md = t.strip_suffix(".md").unwrap_or(&t);
        if let Some(x) = self.by_path.get(no_md) {
            return Some(x);
        }
        let name = no_md.rsplit('/').next().unwrap_or(no_md);
        if let Some(c) = self.by_name.get(name) {
            let here = from_dir.to_lowercase();
            return c
                .iter()
                .find(|x| split_dir(&x.source).0.to_lowercase() == here)
                .or_else(|| c.iter().min_by_key(|x| x.source.len()));
        }
        self.by_title.get(no_md)
    }
}

fn escape_label(s: &str) -> String {
    s.replace('[', "\\[").replace(']', "\\]")
}

fn escape_html(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%'
            && i + 3 <= b.len()
            && let Ok(hex) = std::str::from_utf8(&b[i + 1..i + 3])
            && let Ok(v) = u8::from_str_radix(hex, 16)
        {
            out.push(v);
            i += 3;
            continue;
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// `a/b/../c` → `a/c`.
fn normalize(path: &str) -> String {
    let mut parts: Vec<&str> = Vec::new();
    for p in path.split('/') {
        match p {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            p => parts.push(p),
        }
    }
    parts.join("/")
}

struct Rewriter<'a> {
    index: &'a Index,
    wiki: Regex,
    md_link: Regex,
    callout: Regex,
}

impl Rewriter<'_> {
    /// One line of a note's body, outside code.
    fn line(&self, line: &str, dir: &str, note: &str, missing: &mut Vec<String>) -> String {
        let line = self
            .callout
            .replace(line, |c: &regex::Captures| {
                let kind = &c[2];
                let title = c[4].trim();
                let title = if title.is_empty() {
                    let mut k = kind.to_string();
                    if let Some(f) = k.get_mut(0..1) {
                        f.make_ascii_uppercase();
                    }
                    k
                } else {
                    title.to_string()
                };
                format!("{}**{}**", &c[1], title)
            })
            .into_owned();
        // Code spans stay as they are: rewrite only between backticks.
        let mut out = String::new();
        for (i, part) in line.split('`').enumerate() {
            if i > 0 {
                out.push('`');
            }
            if i % 2 == 1 {
                out.push_str(part);
            } else {
                out.push_str(&self.text(part, dir, note, missing));
            }
        }
        out
    }

    fn text(&self, s: &str, dir: &str, note: &str, missing: &mut Vec<String>) -> String {
        let s = self.wiki.replace_all(s, |c: &regex::Captures| {
            let embed = !c[1].is_empty();
            let inner = &c[2];
            let (target, label) = match inner.split_once('|') {
                Some((t, l)) => (t.trim(), Some(l.trim())),
                None => (inner.trim(), None),
            };
            let (page, anchor) = match target.split_once('#') {
                Some((p, a)) => (p.trim(), Some(a.trim().trim_start_matches('^'))),
                None => (target, None),
            };
            let shown = label.map(str::to_string).unwrap_or_else(|| match anchor {
                Some(a) if page.is_empty() => a.to_string(),
                Some(a) => format!("{page} > {a}"),
                None => page.rsplit('/').next().unwrap_or(page).to_string(),
            });
            // `[[#Heading]]`: a heading on this page.
            if page.is_empty() {
                return format!(
                    "[{}](#{})",
                    escape_label(&shown),
                    slug(anchor.unwrap_or_default())
                );
            }
            match self.index.find(page, dir) {
                Some(t) => {
                    let url = match anchor {
                        Some(a) if !a.is_empty() => format!("{}#{}", t.url, slug(a)),
                        _ => t.url.clone(),
                    };
                    let is_image = matches!(
                        page.rsplit('.')
                            .next()
                            .map(str::to_ascii_lowercase)
                            .as_deref(),
                        Some("png" | "jpg" | "jpeg" | "gif" | "webp" | "svg" | "avif" | "jxl")
                    );
                    if embed && is_image {
                        // `![[x.png|300]]`: a size, not a caption.
                        let alt = label
                            .filter(|l| !l.chars().all(|c| c.is_ascii_digit() || c == 'x'))
                            .unwrap_or_else(|| stem(page.rsplit('/').next().unwrap_or(page)));
                        format!("![{}]({url})", escape_label(alt))
                    } else {
                        format!("[{}]({url})", escape_label(&shown))
                    }
                }
                None => {
                    missing.push(format!("[[{page}]] in {note}"));
                    format!(
                        "<span class=\"missing-link\">{}</span>",
                        escape_html(&shown)
                    )
                }
            }
        });
        self.md_link
            .replace_all(&s, |c: &regex::Captures| {
                let whole = c[0].to_string();
                let dest = c.get(3).or_else(|| c.get(4)).map_or("", |m| m.as_str());
                if dest.is_empty()
                    || dest.starts_with('/')
                    || dest.starts_with('#')
                    || dest.contains("://")
                    || dest.starts_with("mailto:")
                    || dest.starts_with("data:")
                {
                    return whole;
                }
                let decoded = percent_decode(dest);
                let (path, anchor) = match decoded.split_once('#') {
                    Some((p, a)) => (p.to_string(), Some(a.to_string())),
                    None => (decoded.clone(), None),
                };
                let joined = normalize(&format!("{dir}/{path}"));
                match self.index.find(&joined, dir) {
                    Some(t) => {
                        let url = match anchor {
                            Some(a) if !a.is_empty() => format!("{}#{}", t.url, slug(&a)),
                            _ => t.url.clone(),
                        };
                        format!(
                            "{}[{}]({url}{})",
                            &c[1],
                            &c[2],
                            c.get(5).map_or("", |m| m.as_str())
                        )
                    }
                    None => whole,
                }
            })
            .into_owned()
    }

    fn body(&self, body: &str, dir: &str, note: &str, missing: &mut Vec<String>) -> String {
        let mut out = String::with_capacity(body.len());
        let mut fence: Option<String> = None;
        for line in body.split_inclusive('\n') {
            let t = line.trim_start();
            let marker = if t.starts_with("```") {
                Some("```")
            } else if t.starts_with("~~~") {
                Some("~~~")
            } else {
                None
            };
            match (&fence, marker) {
                (None, Some(m)) => {
                    fence = Some(m.to_string());
                    out.push_str(line);
                }
                (Some(f), Some(m)) if f == m => {
                    fence = None;
                    out.push_str(line);
                }
                (Some(_), _) => out.push_str(line),
                (None, None) => {
                    let (text, nl) = match line.strip_suffix('\n') {
                        Some(t) => (t, "\n"),
                        None => (line, ""),
                    };
                    out.push_str(&self.line(text, dir, note, missing));
                    out.push_str(nl);
                }
            }
        }
        out
    }
}

/// Convert a source folder. `texts` holds every Markdown file's text (by
/// its source path); other files are copied as they are.
pub fn convert(
    files: &[SourceFile],
    texts: &HashMap<String, String>,
    settings: &Settings,
) -> Converted {
    let mut out = Converted::default();

    // Folders: original path → slugged path, unique among siblings.
    let mut dirs: BTreeMap<String, String> = BTreeMap::new();
    dirs.insert(String::new(), String::new());
    let mut taken: HashMap<String, HashSet<String>> = HashMap::new();
    let mut all_dirs: Vec<String> = files
        .iter()
        .flat_map(|f| {
            let (d, _) = split_dir(&f.path);
            let parts: Vec<&str> = d.split('/').filter(|p| !p.is_empty()).collect();
            (1..=parts.len())
                .map(|i| parts[..i].join("/"))
                .collect::<Vec<_>>()
        })
        .collect();
    all_dirs.sort();
    all_dirs.dedup();
    for d in all_dirs {
        let (parent, name) = split_dir(&d);
        let parent_slug = dirs.get(parent).cloned().unwrap_or_default();
        let s = unique(taken.entry(parent_slug.clone()).or_default(), slug(name));
        let full = if parent_slug.is_empty() {
            s
        } else {
            format!("{parent_slug}/{s}")
        };
        dirs.insert(d, full);
    }
    let url_of_dir = |d: &str| -> String {
        let s = &dirs[d];
        if s.is_empty() {
            "/".into()
        } else {
            format!("/{s}/")
        }
    };

    // Notes, their frontmatter read; folder notes found.
    let mut notes: Vec<Note> = Vec::new();
    let mut section_note: HashMap<String, usize> = HashMap::new();
    let mut md: Vec<&SourceFile> = files.iter().filter(|f| is_markdown(&f.path)).collect();
    md.sort_by(|a, b| a.path.cmp(&b.path));
    for f in &md {
        let (dir, file) = split_dir(&f.path);
        let text = texts.get(&f.path).cloned().unwrap_or_default();
        let (fm, body) = frontmatter(&text);
        if fm_bool(&fm, "publish") == Some(false) || fm_bool(&fm, "draft") == Some(true) {
            continue;
        }
        let st = stem(file);
        let lower = st.to_lowercase();
        let folder_name = split_dir(dir).1.to_lowercase();
        let rank = match lower.as_str() {
            "_index" => Some(0),
            "index" => Some(1),
            "readme" => Some(2),
            "home" if dir.is_empty() => Some(3),
            _ if !dir.is_empty() && lower == folder_name => Some(3),
            _ => None,
        };
        let title = fm_str(&fm, "title")
            .or_else(|| first_heading(&body).map(|h| h.2))
            .unwrap_or_else(|| st.to_string());
        let i = notes.len();
        notes.push(Note {
            path: f.path.clone(),
            dir: dir.to_string(),
            out: String::new(),
            url: String::new(),
            fm,
            body,
            title,
            is_section: false,
        });
        if let Some(rank) = rank {
            let better = section_note
                .get(dir)
                .map(|&j| {
                    let lj = stem(split_dir(&notes[j].path).1).to_lowercase();
                    let rj = match lj.as_str() {
                        "_index" => 0,
                        "index" => 1,
                        "readme" => 2,
                        _ => 3,
                    };
                    rank < rj
                })
                .unwrap_or(true);
            if better {
                section_note.insert(dir.to_string(), i);
            }
        }
    }

    // Every folder with a note under it is a section.
    let mut sections: BTreeMap<String, Option<usize>> = BTreeMap::new();
    sections.insert(String::new(), None);
    for n in &notes {
        let parts: Vec<&str> = n.dir.split('/').filter(|p| !p.is_empty()).collect();
        for i in 1..=parts.len() {
            sections.insert(parts[..i].join("/"), None);
        }
    }
    for (d, i) in &section_note {
        sections.insert(d.clone(), Some(*i));
    }

    // Where each note goes.
    let mut page_names: HashMap<String, HashSet<String>> = HashMap::new();
    for (i, n) in notes.iter_mut().enumerate() {
        if section_note.get(&n.dir) == Some(&i) {
            n.is_section = true;
            let s = &dirs[&n.dir];
            n.out = if s.is_empty() {
                "content/_index.md".into()
            } else {
                format!("content/{s}/_index.md")
            };
            n.url = url_of_dir(&n.dir);
        } else {
            let dslug = dirs[&n.dir].clone();
            let taken = page_names.entry(dslug.clone()).or_insert_with(|| {
                // A page may not take a sub-section's name.
                dirs.iter()
                    .filter(|(k, _)| split_dir(k).0 == n.dir && !k.is_empty())
                    .map(|(_, v)| split_dir(v).1.to_string())
                    .collect()
            });
            let s = unique(taken, slug(stem(split_dir(&n.path).1)));
            n.out = if dslug.is_empty() {
                format!("content/{s}.md")
            } else {
                format!("content/{dslug}/{s}.md")
            };
            n.url = if dslug.is_empty() {
                format!("/{s}/")
            } else {
                format!("/{dslug}/{s}/")
            };
        }
    }

    // Files beside the notes.
    let mut index = Index {
        by_path: HashMap::new(),
        by_name: HashMap::new(),
        by_title: HashMap::new(),
    };
    let mut asset_names: HashMap<String, HashSet<String>> = HashMap::new();
    for f in files.iter().filter(|f| !is_markdown(&f.path)) {
        let (dir, file) = split_dir(&f.path);
        let dslug = dirs[dir].clone();
        let ext = file
            .rsplit_once('.')
            .map(|(_, e)| format!(".{}", e.to_ascii_lowercase()))
            .unwrap_or_default();
        let name = unique(
            asset_names.entry(dslug.clone()).or_default(),
            format!("{}{ext}", slug(stem(file))),
        );
        let rel = if dslug.is_empty() {
            name
        } else {
            format!("{dslug}/{name}")
        };
        let t = Target {
            url: format!("/{rel}"),
            source: f.path.clone(),
        };
        index.by_path.insert(f.path.to_lowercase(), t.clone());
        index
            .by_name
            .entry(file.to_lowercase())
            .or_default()
            .push(t);
        out.assets.insert(format!("static/{rel}"), (*f).clone());
    }
    for n in &notes {
        let t = Target {
            url: n.url.clone(),
            source: n.path.clone(),
        };
        let no_ext = n.path.strip_suffix(".md").unwrap_or(&n.path);
        index.by_path.insert(no_ext.to_lowercase(), t.clone());
        index
            .by_name
            .entry(stem(split_dir(&n.path).1).to_lowercase())
            .or_default()
            .push(t.clone());
        index
            .by_title
            .entry(n.title.to_lowercase())
            .or_insert_with(|| t.clone());
        for a in fm_list(&n.fm, "aliases") {
            index
                .by_title
                .entry(a.to_lowercase())
                .or_insert_with(|| t.clone());
        }
    }
    // A folder's name links to its section.
    for (d, _) in sections.iter().filter(|(d, _)| !d.is_empty()) {
        let t = Target {
            url: url_of_dir(d),
            source: format!("{d}/"),
        };
        index
            .by_path
            .entry(d.to_lowercase())
            .or_insert_with(|| t.clone());
        index
            .by_name
            .entry(split_dir(d).1.to_lowercase())
            .or_insert_with(|| vec![t]);
    }

    let rw = Rewriter {
        index: &index,
        wiki: Regex::new(r"(!?)\[\[([^\[\]\n]+?)\]\]").expect("a regex"),
        md_link: Regex::new(r#"(!?)\[([^\[\]\n]*)\]\((?:<([^>\n]+)>|([^)\s]+))(\s+"[^"\n]*")?\)"#)
            .expect("a regex"),
        callout: Regex::new(r"^(\s*(?:>\s*)+)\[!([A-Za-z-]+)\]([+-]?)(.*)$").expect("a regex"),
    };

    for n in &notes {
        let mut body = n.body.clone();
        if let Some((a, b, h)) = first_heading(&body)
            && h.eq_ignore_ascii_case(&n.title)
        {
            body.replace_range(a..b, "");
        }
        let body = rw.body(&body, &n.dir, &n.path, &mut out.unresolved);
        let root = n.is_section && n.dir.is_empty();
        let (title, desc) = if root {
            let d = if settings.description.is_empty() {
                fm_str(&n.fm, "description").unwrap_or_default()
            } else {
                settings.description.clone()
            };
            (settings.title.clone(), Some(d))
        } else {
            (n.title.clone(), None)
        };
        let mut fm = n.fm.clone();
        if root {
            fm.remove("description");
        }
        let text = format!(
            "{}\n{}",
            toml_front(&title, &fm, desc.as_deref()),
            body.trim_start_matches('\n')
        );
        out.content.insert(n.out.clone(), text);
    }
    // Sections without a note of their own.
    for (d, note) in &sections {
        if note.is_some() {
            continue;
        }
        let path = if d.is_empty() {
            "content/_index.md".to_string()
        } else {
            format!("content/{}/_index.md", dirs[d])
        };
        let (title, desc) = if d.is_empty() {
            (settings.title.clone(), Some(settings.description.as_str()))
        } else {
            (split_dir(d).1.to_string(), None)
        };
        out.content.insert(
            path,
            toml_front(&title, &serde_yaml_ng::Mapping::new(), desc),
        );
    }
    out.pages = out.content.len();
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source::Content;

    fn file(path: &str) -> SourceFile {
        SourceFile {
            path: path.into(),
            content: Content::Blob {
                hash: "0".repeat(64),
                size: 1,
            },
        }
    }

    fn wiki() -> (Vec<SourceFile>, HashMap<String, String>) {
        let texts: HashMap<String, String> = [
            (
                "Home.md",
                "---\ntitle: Welcome\ndescription: Notes on music\n---\nStart at [[Scales]] or [[Harmony/Chords|chords]].\n",
            ),
            (
                "Basics/Scales.md",
                "---\ntags: [basics, theory]\naliases:\n  - Scale\nweight: 2\n---\n# Scales\n\nSee [[Circle of Fifths#Key Signatures]] and [[Nowhere]].\n\n![[circle.png]]\n\n```\n[[not a link]]\n```\n\nInline `[[code]]` too. [Chords](../Harmony/Chords.md)\n",
            ),
            (
                "Basics/Circle of Fifths.md",
                "# Circle of Fifths\n\n> [!tip] Remember\n> Sharps go clockwise.\n\n## Key Signatures\n\nBack to [[Scale]].\n",
            ),
            ("Basics/Secret.md", "---\npublish: false\n---\nhidden\n"),
            ("Harmony/Harmony.md", "Chords and how they move.\n"),
            ("Harmony/Chords.md", "Built from [[Scales|scales]]. ![[circle.png|300]]\n"),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
        let mut files: Vec<SourceFile> = texts.keys().map(|k| file(k)).collect();
        files.push(file("Basics/circle.png"));
        files.push(file(".obsidian/app.json.md"));
        files.retain(|f| !f.path.starts_with('.'));
        (files, texts)
    }

    #[test]
    fn an_obsidian_wiki_becomes_dodeca_content() {
        let (files, texts) = wiki();
        let c = convert(
            &files,
            &texts,
            &Settings {
                title: "Music Theory".into(),
                description: String::new(),
            },
        );
        let keys: Vec<&str> = c.content.keys().map(String::as_str).collect();
        assert_eq!(
            keys,
            [
                "content/_index.md",
                "content/basics/_index.md",
                "content/basics/circle-of-fifths.md",
                "content/basics/scales.md",
                "content/harmony/_index.md",
                "content/harmony/chords.md",
            ]
        );
        assert_eq!(c.pages, 6);
        let root = &c.content["content/_index.md"];
        assert!(
            root.contains("title = \"Music Theory\"")
                && root.contains("description = \"Notes on music\""),
            "{root}"
        );
        assert!(root.contains("[Scales](/basics/scales/)"), "{root}");
        assert!(root.contains("[chords](/harmony/chords/)"), "{root}");

        let scales = &c.content["content/basics/scales.md"];
        assert!(scales.contains("title = \"Scales\""), "{scales}");
        assert!(scales.contains("weight = 2"), "{scales}");
        assert!(scales.contains("[extra]"), "{scales}");
        assert!(
            scales.contains("tags = [\"basics\", \"theory\"]"),
            "{scales}"
        );
        assert!(
            !scales.contains("# Scales"),
            "the leading H1 is the title: {scales}"
        );
        assert!(
            scales.contains(
                "[Circle of Fifths > Key Signatures](/basics/circle-of-fifths/#key-signatures)"
            ),
            "{scales}"
        );
        assert!(
            scales.contains("<span class=\"missing-link\">Nowhere</span>"),
            "{scales}"
        );
        assert!(scales.contains("![circle](/basics/circle.png)"), "{scales}");
        assert!(scales.contains("```\n[[not a link]]\n```"), "{scales}");
        assert!(scales.contains("`[[code]]`"), "{scales}");
        assert!(scales.contains("[Chords](/harmony/chords/)"), "{scales}");

        let circle = &c.content["content/basics/circle-of-fifths.md"];
        assert!(circle.contains("title = \"Circle of Fifths\""), "{circle}");
        assert!(circle.contains("> **Remember**"), "{circle}");
        assert!(
            circle.contains("[Scale](/basics/scales/)"),
            "an alias resolves: {circle}"
        );

        let harmony = &c.content["content/harmony/_index.md"];
        assert!(
            harmony.contains("title = \"Harmony\"") && harmony.contains("Chords and how"),
            "{harmony}"
        );
        let chords = &c.content["content/harmony/chords.md"];
        assert!(
            chords.contains("![circle](/basics/circle.png)"),
            "a size is not a caption: {chords}"
        );

        assert_eq!(c.unresolved, ["[[Nowhere]] in Basics/Scales.md"]);
        assert!(c.assets.contains_key("static/basics/circle.png"));
        assert!(!c.content.keys().any(|k| k.contains("secret")));
    }

    #[test]
    fn slugs_and_paths() {
        assert_eq!(slug("Circle of Fifths"), "circle-of-fifths");
        assert_eq!(slug("Don't Stop (live)"), "dont-stop-live");
        assert_eq!(slug("!!!"), "page");
        assert_eq!(normalize("a/b/../c/./d.md"), "a/c/d.md");
        assert_eq!(percent_decode("Other%20Note.md"), "Other Note.md");
    }
}

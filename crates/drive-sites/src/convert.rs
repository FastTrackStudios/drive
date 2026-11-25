//! A folder of Markdown, Obsidian's included, as dodeca content.
//!
//! - Every folder with pages under it becomes a section (`_index.md`); a
//!   folder note (`index.md`, `README.md`, or the note named like its
//!   folder) is that section's page.
//! - Names become URL slugs (`Circle of Fifths.md` → `/circle-of-fifths/`);
//!   titles stay as they were.
//! - YAML frontmatter (`---`) becomes TOML (`+++`): `title` and `weight`
//!   (or `order`) at the top, everything else (`tags`, `aliases`, `date`,
//!   `description`, …) under `[extra]` (which always has `description`
//!   and `tags`, empty if the note has none). No title: the note's first `# Heading`,
//!   or its file name. `publish: false` or `draft: true` leaves a note out.
//! - `[[Page]]`, `[[Page|label]]`, `[[Page#Heading]]` become links to the
//!   page's route; `![[image.png]]` an image; relative Markdown links to
//!   notes and files (`[x](Other%20Note.md)`) are pointed at their routes.
//!   A link that names nothing is kept as text, and reported.
//! - Callouts (`> [!note] Title`) become a quote with a bold title.
//! - Everything else beside the notes (images, PDFs) is copied into
//!   `static/` under the same slugged folders.

use std::collections::{BTreeMap, HashMap, HashSet};

use drive_graph::markdown::{
    self, Frontmatter, WikiLink, first_heading, fm_bool, fm_list, fm_str, frontmatter, is_markdown,
    normalize, relative_dest, split_dir, stem,
};
use drive_graph::resolve::{Entry, Resolver};

use crate::source::SourceFile;

pub use drive_graph::markdown::slug;

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

/// Where a page or file ends up (its URL), and the note or file it is.
type Target = Entry<String>;

fn target(url: String, source: &str) -> Target {
    Entry {
        value: url,
        source: source.to_string(),
    }
}

/// One note, its frontmatter read.
struct Note {
    path: String,
    dir: String,
    /// `content/…`
    out: String,
    url: String,
    fm: Frontmatter,
    body: String,
    title: String,
    is_section: bool,
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
fn toml_front(title: &str, fm: &Frontmatter, root_description: Option<&str>) -> String {
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
    // Templates read these on every page; gingembre refuses a missing
    // field, so they are always there.
    extra
        .entry("description")
        .or_insert_with(|| toml::Value::String(String::new()));
    extra
        .entry("tags")
        .or_insert_with(|| toml::Value::Array(Vec::new()));
    top.insert("extra".into(), toml::Value::Table(extra));
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

fn escape_label(s: &str) -> String {
    s.replace('[', "\\[").replace(']', "\\]")
}

fn escape_html(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

struct Rewriter<'a> {
    index: &'a Resolver<String>,
}

impl Rewriter<'_> {
    /// One line of a note's body, outside code.
    fn line(&self, line: &str, dir: &str, note: &str, missing: &mut Vec<String>) -> String {
        let line = markdown::CALLOUT
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
        markdown::map_outside_code_spans(&line, |part| self.text(part, dir, note, missing))
    }

    fn text(&self, s: &str, dir: &str, note: &str, missing: &mut Vec<String>) -> String {
        let s = markdown::WIKILINK.replace_all(s, |c: &regex::Captures| {
            self.wikilink(WikiLink::from_captures(c), dir, note, missing)
        });
        markdown::MD_LINK
            .replace_all(&s, |c: &regex::Captures| self.md_link(c, dir))
            .into_owned()
    }

    fn wikilink(&self, w: WikiLink, dir: &str, note: &str, missing: &mut Vec<String>) -> String {
        let shown = w.shown();
        // `[[#Heading]]`: a heading on this page.
        if w.page.is_empty() {
            return format!(
                "[{}](#{})",
                escape_label(&shown),
                slug(w.anchor.unwrap_or_default())
            );
        }
        let Some(t) = self.index.find(w.page, dir) else {
            missing.push(format!("[[{}]] in {note}", w.page));
            return format!(
                "<span class=\"missing-link\">{}</span>",
                escape_html(&shown)
            );
        };
        let url = with_anchor(&t.value, w.anchor);
        if w.embed && w.is_image() {
            // `![[x.png|300]]`: a size, not a caption.
            format!("![{}]({url})", escape_label(w.image_alt()))
        } else {
            format!("[{}]({url})", escape_label(&shown))
        }
    }

    fn md_link(&self, c: &regex::Captures, dir: &str) -> String {
        let whole = c[0].to_string();
        let Some((path, anchor)) = relative_dest(markdown::md_dest(c)) else {
            return whole;
        };
        let joined = normalize(&format!("{dir}/{path}"));
        match self.index.find(&joined, dir) {
            Some(t) => format!(
                "{}[{}]({}{})",
                &c[1],
                &c[2],
                with_anchor(&t.value, anchor.as_deref()),
                c.get(5).map_or("", |m| m.as_str())
            ),
            None => whole,
        }
    }

    fn body(&self, body: &str, dir: &str, note: &str, missing: &mut Vec<String>) -> String {
        markdown::map_prose_lines(body, |line| self.line(line, dir, note, missing))
    }
}

/// A URL with `#anchor` (slugged) when the link names one.
fn with_anchor(url: &str, anchor: Option<&str>) -> String {
    match anchor {
        Some(a) if !a.is_empty() => format!("{url}#{}", slug(a)),
        _ => url.to_string(),
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
    // A source supplies relative paths with a nonempty name at each level.
    let files: Vec<_> = files
        .iter()
        .filter(|file| file.path.split('/').all(|part| !part.is_empty()))
        .collect();

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
    let mut md: Vec<&SourceFile> = files
        .iter()
        .copied()
        .filter(|f| is_markdown(&f.path))
        .collect();
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
    let mut index = Resolver::new();
    let mut asset_names: HashMap<String, HashSet<String>> = HashMap::new();
    for f in files.iter().copied().filter(|f| !is_markdown(&f.path)) {
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
        let t = target(format!("/{rel}"), &f.path);
        index.path(&f.path, t.clone());
        index.name(file, t);
        out.assets.insert(format!("static/{rel}"), (*f).clone());
    }
    for n in &notes {
        let t = target(n.url.clone(), &n.path);
        let path = n.path.to_lowercase();
        index.path(path.strip_suffix(".md").unwrap_or(&path), t.clone());
        index.name(stem(split_dir(&n.path).1), t.clone());
        index.title(&n.title, t.clone());
        for a in fm_list(&n.fm, "aliases") {
            index.title(&a, t.clone());
        }
    }
    // A folder's name links to its section.
    for (d, _) in sections.iter().filter(|(d, _)| !d.is_empty()) {
        let t = target(url_of_dir(d), &format!("{d}/"));
        index.path_if_absent(d, t.clone());
        index.name_if_absent(split_dir(d).1, t);
    }

    let rw = Rewriter { index: &index };

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
        out.content
            .insert(path, toml_front(&title, &Frontmatter::new(), desc));
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
            ("Harmony/Chords.MD", "Built from [[Scales|scales]]. ![[circle.png|300]]\n"),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
        let mut files: Vec<SourceFile> = texts.keys().map(|k| file(k)).collect();
        files.push(file("Basics/circle.png"));
        files.push(file("/ignored/page.md"));
        files.push(file("//ignored/image.png"));
        files.push(file("Basics//ignored.png"));
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
        assert_eq!(
            drive_graph::markdown::percent_decode("Other%20Note.md"),
            "Other Note.md"
        );
    }
}

//! Reading Markdown, Obsidian's included: frontmatter, headings,
//! `[[wikilinks]]`, `![[embeds]]` and relative Markdown links, with code
//! (fenced blocks and inline spans) left alone.
//!
//! Every function here is pure: text in, values out. The site generator
//! (`drive-sites`) rewrites links with them; the extractor
//! ([`crate::extract`]) records them.

use std::sync::LazyLock;

use regex::{Captures, Regex};

/// A note's YAML frontmatter, read.
pub type Frontmatter = serde_yaml_ng::Mapping;

/// Whether a path names a Markdown file (`.md`, any case).
#[must_use]
pub fn is_markdown(path: &str) -> bool {
    path.to_ascii_lowercase().ends_with(".md")
}

/// `a/b/c.md` → (`a/b`, `c.md`); a bare name has the empty folder.
#[must_use]
pub fn split_dir(path: &str) -> (&str, &str) {
    match path.rsplit_once('/') {
        Some((d, f)) => (d, f),
        None => ("", path),
    }
}

/// A file name without its extension (`.hidden` keeps its dot).
#[must_use]
pub fn stem(file: &str) -> &str {
    match file.rsplit_once('.') {
        Some((s, _)) if !s.is_empty() => s,
        _ => file,
    }
}

/// A URL slug: lowercase letters and digits, words joined by dashes.
#[must_use]
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

/// `%20` and friends decoded (invalid escapes stay as written).
#[must_use]
pub fn percent_decode(s: &str) -> String {
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

/// `a/b/../c` → `a/c` (and no leading, trailing or doubled slashes).
#[must_use]
pub fn normalize(path: &str) -> String {
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

// ── frontmatter ─────────────────────────────────────────────────────────

/// A note's frontmatter (empty when it has none, or it is not a YAML
/// mapping) and its body after it. A byte-order mark is dropped.
#[must_use]
pub fn frontmatter(text: &str) -> (Frontmatter, String) {
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
                return (yaml_mapping(yaml), body);
            }
            offset += line.len();
        }
    }
    (Frontmatter::new(), t.to_string())
}

fn yaml_mapping(yaml: &str) -> Frontmatter {
    serde_yaml_ng::from_str::<serde_yaml_ng::Value>(yaml)
        .ok()
        .and_then(|v| match v {
            serde_yaml_ng::Value::Mapping(m) => Some(m),
            _ => None,
        })
        .unwrap_or_default()
}

/// A text (or number) field.
#[must_use]
pub fn fm_str(fm: &Frontmatter, k: &str) -> Option<String> {
    match fm.get(k)? {
        serde_yaml_ng::Value::String(s) => Some(s.clone()),
        serde_yaml_ng::Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

/// A yes/no field.
#[must_use]
pub fn fm_bool(fm: &Frontmatter, k: &str) -> Option<bool> {
    match fm.get(k)? {
        serde_yaml_ng::Value::Bool(b) => Some(*b),
        _ => None,
    }
}

/// A list of texts; a single text is a list of one.
#[must_use]
pub fn fm_list(fm: &Frontmatter, k: &str) -> Vec<String> {
    match fm.get(k) {
        Some(serde_yaml_ng::Value::Sequence(s)) => s
            .iter()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect(),
        Some(serde_yaml_ng::Value::String(s)) => vec![s.clone()],
        _ => Vec::new(),
    }
}

// ── headings ────────────────────────────────────────────────────────────

/// The first `# Heading`, if the note starts with one: its byte range in
/// `body` (the whole line) and its text.
#[must_use]
pub fn first_heading(body: &str) -> Option<(usize, usize, String)> {
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

/// An ATX heading line (`## Key Signatures`): its level and text.
#[must_use]
pub fn heading(line: &str) -> Option<(u8, String)> {
    let t = line.trim_start();
    let hashes = t.bytes().take_while(|b| *b == b'#').count();
    if hashes == 0 || hashes > 6 {
        return None;
    }
    let rest = &t[hashes..];
    if !(rest.is_empty() || rest.starts_with(' ') || rest.starts_with('\t')) {
        return None;
    }
    let text = rest.trim().trim_end_matches('#').trim_end();
    u8::try_from(hashes).ok().map(|l| (l, text.to_string()))
}

// ── code is left alone ──────────────────────────────────────────────────

/// Each line of `body` outside fenced code (```` ``` ```` or `~~~`) passed
/// through `f` (without its newline); fence lines and code inside them are
/// kept as they are.
pub fn map_prose_lines(body: &str, mut f: impl FnMut(&str) -> String) -> String {
    let mut out = String::with_capacity(body.len());
    let mut fence: Option<&str> = None;
    for line in body.split_inclusive('\n') {
        match (fence, fence_marker(line)) {
            (None, Some(m)) => {
                fence = Some(m);
                out.push_str(line);
            }
            (Some(open), Some(m)) if open == m => {
                fence = None;
                out.push_str(line);
            }
            (Some(_), _) => out.push_str(line),
            (None, None) => {
                let (text, nl) = match line.strip_suffix('\n') {
                    Some(t) => (t, "\n"),
                    None => (line, ""),
                };
                out.push_str(&f(text));
                out.push_str(nl);
            }
        }
    }
    out
}

fn fence_marker(line: &str) -> Option<&'static str> {
    let t = line.trim_start();
    if t.starts_with("```") {
        Some("```")
    } else if t.starts_with("~~~") {
        Some("~~~")
    } else {
        None
    }
}

/// A line with the text outside its inline code spans (between backticks)
/// passed through `f`; the spans are kept as they are.
pub fn map_outside_code_spans(line: &str, mut f: impl FnMut(&str) -> String) -> String {
    let mut out = String::new();
    for (i, part) in line.split('`').enumerate() {
        if i > 0 {
            out.push('`');
        }
        if i % 2 == 1 {
            out.push_str(part);
        } else {
            out.push_str(&f(part));
        }
    }
    out
}

/// Every piece of prose in `body` (outside fenced code and inline code
/// spans), in order, for readers that only look.
pub fn for_each_prose(body: &str, mut f: impl FnMut(&str)) {
    map_prose_lines(body, |line| {
        map_outside_code_spans(line, |part| {
            f(part);
            String::new()
        });
        String::new()
    });
}

/// The lines of `body` outside fenced code.
pub fn for_each_prose_line(body: &str, mut f: impl FnMut(&str)) {
    map_prose_lines(body, |line| {
        f(line);
        String::new()
    });
}

// ── links ───────────────────────────────────────────────────────────────

/// `[[…]]` and `![[…]]`: group 1 is `!` for an embed, group 2 what is inside.
pub static WIKILINK: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(!?)\[\[([^\[\]\n]+?)\]\]").expect("a regex"));

/// `[label](dest "title")` and `![alt](<dest>)`: group 1 `!`, 2 the
/// label, 3 an angle-bracketed destination, 4 a bare one, 5 the title.
pub static MD_LINK: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(!?)\[([^\[\]\n]*)\]\((?:<([^>\n]+)>|([^)\s]+))(\s+"[^"\n]*")?\)"#)
        .expect("a regex")
});

/// `> [!tip]- Title`: group 1 the quote markers, 2 the kind, 3 the fold
/// sign, 4 the title.
pub static CALLOUT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(\s*(?:>\s*)+)\[!([A-Za-z-]+)\]([+-]?)(.*)$").expect("a regex"));

/// One `[[…]]`, read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WikiLink<'a> {
    /// `![[…]]`.
    pub embed: bool,
    /// What it names (`Scales`, `folder/Page`, `circle.png`); empty for
    /// `[[#Heading]]`, a heading on the same page.
    pub page: &'a str,
    /// `#Heading` or `#^block` (without the `#` and `^`).
    pub anchor: Option<&'a str>,
    /// `|label` (or, on an image embed, a size such as `300`).
    pub label: Option<&'a str>,
}

impl<'a> WikiLink<'a> {
    /// A `[[…]]` from its inside (`Page#Heading|label`).
    #[must_use]
    pub fn parse(embed: bool, inner: &'a str) -> WikiLink<'a> {
        let (target, label) = match inner.split_once('|') {
            Some((t, l)) => (t.trim(), Some(l.trim())),
            None => (inner.trim(), None),
        };
        let (page, anchor) = match target.split_once('#') {
            Some((p, a)) => (p.trim(), Some(a.trim().trim_start_matches('^'))),
            None => (target, None),
        };
        WikiLink {
            embed,
            page,
            anchor,
            label,
        }
    }

    /// From a match of [`WIKILINK`].
    #[must_use]
    pub fn from_captures(c: &'a Captures<'a>) -> WikiLink<'a> {
        let embed = c.get(1).is_some_and(|m| !m.as_str().is_empty());
        WikiLink::parse(embed, c.get(2).map_or("", |m| m.as_str()))
    }

    /// What a reader sees: the label, else `Page > Heading`, else the
    /// page's last name.
    #[must_use]
    pub fn shown(&self) -> String {
        self.label
            .map(str::to_string)
            .unwrap_or_else(|| match self.anchor {
                Some(a) if self.page.is_empty() => a.to_string(),
                Some(a) => format!("{} > {a}", self.page),
                None => self
                    .page
                    .rsplit('/')
                    .next()
                    .unwrap_or(self.page)
                    .to_string(),
            })
    }

    /// Whether the page is an image file (by extension).
    #[must_use]
    pub fn is_image(&self) -> bool {
        matches!(
            self.page
                .rsplit('.')
                .next()
                .map(str::to_ascii_lowercase)
                .as_deref(),
            Some("png" | "jpg" | "jpeg" | "gif" | "webp" | "svg" | "avif" | "jxl")
        )
    }

    /// An image embed's alt text: its label unless that is a size
    /// (`300`, `300x200`), else the file's stem.
    #[must_use]
    pub fn image_alt(&self) -> &'a str {
        self.label
            .filter(|l| !l.chars().all(|c| c.is_ascii_digit() || c == 'x'))
            .unwrap_or_else(|| stem(self.page.rsplit('/').next().unwrap_or(self.page)))
    }
}

/// A Markdown link's destination, when it points at another file of the
/// same tree (not a URL, `mailto:`, `data:`, an absolute path or an
/// anchor on the page): its decoded path, as written (relative to the
/// note's folder), and its anchor.
#[must_use]
pub fn relative_dest(dest: &str) -> Option<(String, Option<String>)> {
    if dest.is_empty()
        || dest.starts_with('/')
        || dest.starts_with('#')
        || dest.contains("://")
        || dest.starts_with("mailto:")
        || dest.starts_with("data:")
    {
        return None;
    }
    let decoded = percent_decode(dest);
    Some(match decoded.split_once('#') {
        Some((p, a)) => (p.to_string(), Some(a.to_string())),
        None => (decoded.clone(), None),
    })
}

/// A match of [`MD_LINK`]'s destination.
#[must_use]
pub fn md_dest<'a>(c: &'a Captures<'a>) -> &'a str {
    c.get(3).or_else(|| c.get(4)).map_or("", |m| m.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugs_and_paths() {
        assert_eq!(slug("Circle of Fifths"), "circle-of-fifths");
        assert_eq!(slug("Don't Stop (live)"), "dont-stop-live");
        assert_eq!(slug("!!!"), "page");
        assert_eq!(normalize("a/b/../c/./d.md"), "a/c/d.md");
        assert_eq!(percent_decode("Other%20Note.md"), "Other Note.md");
        assert_eq!(percent_decode("bad%zz"), "bad%zz");
        assert_eq!(split_dir("a/b/c.md"), ("a/b", "c.md"));
        assert_eq!(stem(".hidden"), ".hidden");
        assert_eq!(stem("x.tar.gz"), "x.tar");
    }

    #[test]
    fn wikilinks_are_read() {
        let w = WikiLink::parse(false, "Circle of Fifths#Key Signatures");
        assert_eq!(w.page, "Circle of Fifths");
        assert_eq!(w.anchor, Some("Key Signatures"));
        assert_eq!(w.shown(), "Circle of Fifths > Key Signatures");
        let w = WikiLink::parse(true, " img.png | 300 ");
        assert!(w.embed && w.is_image());
        assert_eq!(w.image_alt(), "img");
        let w = WikiLink::parse(false, "#^block");
        assert_eq!((w.page, w.anchor), ("", Some("block")));
        assert_eq!(WikiLink::parse(false, "a/b/Page").shown(), "Page");
    }

    #[test]
    fn relative_destinations() {
        assert_eq!(
            relative_dest("../A%20B.md#Top"),
            Some(("../A B.md".into(), Some("Top".into())))
        );
        for d in [
            "",
            "/abs.md",
            "#top",
            "https://x/y.md",
            "mailto:a@b",
            "data:x",
        ] {
            assert_eq!(relative_dest(d), None, "{d}");
        }
    }

    #[test]
    fn frontmatter_and_headings() {
        let (fm, body) = frontmatter("\u{feff}---\ntitle: T\ntags: x\n---\n# H\n");
        assert_eq!(fm_str(&fm, "title").as_deref(), Some("T"));
        assert_eq!(fm_list(&fm, "tags"), ["x"]);
        assert_eq!(body, "# H\n");
        let (fm, body) = frontmatter("---\nnot: [closed\n---\nbody");
        assert!(fm.is_empty());
        assert_eq!(body, "body");
        assert_eq!(
            first_heading("\n# Title\nx").map(|h| h.2).as_deref(),
            Some("Title")
        );
        assert_eq!(heading("## Key ##"), Some((2, "Key".into())));
        assert_eq!(heading("#tag"), None);
        assert_eq!(heading("####### seven"), None);
    }

    #[test]
    fn code_is_left_alone() {
        let body = "a `[[x]]` b\n```\n[[y]]\n```\nc\n";
        let mut seen = Vec::new();
        for_each_prose(body, |p| seen.push(p.to_string()));
        assert_eq!(seen, ["a ", " b", "c"]);
        let up = map_prose_lines(body, |l| map_outside_code_spans(l, |p| p.to_uppercase()));
        assert_eq!(up, "A `[[x]]` B\n```\n[[y]]\n```\nC\n");
    }
}

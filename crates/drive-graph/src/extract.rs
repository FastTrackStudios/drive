//! What one Markdown file says about the graph: its links, embeds,
//! aliases, headings and frontmatter tags, read from its text alone.
//!
//! An [`Extract`] depends only on the file's bytes, never on where the file
//! is, so it is kept by content hash ([`Sidecar`]) and a file is never
//! parsed twice: a move, a rename or a copy reuses it. Links are kept as
//! written; [`crate::space`] works out what they name, which does depend on
//! where the file is and what else is in the space.

use serde::{Deserialize, Serialize};

use crate::markdown::{
    self, Frontmatter, WikiLink, first_heading, fm_list, fm_str, frontmatter, relative_dest,
};

/// Changes whenever [`extract`] would give a different result for the same
/// text: a sidecar made by another version is made again.
pub const VERSION: u32 = 1;

/// How a link was written.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Form {
    /// `[[Page]]`, `[[Page|label]]`, `[[Page#Heading]]`.
    Wiki,
    /// `![[image.png]]`, `![[Other note]]`: shown inside the note.
    Embed,
    /// `[label](Other%20Note.md)`, relative to the note's folder.
    Markdown,
    /// `![alt](image.png)`, relative to the note's folder.
    Image,
    /// In the frontmatter: `related: [[Chords]]`, `up: "[[Home]]"`, or a
    /// plain name under `related:`.
    Frontmatter,
}

impl Form {
    /// Shown inside the note rather than linked from it.
    #[must_use]
    pub fn is_embed(self) -> bool {
        matches!(self, Form::Embed | Form::Image)
    }

    /// Its target is a path relative to the note's folder (not a name).
    #[must_use]
    pub fn is_relative(self) -> bool {
        matches!(self, Form::Markdown | Form::Image)
    }
}

/// One outgoing link, as written.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Link {
    pub form: Form,
    /// What it names: a page name or path for wikilinks (`Scales`,
    /// `folder/Page`), the decoded relative path for Markdown links
    /// (`../Harmony/Chords.md`).
    pub target: String,
    /// `#Heading` or `#^block`, without the marks.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub anchor: Option<String>,
    /// The text shown (`|label`, a Markdown link's label).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// The link exactly as written (`[[Scales|scales]]`, `related: Chords`).
    pub raw: String,
}

/// A heading of the note.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Heading {
    pub level: u8,
    pub text: String,
}

/// Everything the graph needs from one Markdown file.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Extract {
    /// The frontmatter's `title`, else the first `# Heading`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    pub links: Vec<Link>,
    /// `aliases:` (or `alias:`): other names the note is linked by.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub aliases: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub headings: Vec<Heading>,
    /// `tags:` (or `tag:`), without a leading `#`, as written.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
}

impl Extract {
    /// The links that are embeds.
    pub fn embeds(&self) -> impl Iterator<Item = &Link> {
        self.links.iter().filter(|l| l.form.is_embed())
    }
}

/// Read a Markdown file's text.
#[must_use]
pub fn extract(text: &str) -> Extract {
    let (fm, body) = frontmatter(text);
    let mut links = frontmatter_links(&fm);
    markdown::for_each_prose(&body, |part| body_links(part, &mut links));
    let mut headings = Vec::new();
    markdown::for_each_prose_line(&body, |line| {
        if let Some((level, text)) = markdown::heading(line) {
            headings.push(Heading { level, text });
        }
    });
    Extract {
        title: fm_str(&fm, "title").or_else(|| first_heading(&body).map(|h| h.2)),
        links,
        aliases: either_list(&fm, "aliases", "alias"),
        headings,
        tags: frontmatter_tags(&fm),
    }
}

/// `key:` as a list, else `other:`.
fn either_list(fm: &Frontmatter, key: &str, other: &str) -> Vec<String> {
    let l = fm_list(fm, key);
    if l.is_empty() { fm_list(fm, other) } else { l }
}

/// `tags: [a, b]`, `tags: a, b`, `tags: "#a #b"`: each without its `#`.
fn frontmatter_tags(fm: &Frontmatter) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for t in either_list(fm, "tags", "tag") {
        for word in t.split([',', ' ']) {
            let w = word.trim().trim_start_matches('#');
            if !w.is_empty() && !out.iter().any(|o| o == w) {
                out.push(w.to_string());
            }
        }
    }
    out
}

/// Links in the frontmatter: any `[[…]]` in any text value, and plain
/// names under `related:`.
fn frontmatter_links(fm: &Frontmatter) -> Vec<Link> {
    let mut out = Vec::new();
    for (k, v) in fm {
        let Some(key) = k.as_str() else { continue };
        if matches!(key, "aliases" | "alias" | "tags" | "tag") {
            continue;
        }
        for text in yaml_texts(v) {
            let wikis: Vec<Link> = markdown::WIKILINK
                .captures_iter(text)
                .filter_map(|c| {
                    wiki_link(&WikiLink::from_captures(&c), &c[0]).map(|l| Link {
                        form: Form::Frontmatter,
                        raw: format!("{key}: {}", l.raw),
                        ..l
                    })
                })
                .collect();
            if wikis.is_empty() && key == "related" && !text.trim().is_empty() {
                out.push(Link {
                    form: Form::Frontmatter,
                    target: text.trim().to_string(),
                    anchor: None,
                    label: None,
                    raw: format!("{key}: {}", text.trim()),
                });
            }
            out.extend(wikis);
        }
    }
    out
}

/// Every text inside a YAML value (lists and maps searched).
fn yaml_texts(v: &serde_yaml_ng::Value) -> Vec<&str> {
    use serde_yaml_ng::Value as Y;
    match v {
        Y::String(s) => vec![s.as_str()],
        Y::Sequence(s) => s.iter().flat_map(yaml_texts).collect(),
        Y::Mapping(m) => m.values().flat_map(yaml_texts).collect(),
        Y::Tagged(t) => yaml_texts(&t.value),
        _ => Vec::new(),
    }
}

/// Wikilinks, then Markdown links, of one piece of prose (outside code).
fn body_links(part: &str, out: &mut Vec<Link>) {
    for c in markdown::WIKILINK.captures_iter(part) {
        if let Some(l) = wiki_link(&WikiLink::from_captures(&c), &c[0]) {
            out.push(l);
        }
    }
    for c in markdown::MD_LINK.captures_iter(part) {
        let Some((path, anchor)) = relative_dest(markdown::md_dest(&c)) else {
            continue;
        };
        let image = c.get(1).is_some_and(|m| !m.as_str().is_empty());
        out.push(Link {
            form: if image { Form::Image } else { Form::Markdown },
            target: path,
            anchor: anchor.filter(|a| !a.is_empty()),
            label: Some(c[2].to_string()).filter(|l| !l.is_empty()),
            raw: c[0].to_string(),
        });
    }
}

/// A `[[…]]` as a link; `[[#Heading]]` (this page) is none.
fn wiki_link(w: &WikiLink, raw: &str) -> Option<Link> {
    if w.page.is_empty() {
        return None;
    }
    let label = if w.embed && w.is_image() {
        // `![[x.png|300]]`: a size, not a caption.
        w.label
            .filter(|l| !l.chars().all(|c| c.is_ascii_digit() || c == 'x'))
    } else {
        w.label
    };
    Some(Link {
        form: if w.embed { Form::Embed } else { Form::Wiki },
        target: w.page.to_string(),
        anchor: w.anchor.filter(|a| !a.is_empty()).map(str::to_string),
        label: label.filter(|l| !l.is_empty()).map(str::to_string),
        raw: raw.to_string(),
    })
}

// ── sidecars: an extract kept by content ────────────────────────────────

/// An [`Extract`] as kept and shared: by the file's content key (its full
/// BLAKE3 hash) and the extractor's version. Any peer can make it from the
/// bytes; one received from another peer is a cache, used only when its
/// key and version are the ones asked for, and never trusted over the
/// bytes when this peer has them.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Sidecar {
    pub key: String,
    pub version: u32,
    pub extract: Extract,
}

/// Why a sidecar was not used.
#[derive(Debug, PartialEq, Eq)]
pub enum SidecarError {
    /// Not a sidecar at all.
    Malformed(String),
    /// Made from other content than asked for.
    OtherContent { asked: String, got: String },
    /// Made by another version of the extractor.
    OtherVersion(u32),
}

impl std::fmt::Display for SidecarError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SidecarError::Malformed(e) => write!(f, "not a links sidecar: {e}"),
            SidecarError::OtherContent { asked, got } => {
                write!(f, "a sidecar of {got}, not {asked}")
            }
            SidecarError::OtherVersion(v) => {
                write!(f, "made by extractor version {v}, not {VERSION}")
            }
        }
    }
}

impl std::error::Error for SidecarError {}

impl Sidecar {
    /// The current extractor's sidecar for content `key`.
    #[must_use]
    pub fn new(key: &str, extract: Extract) -> Sidecar {
        Sidecar {
            key: key.to_string(),
            version: VERSION,
            extract,
        }
    }

    /// Its bytes (JSON).
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        serde_json::to_vec(self).expect("an extract is always JSON")
    }

    /// A sidecar's extract, if it is the current extractor's for `key`.
    ///
    /// # Errors
    ///
    /// Malformed bytes, or a sidecar of other content or another version.
    pub fn decode(bytes: &[u8], key: &str) -> Result<Extract, SidecarError> {
        let s: Sidecar =
            serde_json::from_slice(bytes).map_err(|e| SidecarError::Malformed(e.to_string()))?;
        if s.key != key {
            return Err(SidecarError::OtherContent {
                asked: key.to_string(),
                got: s.key,
            });
        }
        if s.version != VERSION {
            return Err(SidecarError::OtherVersion(s.version));
        }
        Ok(s.extract)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn targets(e: &Extract) -> Vec<(Form, &str)> {
        e.links
            .iter()
            .map(|l| (l.form, l.target.as_str()))
            .collect()
    }

    #[test]
    fn a_note_is_read() {
        let e = extract(
            "---\ntitle: Scales\naliases: [Scale, Modes]\ntags: [theory, \"#basics\"]\nrelated:\n  - \"[[Chords]]\"\n  - Circle of Fifths\nup: \"[[Home]]\"\n---\n# Scales\n\nSee [[Circle of Fifths#Key Signatures|the circle]] and [[#Local]].\n\n![[circle.png|300]] ![[Sheet]]\n\n```\n[[not a link]]\n```\n`[[code]]` [Chords](../Harmony/Chords%20Two.md#Top) ![p](img.png) [w](https://x.y/a.md)\n\n## Key ##\n",
        );
        assert_eq!(e.title.as_deref(), Some("Scales"));
        assert_eq!(e.aliases, ["Scale", "Modes"]);
        assert_eq!(e.tags, ["theory", "basics"]);
        assert_eq!(
            targets(&e),
            [
                (Form::Frontmatter, "Chords"),
                (Form::Frontmatter, "Circle of Fifths"),
                (Form::Frontmatter, "Home"),
                (Form::Wiki, "Circle of Fifths"),
                (Form::Embed, "circle.png"),
                (Form::Embed, "Sheet"),
                (Form::Markdown, "../Harmony/Chords Two.md"),
                (Form::Image, "img.png"),
            ]
        );
        let circle = &e.links[3];
        assert_eq!(circle.anchor.as_deref(), Some("Key Signatures"));
        assert_eq!(circle.label.as_deref(), Some("the circle"));
        assert_eq!(circle.raw, "[[Circle of Fifths#Key Signatures|the circle]]");
        assert_eq!(e.links[4].label, None, "a size is not a label");
        assert_eq!(e.links[6].anchor.as_deref(), Some("Top"));
        assert_eq!(e.links[0].raw, "related: [[Chords]]");
        assert_eq!(e.embeds().count(), 3);
        assert_eq!(
            e.headings,
            [
                Heading {
                    level: 1,
                    text: "Scales".into()
                },
                Heading {
                    level: 2,
                    text: "Key".into()
                }
            ]
        );
    }

    #[test]
    fn tags_written_as_text() {
        let e = extract("---\ntags: \"a, #b c\"\ntag: ignored\n---\n");
        assert_eq!(e.tags, ["a", "b", "c"]);
        assert_eq!(extract("---\ntag: one\n---\n").tags, ["one"]);
        assert_eq!(extract("no frontmatter, # not a heading\n").title, None);
    }

    #[test]
    fn a_sidecar_is_used_only_for_its_content_and_version() {
        let e = extract("[[A]]");
        let bytes = Sidecar::new("h1", e.clone()).encode();
        assert_eq!(Sidecar::decode(&bytes, "h1"), Ok(e.clone()));
        assert!(matches!(
            Sidecar::decode(&bytes, "h2"),
            Err(SidecarError::OtherContent { .. })
        ));
        let old = Sidecar {
            version: VERSION + 1,
            ..Sidecar::new("h1", e)
        };
        assert_eq!(
            Sidecar::decode(&old.encode(), "h1"),
            Err(SidecarError::OtherVersion(VERSION + 1))
        );
        assert!(matches!(
            Sidecar::decode(b"nope", "h1"),
            Err(SidecarError::Malformed(_))
        ));
    }
}

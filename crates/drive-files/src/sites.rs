//! Static sites published from the space's files.
//!
//! A site is one small Loro document, `<space>/drive:site:<id>`: its title,
//! the folder its pages come from, the theme folder and an override folder
//! (both folders of the file tree), where it is published, and how its
//! last build went. The space's registry lists the sites (`sites`: id →
//! title) so an app can show them without opening each.
//!
//! What a build made is a second document, `<space>/drive:siteout:<id>`:
//! every output path and the blob holding its bytes (as the node serves
//! them, under `/sites/<space>/<id>/`), and what each public storage was
//! last sent, so a rebuild only moves what changed.
//!
//! The generator (`drive-sites`, native) reads and writes these; the node
//! serves the output; the Drive app lists sites, makes them and asks for
//! builds (`requested`).

use std::collections::BTreeMap;

use loro::{LoroDoc, LoroMap, LoroValue, ValueOrContainer};

/// A site's document id.
pub fn site_doc(space: &str, id: &str) -> String {
    format!("{space}/drive:site:{id}")
}

/// The document holding what a site's last build made.
pub fn output_doc(space: &str, id: &str) -> String {
    format!("{space}/drive:siteout:{id}")
}

/// Where the node serves a site.
pub fn node_path(space: &str, id: &str) -> String {
    format!("/sites/{space}/{id}/")
}

/// A site, as it is set up.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Site {
    /// Short, URL-safe (`music-theory`).
    pub id: String,
    pub title: String,
    pub description: String,
    /// The folder of Markdown pages (a path in the file tree).
    pub source: String,
    /// The base theme folder (templates/, sass/, static/).
    pub theme: String,
    /// This site's own templates and styles, laid over the theme (empty:
    /// none).
    pub overrides: String,
    /// The storage it is published to (its id; it must be public:
    /// `space::set_public`). Empty: the node only (tailnet).
    pub storage: String,
    /// Its address where it is published. Empty: the storage's public URL
    /// plus `/sites/<id>/`.
    pub base_url: String,
    /// The device whose agent builds it (empty: the first agent to see
    /// it claims it).
    pub builder: String,
    /// Rebuild by itself when its pages or theme change.
    pub auto: bool,
    /// When someone last asked for a build (ms); a build later than this
    /// answers it.
    pub requested: i64,
    pub created: i64,
}

/// How a build went.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Build {
    /// When it finished (ms).
    pub at: i64,
    pub ok: bool,
    /// What went wrong, or a short summary.
    pub message: String,
    pub pages: i64,
    /// Files in the output.
    pub files: i64,
    /// Broken links the link check found (`/target (from /page)`).
    pub broken_links: Vec<String>,
    /// `[[wikilinks]]` that named no page (`Page (in folder/note.md)`).
    pub unresolved: Vec<String>,
    /// The hash of everything the build read; the same inputs are not
    /// built again.
    pub input_hash: String,
    pub duration_ms: i64,
    /// Where it went: the node's path and the public URL, if any.
    pub published: Vec<String>,
    /// Which device built it.
    pub by: String,
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

fn b(m: &LoroMap, k: &str) -> bool {
    matches!(
        m.get(k),
        Some(ValueOrContainer::Value(LoroValue::Bool(true)))
    )
}

fn list(m: &LoroMap, k: &str) -> Vec<String> {
    let text = s(m, k);
    if text.is_empty() {
        return Vec::new();
    }
    text.split('\n').map(str::to_string).collect()
}

/// The site a document describes (`None`: not set up).
pub fn read(doc: &LoroDoc) -> Option<Site> {
    let m = doc.get_map("site");
    let id = s(&m, "id");
    if id.is_empty() {
        return None;
    }
    Some(Site {
        id,
        title: s(&m, "title"),
        description: s(&m, "description"),
        source: s(&m, "source"),
        theme: s(&m, "theme"),
        overrides: s(&m, "overrides"),
        storage: s(&m, "storage"),
        base_url: s(&m, "base_url"),
        builder: s(&m, "builder"),
        auto: b(&m, "auto"),
        requested: n(&m, "requested"),
        created: n(&m, "created"),
    })
}

/// Set a site up, or change it.
pub fn write(doc: &LoroDoc, site: &Site) -> loro::LoroResult<()> {
    let m = doc.get_map("site");
    for (k, v) in [
        ("id", &site.id),
        ("title", &site.title),
        ("description", &site.description),
        ("source", &site.source),
        ("theme", &site.theme),
        ("overrides", &site.overrides),
        ("storage", &site.storage),
        ("base_url", &site.base_url),
        ("builder", &site.builder),
    ] {
        if s(&m, k) != *v {
            m.insert(k, v.as_str())?;
        }
    }
    if b(&m, "auto") != site.auto || m.get("auto").is_none() {
        m.insert("auto", site.auto)?;
    }
    if n(&m, "requested") != site.requested {
        m.insert("requested", site.requested)?;
    }
    if n(&m, "created") != site.created {
        m.insert("created", site.created)?;
    }
    Ok(())
}

/// Ask for a build now (the builder's agent answers).
pub fn request_build(doc: &LoroDoc) -> loro::LoroResult<()> {
    doc.get_map("site").insert("requested", crate::now_ms())
}

/// The last build, if there was one.
pub fn last_build(doc: &LoroDoc) -> Option<Build> {
    let m = doc.get_map("build");
    let at = n(&m, "at");
    if at == 0 {
        return None;
    }
    Some(Build {
        at,
        ok: b(&m, "ok"),
        message: s(&m, "message"),
        pages: n(&m, "pages"),
        files: n(&m, "files"),
        broken_links: list(&m, "broken_links"),
        unresolved: list(&m, "unresolved"),
        input_hash: s(&m, "input_hash"),
        duration_ms: n(&m, "duration_ms"),
        published: list(&m, "published"),
        by: s(&m, "by"),
    })
}

pub fn set_build(doc: &LoroDoc, r: &Build) -> loro::LoroResult<()> {
    let m = doc.get_map("build");
    m.insert("at", r.at)?;
    m.insert("ok", r.ok)?;
    m.insert("message", r.message.as_str())?;
    m.insert("pages", r.pages)?;
    m.insert("files", r.files)?;
    m.insert("broken_links", r.broken_links.join("\n"))?;
    m.insert("unresolved", r.unresolved.join("\n"))?;
    m.insert("input_hash", r.input_hash.as_str())?;
    m.insert("duration_ms", r.duration_ms)?;
    m.insert("published", r.published.join("\n"))?;
    m.insert("by", r.by.as_str())?;
    Ok(())
}

/// The sites a space has (from its registry): id → title.
pub fn listed(registry: &LoroDoc) -> BTreeMap<String, String> {
    let m = registry.get_map("sites");
    m.keys()
        .map(|k| {
            let k = k.to_string();
            let title = s(&m, &k);
            (k, title)
        })
        .collect()
}

/// List a site in the registry (`None`: take it off).
pub fn set_listed(registry: &LoroDoc, id: &str, title: Option<&str>) -> loro::LoroResult<()> {
    let m = registry.get_map("sites");
    match title {
        Some(t) => m.insert(id, t),
        None => m.delete(id),
    }
}

/// A site id from a title: lowercase letters, digits and dashes.
pub fn id_from(title: &str) -> String {
    let mut out = String::new();
    for c in title.chars().flat_map(char::to_lowercase) {
        if c.is_ascii_alphanumeric() {
            out.push(c);
        } else if !out.ends_with('-') && !out.is_empty() {
            out.push('-');
        }
    }
    out.trim_end_matches('-').to_string()
}

/// What the node serves for a site: output path → blob hash.
pub fn output(doc: &LoroDoc) -> BTreeMap<String, String> {
    map_of(&doc.get_map("files"))
}

/// What a public storage was last sent: path → blob hash.
pub fn published(doc: &LoroDoc, storage: &str) -> BTreeMap<String, String> {
    match doc.get_map("published").get(storage) {
        Some(ValueOrContainer::Container(loro::Container::Map(m))) => map_of(&m),
        _ => BTreeMap::new(),
    }
}

fn map_of(m: &LoroMap) -> BTreeMap<String, String> {
    m.keys()
        .map(|k| {
            let k = k.to_string();
            let v = s(m, &k);
            (k, v)
        })
        .collect()
}

fn set_map(m: &LoroMap, files: &BTreeMap<String, String>) -> loro::LoroResult<()> {
    let old = map_of(m);
    for k in old.keys().filter(|k| !files.contains_key(*k)) {
        m.delete(k)?;
    }
    for (k, v) in files {
        if old.get(k) != Some(v) {
            m.insert(k, v.as_str())?;
        }
    }
    Ok(())
}

/// Replace what the node serves (only the paths that changed are written).
pub fn set_output(doc: &LoroDoc, files: &BTreeMap<String, String>) -> loro::LoroResult<()> {
    set_map(&doc.get_map("files"), files)
}

pub fn set_published(
    doc: &LoroDoc,
    storage: &str,
    files: &BTreeMap<String, String>,
) -> loro::LoroResult<()> {
    let m = doc.get_map("published").ensure_mergeable_map(storage)?;
    set_map(&m, files)
}

/// A MIME type for a site's file (by its extension).
pub fn mime(path: &str) -> &'static str {
    let ext = path
        .rsplit('/')
        .next()
        .and_then(|n| n.rsplit_once('.'))
        .map(|(_, e)| e.to_ascii_lowercase());
    match ext.as_deref() {
        Some("html" | "htm") => "text/html; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("js" | "mjs") => "text/javascript; charset=utf-8",
        Some("json") => "application/json",
        Some("wasm") => "application/wasm",
        Some("svg") => "image/svg+xml",
        Some("png") => "image/png",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        Some("jxl") => "image/jxl",
        Some("avif") => "image/avif",
        Some("ico") => "image/x-icon",
        Some("woff2") => "font/woff2",
        Some("woff") => "font/woff",
        Some("ttf") => "font/ttf",
        Some("txt") => "text/plain; charset=utf-8",
        Some("xml") => "application/xml",
        Some("pdf") => "application/pdf",
        Some("mp3") => "audio/mpeg",
        Some("wav") => "audio/wav",
        Some("mp4") => "video/mp4",
        _ => "application/octet-stream",
    }
}

/// The output file a request path names: the file itself, or the folder's
/// `index.html`.
pub fn resolve<'a>(files: &'a BTreeMap<String, String>, path: &str) -> Option<(String, &'a str)> {
    let p = path.trim_matches('/');
    let candidates = if p.is_empty() {
        vec!["index.html".to_string()]
    } else {
        vec![
            p.to_string(),
            format!("{p}/index.html"),
            format!("{p}.html"),
        ]
    };
    candidates
        .into_iter()
        .find_map(|c| files.get(&c).map(|h| (c, h.as_str())))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_site_round_trips_and_lists() {
        let doc = LoroDoc::new();
        assert!(read(&doc).is_none());
        let site = Site {
            id: id_from("Music Theory!"),
            title: "Music Theory".into(),
            source: "Wikis/Music Theory".into(),
            theme: "Sites/Themes/Base".into(),
            auto: true,
            created: 5,
            ..Site::default()
        };
        assert_eq!(site.id, "music-theory");
        write(&doc, &site).unwrap();
        assert_eq!(read(&doc).unwrap(), site);
        request_build(&doc).unwrap();
        assert!(read(&doc).unwrap().requested > 0);

        let r = Build {
            at: 9,
            ok: true,
            pages: 4,
            broken_links: vec!["/x (from /)".into(), "/y (from /a)".into()],
            ..Build::default()
        };
        set_build(&doc, &r).unwrap();
        assert_eq!(last_build(&doc).unwrap(), r);

        let reg = LoroDoc::new();
        set_listed(&reg, "music-theory", Some("Music Theory")).unwrap();
        assert_eq!(listed(&reg).len(), 1);
        set_listed(&reg, "music-theory", None).unwrap();
        assert!(listed(&reg).is_empty());
    }

    #[test]
    fn output_maps_and_resolution() {
        let doc = LoroDoc::new();
        let mut files = BTreeMap::new();
        files.insert("index.html".to_string(), "a".to_string());
        files.insert("theory/index.html".to_string(), "b".to_string());
        files.insert("main.css".to_string(), "c".to_string());
        set_output(&doc, &files).unwrap();
        files.remove("main.css");
        set_output(&doc, &files).unwrap();
        let out = output(&doc);
        assert_eq!(out.len(), 2);
        assert_eq!(resolve(&out, "/").unwrap().1, "a");
        assert_eq!(resolve(&out, "theory").unwrap().1, "b");
        assert_eq!(resolve(&out, "theory/").unwrap().0, "theory/index.html");
        assert!(resolve(&out, "nope").is_none());
        set_published(&doc, "r2", &files).unwrap();
        assert_eq!(published(&doc, "r2"), files);
        assert_eq!(mime("a/b.wasm"), "application/wasm");
    }
}

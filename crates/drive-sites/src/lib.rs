//! Drive's static sites.
//!
//! A site (`drive_files::sites::Site`) names a folder of Markdown in the
//! space's file tree, a theme folder and an override folder. A build:
//!
//! 1. reads the three folders ([`source`]; a Loro page source can be added
//!    beside the tree's later), and hashes what it read ([`input_hash`]):
//!    the same inputs are not built twice;
//! 2. converts the pages to dodeca content ([`convert`]: Obsidian
//!    frontmatter and `[[wikilinks]]` included) and lays the theme, then the
//!    overrides, beside them in a dodeca project on this computer
//!    ([`project`]: only files that changed are written, so dodeca's own
//!    cache stays warm);
//! 3. runs `ddc build` ([`ddc`]), which renders, indexes for search, and
//!    checks links;
//! 4. places the output under the path it is served from ([`rewrite`]) and
//!    publishes it: to the node (the caller uploads blobs and records them
//!    in the site's output document) and to public storage
//!    ([`publish_storage`], only what changed).

pub mod convert;
pub mod ddc;
pub mod project;
pub mod rewrite;
pub mod source;
pub mod theme;

use std::collections::{BTreeMap, HashMap};
use std::path::Path;

use drive_files::sites::Site;

use source::{Blobs, Content, SourceFile};

/// Changes when the generator's output would change for the same inputs.
pub const GENERATOR: &str = "drive-sites/1";

/// What a site is built from.
#[derive(Clone, Debug, Default)]
pub struct Inputs {
    pub source: Vec<SourceFile>,
    pub theme: Vec<SourceFile>,
    pub overrides: Vec<SourceFile>,
}

/// Everything a build depends on, hashed: the generator, `ddc`'s version,
/// the site's settings and every input file's content hash.
pub fn input_hash(site: &Site, base_url: &str, inputs: &Inputs, ddc_version: &str) -> String {
    let mut h = blake3::Hasher::new();
    for part in [
        GENERATOR,
        ddc_version,
        &site.title,
        &site.description,
        base_url,
    ] {
        h.update(part.as_bytes());
        h.update(&[0]);
    }
    for (role, files) in [
        ("source", &inputs.source),
        ("theme", &inputs.theme),
        ("overrides", &inputs.overrides),
    ] {
        let mut sorted: Vec<(&str, String)> = files
            .iter()
            .map(|f| (f.path.as_str(), f.content.hash()))
            .collect();
        sorted.sort();
        for (p, c) in sorted {
            for part in [role, p, &c] {
                h.update(part.as_bytes());
                h.update(&[0]);
            }
        }
    }
    h.finalize().to_hex().to_string()
}

/// A finished build.
#[derive(Debug, Default)]
pub struct Built {
    /// Output path → bytes, as dodeca wrote them (root-absolute URLs).
    pub files: BTreeMap<String, Vec<u8>>,
    pub pages: i64,
    pub broken_links: Vec<String>,
    pub unresolved: Vec<String>,
    pub log: String,
}

/// The project's `.config/dodeca.styx`.
pub fn dodeca_config(base_url: &str) -> String {
    let base = if base_url.is_empty() { "/" } else { base_url };
    format!(
        "source {{\n    content content\n}}\n\nsite {{\n    output public\n    base_url {}\n    stable_assets (\n        robots.txt\n        favicon.ico\n        favicon.svg\n    )\n}}\n",
        serde_json::to_string(base).unwrap_or_else(|_| "\"/\"".into())
    )
}

/// The theme's files that belong in a dodeca project.
fn theme_part(path: &str) -> bool {
    ["templates/", "sass/", "static/", "data/"]
        .iter()
        .any(|p| path.starts_with(p))
}

/// Build a site in `work` (its dodeca project, kept between builds).
/// `base_url`: where it is published (`/` when only the node serves it).
pub async fn build<B: Blobs>(
    work: &Path,
    site: &Site,
    base_url: &str,
    inputs: &Inputs,
    blobs: &B,
    ddc_bin: &Path,
) -> Result<Built, String> {
    // The pages' text.
    let mut texts = HashMap::new();
    for f in inputs
        .source
        .iter()
        .filter(|f| f.path.to_ascii_lowercase().ends_with(".md"))
    {
        let text = match &f.content {
            Content::Text(t) => t.clone(),
            Content::Blob { hash, .. } => {
                let bytes = blobs
                    .get(hash)
                    .await
                    .map_err(|e| format!("{}: {e}", f.path))?;
                String::from_utf8_lossy(&bytes).into_owned()
            }
        };
        texts.insert(f.path.clone(), text);
    }
    let converted = convert::convert(
        &inputs.source,
        &texts,
        &convert::Settings {
            title: site.title.clone(),
            description: site.description.clone(),
        },
    );

    // The project: theme, then overrides, then the content and its files.
    let mut plan: BTreeMap<String, project::Planned> = BTreeMap::new();
    let theme = if inputs.theme.iter().any(|f| theme_part(&f.path)) {
        inputs.theme.clone()
    } else {
        theme::starter()
    };
    for f in theme.iter().chain(inputs.overrides.iter()) {
        if theme_part(&f.path) {
            plan.insert(f.path.clone(), project::Planned::File(f.clone()));
        }
    }
    for (path, f) in &converted.assets {
        plan.insert(path.clone(), project::Planned::File(f.clone()));
    }
    for (path, text) in &converted.content {
        plan.insert(path.clone(), project::Planned::Text(text.clone()));
    }
    plan.insert(
        ".config/dodeca.styx".into(),
        project::Planned::Text(dodeca_config(base_url)),
    );
    project::sync(work, &plan, blobs).await?;

    let report = ddc::build(ddc_bin, work).await;
    if !report.ok {
        let tail: Vec<&str> = report.log.lines().rev().take(25).collect();
        let tail: Vec<&str> = tail.into_iter().rev().collect();
        return Err(format!("ddc build failed:\n{}", tail.join("\n")));
    }
    let files = project::read_output(&work.join("public"))?;
    Ok(Built {
        pages: if report.pages > 0 {
            report.pages
        } else {
            converted.pages as i64
        },
        files,
        broken_links: report.broken,
        unresolved: converted.unresolved,
        log: report.log,
    })
}

/// A file's blob hash.
pub fn hash(bytes: &[u8]) -> String {
    blake3::hash(bytes).to_hex().to_string()
}

/// Put a placed output into a storage under `root` (`sites/<id>`): files
/// whose bytes changed since `before` (path → hash) are written, files gone
/// are deleted. Returns what the storage now holds (path → hash).
pub async fn publish_storage(
    op: &opendal::Operator,
    root: &str,
    files: &BTreeMap<String, Vec<u8>>,
    before: &BTreeMap<String, String>,
) -> Result<BTreeMap<String, String>, String> {
    let root = root.trim_matches('/');
    let mut now = BTreeMap::new();
    for (path, bytes) in files {
        let h = hash(bytes);
        if before.get(path) != Some(&h) {
            op.write_with(&format!("{root}/{path}"), bytes.clone())
                .content_type(drive_files::sites::mime(path))
                .await
                .map_err(|e| format!("{path}: {e}"))?;
        }
        now.insert(path.clone(), h);
    }
    for path in before.keys().filter(|p| !files.contains_key(*p)) {
        if let Err(e) = op.delete(&format!("{root}/{path}")).await {
            tracing::warn!(site.path = %path, error = %e, "an old site file could not be removed from storage");
        }
    }
    Ok(now)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_input_hash_follows_content_not_order() {
        let site = Site {
            id: "w".into(),
            title: "W".into(),
            ..Site::default()
        };
        let f = |p: &str, t: &str| SourceFile {
            path: p.into(),
            content: Content::Text(t.into()),
        };
        let a = Inputs {
            source: vec![f("a.md", "1"), f("b.md", "2")],
            ..Inputs::default()
        };
        let b = Inputs {
            source: vec![f("b.md", "2"), f("a.md", "1")],
            ..Inputs::default()
        };
        let c = Inputs {
            source: vec![f("a.md", "1"), f("b.md", "3")],
            ..Inputs::default()
        };
        assert_eq!(
            input_hash(&site, "/", &a, "ddc 1"),
            input_hash(&site, "/", &b, "ddc 1")
        );
        assert_ne!(
            input_hash(&site, "/", &a, "ddc 1"),
            input_hash(&site, "/", &c, "ddc 1")
        );
        assert_ne!(
            input_hash(&site, "/", &a, "ddc 1"),
            input_hash(&site, "/", &a, "ddc 2")
        );
    }

    #[tokio::test]
    async fn storage_publishing_moves_only_changes() {
        let op = opendal::Operator::new(opendal::services::Memory::default()).unwrap();
        let mut files = BTreeMap::new();
        files.insert("index.html".to_string(), b"a".to_vec());
        files.insert("old.html".to_string(), b"o".to_vec());
        let first = publish_storage(&op, "sites/w", &files, &BTreeMap::new())
            .await
            .unwrap();
        assert_eq!(op.read("sites/w/old.html").await.unwrap().to_vec(), b"o");
        files.remove("old.html");
        files.insert("index.html".to_string(), b"b".to_vec());
        let second = publish_storage(&op, "sites/w", &files, &first)
            .await
            .unwrap();
        assert_eq!(second.len(), 1);
        assert_eq!(op.read("sites/w/index.html").await.unwrap().to_vec(), b"b");
        assert!(op.read("sites/w/old.html").await.is_err());
    }
}

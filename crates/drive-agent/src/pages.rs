//! `drive page …`: notes kept as live Loro text (docs/graph.md, "Pages"),
//! which every device and agent edits at once and merges the same.
//!
//! ```text
//! drive page new PATH [FILE|-]     # a page at PATH in the Drive (folders made); its text from FILE or stdin
//! drive page cat PATH
//! drive page write PATH [FILE|-]   # the page's text becomes FILE's: only what changed is applied
//! drive page ls                    # every page and its path
//! ```

use std::io::Read;
use std::path::Path;

use drive_files::tree;
use drive_spine::pages;
use loro::LoroDoc;

use crate::agent::Agent;
use crate::graph::tree_items;
use crate::{out, settle, start};

/// A page found at a Drive path.
pub struct Page {
    pub path: String,
    pub doc_id: String,
    pub doc: LoroDoc,
}

/// The page at `path` in the Drive (with the other peers' copy when they
/// answer within a few seconds).
pub async fn open(a: &Agent, path: &str) -> Result<Page, String> {
    let path = path.trim_start_matches("drive:").trim_matches('/');
    let e = tree::resolve(&a.files, path)
        .flatten()
        .ok_or_else(|| format!("no {path} in the Drive"))?;
    if e.page.is_empty() {
        return Err(format!(
            "{path} is a file, not a page (its text is in a blob)"
        ));
    }
    let space = a.cfg.lock().expect("cfg").space.clone();
    let doc_id = pages::page_doc(&space, &e.page);
    let doc = a.drive.open(&doc_id).await;
    let _ = tokio::time::timeout(std::time::Duration::from_secs(4), a.drive.synced(&doc_id)).await;
    Ok(Page {
        path: path.to_string(),
        doc_id,
        doc,
    })
}

/// A page's path: in the Drive, ending in `.md` (added when it does not).
fn page_path(path: &str) -> Result<String, String> {
    let p = path.trim_start_matches("drive:").trim_matches('/');
    if p.is_empty()
        || p.split('/')
            .any(|part| part.is_empty() || part.starts_with('.'))
    {
        return Err(format!(
            "{path}: a page needs a path in the Drive (no hidden folders)"
        ));
    }
    Ok(if drive_graph::markdown::is_markdown(p) {
        p.to_string()
    } else {
        format!("{p}.md")
    })
}

/// A new page at `path` (its folders made), with `text`.
pub async fn create(a: &Agent, path: &str, text: &str) -> Result<Page, String> {
    let path = page_path(path)?;
    if tree::resolve(&a.files, &path).flatten().is_some() {
        return Err(format!("{path} is already in the Drive"));
    }
    let (dir, name) = path.rsplit_once('/').unwrap_or(("", &path));
    let parent = a.ensure_folder(dir)?;
    let space = a.cfg.lock().expect("cfg").space.clone();
    let id = pages::new_id(&a.files, drive_files::now_ms());
    let doc_id = pages::page_doc(&space, &id);
    let doc = a.drive.open(&doc_id).await;
    pages::write(&doc, text)?;
    a.drive.commit(&doc_id);
    tree::create_page(&a.files, parent, name, &id).map_err(|e| e.to_string())?;
    a.commit_files();
    Ok(Page { path, doc_id, doc })
}

/// Make the page's text `text` (only what differs is applied).
pub fn write(a: &Agent, p: &Page, text: &str) -> Result<(), String> {
    pages::write(&p.doc, text)?;
    a.drive.commit(&p.doc_id);
    Ok(())
}

/// Every page in the Drive: its path and page id.
pub fn all(files: &LoroDoc) -> Vec<(String, String)> {
    tree_items(files)
        .into_iter()
        .filter(|(_, page)| !page.is_empty())
        .map(|(i, page)| (i.path, page))
        .collect()
}

/// A text from a file, or stdin for `-`; empty when none is named.
fn input(arg: Option<&String>) -> Result<String, String> {
    match arg.map(String::as_str) {
        None => Ok(String::new()),
        Some("-") => {
            let mut s = String::new();
            std::io::stdin()
                .read_to_string(&mut s)
                .map_err(|e| e.to_string())?;
            Ok(s)
        }
        Some(f) => std::fs::read_to_string(f).map_err(|e| format!("{f}: {e}")),
    }
}

pub async fn command(dir: &Path, args: &[String]) -> Result<(), String> {
    let usage = "page new PATH [FILE|-] | cat PATH | write PATH [FILE|-] | ls";
    let sub = args.get(1).map(String::as_str).unwrap_or("");
    let path = args.get(2);
    let a = start(dir).await?;
    match (sub, path) {
        ("ls", None) => {
            for (path, id) in all(&a.files) {
                out(format!("drive:{path}  ({id})"));
            }
        }
        ("new", Some(path)) => {
            let p = create(&a, path, &input(args.get(3))?).await?;
            settle(&a).await;
            out(format!("drive:{}", p.path));
        }
        ("cat", Some(path)) => {
            let p = open(&a, path).await?;
            print!("{}", pages::text(&p.doc));
        }
        ("write", Some(path)) => {
            let p = open(&a, path).await?;
            write(&a, &p, &input(args.get(3))?)?;
            settle(&a).await;
            out(format!("drive:{}", p.path));
        }
        _ => return Err(usage.into()),
    }
    Ok(())
}

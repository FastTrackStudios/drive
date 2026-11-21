//! Where a site's files come from. Today a folder of the space's file tree
//! (Markdown and whatever sits beside it, bytes in blobs); later Loro page
//! documents as well, whose text is in the document itself. The generator
//! only sees [`SourceFile`]s, so a new source is one more implementation of
//! [`Source`].

use std::future::Future;

use drive_files::tree;
use loro::LoroDoc;

/// One file of a site's input: its path inside the source (`Theory/Scales.md`)
/// and where its bytes are.
#[derive(Clone, Debug, PartialEq)]
pub struct SourceFile {
    pub path: String,
    pub content: Content,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Content {
    /// Bytes in a blob (the file tree's files).
    Blob { hash: String, size: u64 },
    /// Text held by the source itself (a page document).
    Text(String),
}

impl Content {
    /// What identifies these bytes: the blob's hash, or the text's.
    pub fn hash(&self) -> String {
        match self {
            Content::Blob { hash, .. } => hash.clone(),
            Content::Text(t) => blake3::hash(t.as_bytes()).to_hex().to_string(),
        }
    }
}

/// A site's input files.
pub trait Source {
    fn files(&self) -> Vec<SourceFile>;
}

/// Bytes by blob hash (the agent's cache, its storages, the node).
pub trait Blobs {
    fn get(&self, hash: &str) -> impl Future<Output = Result<Vec<u8>, String>> + Send;
}

/// A folder of the file tree. Hidden files and folders (`.obsidian/`,
/// `.trash/`) and files not yet uploaded are left out.
pub struct TreeFolder<'a> {
    pub doc: &'a LoroDoc,
    pub folder: String,
}

impl<'a> TreeFolder<'a> {
    pub fn new(doc: &'a LoroDoc, folder: &str) -> TreeFolder<'a> {
        TreeFolder {
            doc,
            folder: folder.trim_matches('/').to_string(),
        }
    }

    /// Whether the folder is there.
    pub fn exists(&self) -> bool {
        self.folder.is_empty()
            || tree::resolve(self.doc, &self.folder)
                .flatten()
                .is_some_and(|e| e.dir)
    }
}

impl Source for TreeFolder<'_> {
    fn files(&self) -> Vec<SourceFile> {
        if !self.exists() {
            return Vec::new();
        }
        let prefix = if self.folder.is_empty() {
            String::new()
        } else {
            format!("{}/", self.folder)
        };
        tree::files(self.doc)
            .into_iter()
            .filter_map(|(path, e)| {
                let rel = path.strip_prefix(&prefix)?.to_string();
                if e.hash.is_empty() || rel.split('/').any(|p| p.starts_with('.')) {
                    return None;
                }
                Some(SourceFile {
                    path: rel,
                    content: Content::Blob {
                        hash: e.hash,
                        size: e.size,
                    },
                })
            })
            .collect()
    }
}

/// Files already in hand (tests; the starter theme).
pub struct InMemory(pub Vec<SourceFile>);

impl Source for InMemory {
    fn files(&self) -> Vec<SourceFile> {
        self.0.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_tree_folder_lists_its_files_relative_and_skips_hidden_ones() {
        let doc = LoroDoc::new();
        let w = tree::mkdir(&doc, None, "Wiki").unwrap();
        let t = tree::mkdir(&doc, Some(w), "Theory").unwrap();
        let o = tree::mkdir(&doc, Some(w), ".obsidian").unwrap();
        let a = tree::create_file(&doc, Some(t), "Scales.md").unwrap();
        tree::set_content(&doc, a, 3, &"a".repeat(64)).unwrap();
        let b = tree::create_file(&doc, Some(o), "app.json").unwrap();
        tree::set_content(&doc, b, 3, &"b".repeat(64)).unwrap();
        tree::create_file(&doc, Some(w), "Draft.md").unwrap(); // not uploaded
        let other = tree::create_file(&doc, None, "Elsewhere.md").unwrap();
        tree::set_content(&doc, other, 3, &"c".repeat(64)).unwrap();
        let files = TreeFolder::new(&doc, "/Wiki/").files();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].path, "Theory/Scales.md");
        assert!(TreeFolder::new(&doc, "Nope").files().is_empty());
    }
}

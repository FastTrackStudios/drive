//! The dodeca project a site is built in, kept on this computer between
//! builds: only files whose content changed are written (so `ddc`'s own
//! incremental cache, `.cache/`, stays warm), files no longer wanted are
//! removed, and `public/` is read back after a build.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::source::{Blobs, Content, SourceFile};

/// One file the project should have.
#[derive(Clone, Debug)]
pub enum Planned {
    /// Generated here (converted pages, the config).
    Text(String),
    /// A source or theme file, copied.
    File(SourceFile),
}

impl Planned {
    fn hash(&self) -> String {
        match self {
            Planned::Text(t) => crate::hash(t.as_bytes()),
            Planned::File(f) => f.content.hash(),
        }
    }
}

/// What was written last time: path → content hash.
const MANIFEST: &str = ".drive-project.json";

fn safe(path: &str) -> Option<PathBuf> {
    let p = Path::new(path);
    let ok = !path.is_empty()
        && p.components()
            .all(|c| matches!(c, std::path::Component::Normal(_)));
    ok.then(|| p.to_path_buf())
}

/// Make the project match the plan.
pub async fn sync<B: Blobs>(
    dir: &Path,
    plan: &BTreeMap<String, Planned>,
    blobs: &B,
) -> Result<(), String> {
    tokio::fs::create_dir_all(dir)
        .await
        .map_err(|e| format!("{}: {e}", dir.display()))?;
    let before: BTreeMap<String, String> = tokio::fs::read(dir.join(MANIFEST))
        .await
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default();
    let mut now = BTreeMap::new();
    for (path, item) in plan {
        let Some(rel) = safe(path) else {
            tracing::warn!(site.path = %path, "a site file with an unsafe path was left out");
            continue;
        };
        let h = item.hash();
        let target = dir.join(&rel);
        if before.get(path) != Some(&h) || !target.exists() {
            let bytes = match item {
                Planned::Text(t) => t.clone().into_bytes(),
                Planned::File(f) => match &f.content {
                    Content::Text(t) => t.clone().into_bytes(),
                    Content::Blob { hash, .. } => blobs
                        .get(hash)
                        .await
                        .map_err(|e| format!("{}: {e}", f.path))?,
                },
            };
            if let Some(parent) = target.parent() {
                tokio::fs::create_dir_all(parent)
                    .await
                    .map_err(|e| e.to_string())?;
            }
            tokio::fs::write(&target, bytes)
                .await
                .map_err(|e| format!("{}: {e}", target.display()))?;
        }
        now.insert(path.clone(), h);
    }
    for gone in before.keys().filter(|p| !plan.contains_key(*p)) {
        if let Some(rel) = safe(gone) {
            let _ = tokio::fs::remove_file(dir.join(rel)).await;
        }
    }
    tokio::fs::write(
        dir.join(MANIFEST),
        serde_json::to_vec_pretty(&now).map_err(|e| e.to_string())?,
    )
    .await
    .map_err(|e| e.to_string())?;
    Ok(())
}

/// Every file a build wrote: path (with `/`) → bytes.
pub fn read_output(dir: &Path) -> Result<BTreeMap<String, Vec<u8>>, String> {
    let mut out = BTreeMap::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let entries = std::fs::read_dir(&d).map_err(|e| format!("{}: {e}", d.display()))?;
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else if let Ok(rel) = p.strip_prefix(dir) {
                let key = rel
                    .components()
                    .map(|c| c.as_os_str().to_string_lossy().into_owned())
                    .collect::<Vec<_>>()
                    .join("/");
                out.insert(key, std::fs::read(&p).map_err(|e| e.to_string())?);
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct NoBlobs;
    impl Blobs for NoBlobs {
        async fn get(&self, hash: &str) -> Result<Vec<u8>, String> {
            Err(format!("no blob {hash}"))
        }
    }

    #[tokio::test]
    async fn only_changes_are_written_and_stale_files_go() {
        let dir = tempfile::tempdir().unwrap();
        let mut plan = BTreeMap::new();
        plan.insert("content/a.md".to_string(), Planned::Text("a".into()));
        plan.insert("content/b.md".to_string(), Planned::Text("b".into()));
        plan.insert("../escape".to_string(), Planned::Text("x".into()));
        sync(dir.path(), &plan, &NoBlobs).await.unwrap();
        assert!(dir.path().join("content/b.md").exists());
        assert!(!dir.path().parent().unwrap().join("escape").exists());
        let t0 = std::fs::metadata(dir.path().join("content/a.md"))
            .unwrap()
            .modified()
            .unwrap();
        plan.remove("content/b.md");
        sync(dir.path(), &plan, &NoBlobs).await.unwrap();
        assert!(!dir.path().join("content/b.md").exists());
        let t1 = std::fs::metadata(dir.path().join("content/a.md"))
            .unwrap()
            .modified()
            .unwrap();
        assert_eq!(t0, t1, "an unchanged file is not rewritten");
        let out = read_output(dir.path()).unwrap();
        assert!(out.contains_key("content/a.md"));
    }
}

//! Kinds: what a file or a document is, so the Drive app can show it with
//! the right icon and open it in the right app, without knowing what is
//! inside.
//!
//! Drive knows the common kinds by file extension (audio, video, images,
//! DAW projects, sample instruments, …). Apps add their own in the spine
//! document's `kinds` map: Studio registers its song sessions (by document
//! id prefix, `studio:song:`) and Meals its meal plan. A kind an app
//! registers for an extension Drive already knows wins, so an app can
//! claim the files it opens.

use loro::{LoroDoc, LoroMap, LoroValue, ValueOrContainer};

/// A kind of file or document.
#[derive(Clone, Debug, PartialEq)]
pub struct Kind {
    /// `audio`, `studio.song`.
    pub id: String,
    /// What people call it: `Audio`, `Song`.
    pub name: String,
    /// The broader kind it belongs to (`audio` for MIDI), or empty.
    pub parent: String,
    /// An icon of the Drive app's set (`Audio`, `Folder`, `Package`, …).
    pub icon: String,
    /// The app that opens it (`studio`), or empty.
    pub app: String,
    /// File extensions, lower case, without the dot.
    pub extensions: Vec<String>,
    /// For an app's documents: the id prefix (`studio:song:`).
    pub doc_prefix: String,
    /// Where it opens: a path on the node, `{id}` standing for the document
    /// id after the prefix (`/studio/song/{id}`).
    pub open: String,
    /// Keep files of this kind on the node too, so they can be had (and
    /// shared) while the computer that has them is off: charts, documents,
    /// lighting shows. Big media stays where it is.
    pub available: bool,
    /// The largest file kept available (bytes; 0: no limit).
    pub max_available: u64,
}

/// Files kept available by kind are at most this big unless the kind says.
pub const SMALL: u64 = 50 * 1024 * 1024;

impl Kind {
    fn builtin(id: &str, name: &str, parent: &str, icon: &str, exts: &str) -> Kind {
        Kind {
            id: id.into(),
            name: name.into(),
            parent: parent.into(),
            icon: icon.into(),
            app: String::new(),
            extensions: exts.split_whitespace().map(str::to_string).collect(),
            doc_prefix: String::new(),
            open: String::new(),
            available: false,
            max_available: 0,
        }
    }

    fn kept_available(mut self) -> Kind {
        self.available = true;
        self.max_available = SMALL;
        self
    }

    /// Whether a file of this kind and size is kept on the node.
    pub fn keeps_available(&self, size: u64) -> bool {
        self.available && (self.max_available == 0 || size <= self.max_available)
    }

    /// The link that opens this document in its app, if it has one.
    pub fn link(&self, doc_id: &str) -> Option<String> {
        if self.open.is_empty() {
            return None;
        }
        let rest = doc_id.strip_prefix(&self.doc_prefix).unwrap_or(doc_id);
        Some(self.open.replace("{id}", rest))
    }
}

/// The kinds Drive knows by itself.
pub fn builtins() -> Vec<Kind> {
    let k = Kind::builtin;
    vec![
        k("folder", "Folder", "", "Folder", ""),
        k(
            "audio",
            "Audio",
            "",
            "Audio",
            "wav wave mp3 flac aif aiff aifc m4a ogg opus caf w64 rf64 bwf wv ape alac",
        ),
        k("midi", "MIDI", "audio", "Audio", "mid midi"),
        k(
            "instrument",
            "Sample instrument",
            "",
            "Collection",
            "nki nkm nkb nkx nkc ncw nicnt exs sfz sf2 signalpack mse",
        ),
        k(
            "project",
            "Session",
            "",
            "Package",
            "rpp als ptx ptf logicx cpr npr song flp dawproject aaf omf bwproject",
        ),
        k(
            "preset",
            "Preset",
            "",
            "Widget",
            "fxp fxb vstpreset aupreset nksf nksfx nmsv adg adv",
        ),
        k("chart", "Chart", "", "Book", "kf musicxml mxl").kept_available(),
        k(
            "video",
            "Video",
            "",
            "Video",
            "mp4 mov m4v mkv webm avi mts",
        ),
        k(
            "image",
            "Image",
            "",
            "Image",
            "png jpg jpeg gif webp heic heif tif tiff bmp svg avif",
        ),
        k("pdf", "PDF", "document", "Document_pdf", "pdf").kept_available(),
        k(
            "document",
            "Document",
            "",
            "Document",
            "doc docx odt rtf pages",
        )
        .kept_available(),
        k(
            "sheet",
            "Spreadsheet",
            "document",
            "Document_xls",
            "xls xlsx ods csv tsv numbers",
        )
        .kept_available(),
        k(
            "text",
            "Text",
            "document",
            "Text_txt",
            "txt md markdown log",
        )
        .kept_available(),
        k(
            "code",
            "Code",
            "",
            "TextAlt",
            "rs ts tsx js jsx py json toml yaml yml styx html css sh nix c h cpp hpp swift kt go lua",
        ),
        k(
            "archive",
            "Archive",
            "",
            "Archive",
            "zip rar 7z tar gz tgz xz zst bz2 dmg iso",
        ),
        k(
            "app",
            "App",
            "",
            "Executable",
            "app exe msi pkg deb rpm appimage apk ipa",
        ),
        k("database", "Database", "", "Database", "db sqlite sqlite3"),
        k("font", "Font", "", "Scrapbook", "ttf otf woff woff2"),
        k("other", "File", "", "Undefined", ""),
        k("doc", "Document", "", "Scrapbook", ""),
    ]
}

fn kinds_map(doc: &LoroDoc) -> LoroMap {
    doc.get_map("kinds")
}

fn s(m: &LoroMap, k: &str) -> String {
    match m.get(k) {
        Some(ValueOrContainer::Value(LoroValue::String(v))) => v.to_string(),
        _ => String::new(),
    }
}

fn fields(k: &Kind) -> [(&'static str, String); 10] {
    [
        ("id", k.id.clone()),
        ("name", k.name.clone()),
        ("parent", k.parent.clone()),
        ("icon", k.icon.clone()),
        ("app", k.app.clone()),
        ("extensions", k.extensions.join(" ")),
        ("doc_prefix", k.doc_prefix.clone()),
        ("open", k.open.clone()),
        (
            "available",
            if k.available {
                "yes".into()
            } else {
                String::new()
            },
        ),
        ("max_available", k.max_available.to_string()),
    ]
}

/// Register (or update) an app's kind. Writes nothing when it is already
/// registered as given, so an app can call this every time it starts.
pub fn register(doc: &LoroDoc, kind: &Kind) -> loro::LoroResult<()> {
    if registered(doc).iter().any(|k| k == kind) {
        return Ok(());
    }
    let m = kinds_map(doc).ensure_mergeable_map(&kind.id)?;
    for (key, value) in fields(kind) {
        if s(&m, key) != value {
            m.insert(key, value)?;
        }
    }
    Ok(())
}

/// The kinds apps have registered.
pub fn registered(doc: &LoroDoc) -> Vec<Kind> {
    let map = kinds_map(doc);
    let mut out = Vec::new();
    map.for_each(|_, v| {
        if let ValueOrContainer::Container(c) = v
            && let Ok(m) = c.into_map()
        {
            out.push(Kind {
                id: s(&m, "id"),
                name: s(&m, "name"),
                parent: s(&m, "parent"),
                icon: s(&m, "icon"),
                app: s(&m, "app"),
                extensions: s(&m, "extensions")
                    .split_whitespace()
                    .map(str::to_string)
                    .collect(),
                doc_prefix: s(&m, "doc_prefix"),
                open: s(&m, "open"),
                available: s(&m, "available") == "yes",
                max_available: s(&m, "max_available").parse().unwrap_or(0),
            });
        }
    });
    out.retain(|k| !k.id.is_empty());
    out.sort_by(|a, b| a.id.cmp(&b.id));
    out
}

/// Every kind, Drive's and the apps', ready to look things up.
#[derive(Clone, Debug, PartialEq)]
pub struct Kinds {
    all: Vec<Kind>,
}

impl Kinds {
    /// Drive's kinds and those registered in `spine`.
    pub fn load(spine: &LoroDoc) -> Kinds {
        Kinds::from(registered(spine))
    }

    /// Drive's kinds and the given app kinds (which win on a shared extension).
    pub fn from(apps: Vec<Kind>) -> Kinds {
        let mut all = apps;
        for b in builtins() {
            if !all.iter().any(|k| k.id == b.id) {
                all.push(b);
            }
        }
        Kinds { all }
    }

    pub fn all(&self) -> &[Kind] {
        &self.all
    }

    pub fn get(&self, id: &str) -> &Kind {
        self.all
            .iter()
            .find(|k| k.id == id)
            .unwrap_or_else(|| self.other())
    }

    fn other(&self) -> &Kind {
        self.all
            .iter()
            .find(|k| k.id == "other")
            .expect("the built-in kinds include other")
    }

    /// A file's kind, by its name's extension (`dir`: a folder).
    pub fn of_file(&self, name: &str, dir: bool) -> &Kind {
        if dir {
            // A bundle with a known extension (`Song.logicx`) is its kind.
            if let Some(k) = self.by_extension(name).filter(|k| k.id != "other") {
                return k;
            }
            return self.get("folder");
        }
        self.by_extension(name).unwrap_or_else(|| self.other())
    }

    fn by_extension(&self, name: &str) -> Option<&Kind> {
        let (_, ext) = name.rsplit_once('.')?;
        let ext = ext.to_ascii_lowercase();
        self.all.iter().find(|k| k.extensions.contains(&ext))
    }

    /// An app document's kind, by its id's prefix (the longest that fits).
    pub fn of_doc(&self, id: &str) -> &Kind {
        self.all
            .iter()
            .filter(|k| !k.doc_prefix.is_empty() && id.starts_with(&k.doc_prefix))
            .max_by_key(|k| k.doc_prefix.len())
            .unwrap_or_else(|| self.get("doc"))
    }

    /// A kind and every kind under it.
    pub fn family(&self, id: &str) -> Vec<&Kind> {
        self.all
            .iter()
            .filter(|k| k.id == id || k.parent == id)
            .collect()
    }

    /// The extensions of a kind and every kind under it.
    pub fn extensions_of(&self, id: &str) -> Vec<String> {
        self.family(id)
            .into_iter()
            .flat_map(|k| k.extensions.iter().cloned())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn song() -> Kind {
        Kind {
            id: "studio.song".into(),
            name: "Song".into(),
            parent: "doc".into(),
            icon: "Audio".into(),
            app: "studio".into(),
            extensions: vec![],
            doc_prefix: "studio:song:".into(),
            open: "/studio/song/{id}".into(),
            available: false,
            max_available: 0,
        }
    }

    #[test]
    fn files_by_extension() {
        let k = Kinds::from(vec![]);
        assert_eq!(k.of_file("Kick In.WAV", false).id, "audio");
        assert_eq!(k.of_file("groove.mid", false).id, "midi");
        assert_eq!(k.of_file("Grand.nki", false).id, "instrument");
        assert_eq!(k.of_file("Song A.rpp", false).id, "project");
        assert_eq!(k.of_file("Song A.logicx", true).id, "project");
        assert_eq!(k.of_file("Stems", true).id, "folder");
        assert_eq!(k.of_file("README", false).id, "other");
        assert!(k.extensions_of("audio").contains(&"mid".to_string()));
        // Charts and documents are kept on the node; audio is not.
        assert!(k.of_file("Song A.kf", false).keeps_available(4_000));
        assert!(!k.of_file("Kick In.wav", false).keeps_available(4_000));
        assert!(!k.of_file("Huge.pdf", false).keeps_available(SMALL + 1));
    }

    #[test]
    fn apps_register_their_documents_once() {
        let doc = LoroDoc::new();
        register(&doc, &song()).unwrap();
        doc.commit();
        let v = doc.oplog_vv();
        register(&doc, &song()).unwrap();
        doc.commit();
        assert_eq!(doc.oplog_vv(), v, "registering again writes nothing");

        let k = Kinds::load(&doc);
        let s = k.of_doc("studio:song:amazing-grace");
        assert_eq!(s.id, "studio.song");
        assert_eq!(
            s.link("studio:song:amazing-grace").as_deref(),
            Some("/studio/song/amazing-grace")
        );
        assert_eq!(k.of_doc("meals:plan").id, "doc");
    }

    #[test]
    fn an_app_can_claim_an_extension() {
        let doc = LoroDoc::new();
        let chart = Kind {
            id: "studio.chart".into(),
            name: "Chart".into(),
            parent: "chart".into(),
            icon: "Book".into(),
            app: "studio".into(),
            extensions: vec!["kf".into()],
            doc_prefix: String::new(),
            open: String::new(),
            available: true,
            max_available: 0,
        };
        register(&doc, &chart).unwrap();
        assert_eq!(
            Kinds::load(&doc).of_file("Amazing Grace.kf", false).id,
            "studio.chart"
        );
    }
}

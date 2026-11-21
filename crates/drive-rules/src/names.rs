//! Names: a free name beside taken ones (copies never overwrite), files a
//! rule must leave alone (temporary, lock and partial files), bundles
//! (folders the operating system shows as one file), and templates.

/// A name and its extension (`Song.logicx` → `("Song", "logicx")`; a
/// leading dot is part of the name: `.env` has none).
pub fn split_ext(name: &str) -> (&str, &str) {
    match name.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() && !ext.is_empty() && !ext.contains(' ') => {
            (stem, ext)
        }
        _ => (name, ""),
    }
}

/// `name` if it is free, else `name (2)`, `name (3)`, … before the
/// extension (`take (2).wav`). Never returns a taken name.
pub fn free_name(name: &str, taken: impl Fn(&str) -> bool) -> String {
    if !taken(name) {
        return name.to_string();
    }
    let (stem, ext) = split_ext(name);
    (2u32..)
        .map(|n| {
            if ext.is_empty() {
                format!("{stem} ({n})")
            } else {
                format!("{stem} ({n}).{ext}")
            }
        })
        .find(|c| !taken(c))
        .expect("some number is free")
}

/// A name made safe to use as one path piece: no separators, no control
/// characters, not `.` or `..`, not empty.
pub fn clean_piece(s: &str) -> String {
    let t: String = s
        .chars()
        .map(|c| match c {
            '/' | '\\' | ':' => '-',
            c if c.is_control() => ' ',
            c => c,
        })
        .collect();
    let t = t.trim().to_string();
    match t.as_str() {
        "" | "." | ".." => "_".into(),
        _ => t,
    }
}

/// A folder path from a template's output: pieces cleaned, empty ones and
/// `..` dropped (a rule cannot climb out of its destination).
pub fn clean_path(s: &str) -> String {
    s.split('/')
        .map(str::trim)
        .filter(|p| !p.is_empty() && *p != "." && *p != "..")
        .map(clean_piece)
        .collect::<Vec<_>>()
        .join("/")
}

/// Files that are still being written or belong to a program's working
/// state: a rule never takes them.
pub fn is_temporary(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    const ENDS: &[&str] = &[
        ".part",
        ".partial",
        ".crdownload",
        ".download",
        ".opdownload",
        ".tmp",
        ".temp",
        ".swp",
        ".swx",
        ".lock",
        ".lck",
        ".!qb",
        ".icloud",
        "~",
    ];
    const STARTS: &[&str] = &["~$", ".~lock.", "._", ".#", "#"];
    const NAMES: &[&str] = &[".ds_store", "thumbs.db", "desktop.ini", ".localized"];
    NAMES.contains(&lower.as_str())
        || ENDS.iter().any(|e| lower.ends_with(e))
        || STARTS.iter().any(|s| lower.starts_with(s))
        || lower == ".drive-trash"
}

/// Folders that are one thing (an app, a Logic project, a GarageBand
/// song, a Photos library, a plug-in): a rule takes them whole and never
/// looks inside.
pub fn is_bundle(name: &str) -> bool {
    const EXTS: &[&str] = &[
        "app",
        "logicx",
        "band",
        "photoslibrary",
        "musiclibrary",
        "imovielibrary",
        "fcpbundle",
        "tvlibrary",
        "rpp-bak",
        "bundle",
        "framework",
        "plugin",
        "component",
        "vst",
        "vst3",
        "aaxplugin",
        "clap",
        "pages",
        "numbers",
        "key",
        "rtfd",
        "lrdata",
        "lrlibrary",
        "dtBase2",
        "sparsebundle",
        "xcodeproj",
        "nicnt",
    ];
    let (_, ext) = split_ext(name);
    !ext.is_empty() && EXTS.iter().any(|e| e.eq_ignore_ascii_case(ext))
}

/// The bundle a path is inside, if any: the path of the outermost bundle
/// folder above it (`Songs/A.logicx/Alternatives/000` → `Songs/A.logicx`).
pub fn bundle_of(path: &str) -> Option<&str> {
    let mut end = 0;
    for (i, piece) in path.split('/').enumerate() {
        end += piece.len() + usize::from(i > 0);
        if is_bundle(piece) {
            return Some(&path[..end]);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn free_names_never_take_a_taken_one() {
        let taken = ["take.wav", "take (2).wav", "Notes"];
        let t = |n: &str| taken.contains(&n);
        assert_eq!(free_name("new.wav", t), "new.wav");
        assert_eq!(free_name("take.wav", t), "take (3).wav");
        assert_eq!(free_name("Notes", t), "Notes (2)");
        assert_eq!(
            free_name("Song.logicx", |n| n == "Song.logicx"),
            "Song (2).logicx"
        );
    }

    #[test]
    fn temporary_files_are_left_alone() {
        for n in [
            "mix.wav.part",
            "a.crdownload",
            "~$report.docx",
            ".~lock.sheet.ods#",
            ".DS_Store",
            "._mix.wav",
            "file.tmp",
            "x.swp",
        ] {
            assert!(is_temporary(n), "{n}");
        }
        for n in ["mix.wav", "partial mix.wav", "Templates"] {
            assert!(!is_temporary(n), "{n}");
        }
    }

    #[test]
    fn bundles_are_one_item() {
        assert!(is_bundle("Song.logicx"));
        assert!(is_bundle("Photos Library.photoslibrary"));
        assert!(is_bundle("Kick.RPP-bak"));
        assert!(!is_bundle("Song.wav"));
        assert_eq!(
            bundle_of("Songs/A.logicx/Alternatives/000/x"),
            Some("Songs/A.logicx")
        );
        assert_eq!(bundle_of("A.app"), Some("A.app"));
        assert_eq!(bundle_of("Songs/a.wav"), None);
    }

    #[test]
    fn paths_cannot_climb_out() {
        assert_eq!(clean_path("../../etc/{x}"), "etc/{x}");
        assert_eq!(clean_path("Sorted//2026/./11"), "Sorted/2026/11");
        assert_eq!(clean_piece("a/b"), "a-b");
        assert_eq!(clean_piece(".."), "_");
    }
}

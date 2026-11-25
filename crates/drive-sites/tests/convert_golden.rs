//! The converter's whole output for a wiki that exercises every link form,
//! byte for byte. Written before the link parsing moved to `drive-graph`,
//! so the move is proven not to change a single page.
//!
//! `UPDATE_GOLDEN=1 cargo test -p drive-sites --test convert_golden` rewrites
//! the file (only when the output is meant to change).

use std::collections::HashMap;
use std::fmt::Write as _;

use drive_sites::convert::{Settings, convert};
use drive_sites::source::{Content, SourceFile};

fn file(path: &str) -> SourceFile {
    SourceFile {
        path: path.into(),
        content: Content::Blob {
            hash: "0".repeat(64),
            size: 1,
        },
    }
}

const NOTES: &[(&str, &str)] = &[
    (
        "Home.md",
        "---\ntitle: Welcome\ndescription: Notes on music\norder: 1\n---\nStart at [[Scales]] or [[Harmony/Chords|chords]].\n[[#Top]] and [[Basics]] and [[Harmony]].\n",
    ),
    (
        "Basics/Scales.md",
        "---\ntags: [basics, theory]\naliases:\n  - Scale\n  - Modes\nweight: 2\nrelated: \"[[Chords]]\"\n---\n# Scales\n\nSee [[Circle of Fifths#Key Signatures]] and [[Nowhere]] and [[Nowhere|gone]].\n\n![[circle.png]] ![[circle.png|alt text]] ![[circle.png|300x200]] ![[Sheet.pdf]]\n\n```\n[[not a link]]\n```\n\n~~~rust\n[x](Chords.md)\n~~~\n\nInline `[[code]]` too. [Chords](../Harmony/Chords.md) [Top](#top) [Web](https://example.com/a.md)\n[Spaced](<../Harmony/Chords.md#Voice Leading>) [Enc](Circle%20of%20Fifths.md \"title\") ![pic](circle.png)\n[Mail](mailto:a@b.c) [Data](data:x) [Abs](/abs.md) [None](Missing.md)\n",
    ),
    (
        "Basics/Circle of Fifths.md",
        "# Circle of Fifths\n\n> [!tip] Remember\n> Sharps go clockwise.\n\n> [!warning]\n> > [!note]- Folded\n\n## Key Signatures\n\nBack to [[Scale]] or [[modes|the modes]] or [[Welcome]].\n[[Circle of Fifths#^block-id]] [[ Scales | spaced ]] [[Scales#]]\n",
    ),
    ("Basics/Secret.md", "---\npublish: false\n---\nhidden\n"),
    ("Basics/Draft.md", "---\ndraft: true\n---\nhidden\n"),
    ("Basics/Basics.md", "The basics, Don't skip them.\n"),
    ("Harmony/Harmony.md", "Chords and how they move.\n"),
    (
        "Harmony/Chords.MD",
        "Built from [[Scales|scales]]. ![[circle.png|300]] [[Basics/Scales]] [[basics/scales.md]]\n",
    ),
    (
        "Harmony/index.md",
        "---\ntitle: Harmony index\n---\nThe index wins.\n",
    ),
    (
        "Harmony/Scales.md",
        "A second Scales, nearer [[Scales]] links here.\n",
    ),
    (
        "Deep/Er/Note Ü.md",
        "Unicode [[Note Ü]] and [up](../../Home.md).\n",
    ),
    ("Deep/README.md", "Deep section, [[Er]].\n"),
    ("bom.md", "\u{feff}---\ntitle: BOM\n---\nbody\n"),
    (
        "crlf.md",
        "---\r\ntitle: CRLF\r\n---\r\nline [[Scales]]\r\n",
    ),
    (
        "broken-fm.md",
        "---\ntitle: [unclosed\n---\nStill [[Scales]].\n",
    ),
    ("no-close.md", "---\ntitle: x\nno end\n"),
];

fn render() -> String {
    let texts: HashMap<String, String> = NOTES
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    let mut paths: Vec<&str> = NOTES.iter().map(|(k, _)| *k).collect();
    paths.extend(["Basics/circle.png", "Basics/Sheet.pdf", "Basics/Circle.PNG"]);
    let files: Vec<SourceFile> = paths.into_iter().map(file).collect();
    let c = convert(
        &files,
        &texts,
        &Settings {
            title: "Music Theory".into(),
            description: "Site wide".into(),
        },
    );
    let mut out = String::new();
    for (path, text) in &c.content {
        writeln!(out, "=== {path}\n{text}").unwrap();
    }
    for (path, f) in &c.assets {
        writeln!(out, "--- asset {path} <- {}", f.path).unwrap();
    }
    for u in &c.unresolved {
        writeln!(out, "--- unresolved {u}").unwrap();
    }
    writeln!(out, "--- pages {}", c.pages).unwrap();
    out
}

#[test]
fn the_output_is_byte_identical() {
    let golden = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/golden/convert.txt");
    let got = render();
    if std::env::var_os("UPDATE_GOLDEN").is_some() {
        std::fs::create_dir_all(std::path::Path::new(golden).parent().unwrap()).unwrap();
        std::fs::write(golden, &got).unwrap();
    }
    let want = std::fs::read_to_string(golden).expect("the golden file");
    assert!(got == want, "the converter's output changed:\n{got}");
}

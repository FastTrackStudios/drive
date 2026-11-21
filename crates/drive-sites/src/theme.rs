//! The starter theme (`assets/site-theme` in this repo), built in: copied
//! into a space as its base theme folder, and used by a site whose theme
//! folder is empty or missing.

use crate::source::{Content, SourceFile};

/// Path in the theme → its text.
pub const STARTER: &[(&str, &str)] = &[
    (
        "templates/base.html",
        include_str!("../../../assets/site-theme/templates/base.html"),
    ),
    (
        "templates/index.html",
        include_str!("../../../assets/site-theme/templates/index.html"),
    ),
    (
        "templates/section.html",
        include_str!("../../../assets/site-theme/templates/section.html"),
    ),
    (
        "templates/page.html",
        include_str!("../../../assets/site-theme/templates/page.html"),
    ),
    (
        "sass/main.scss",
        include_str!("../../../assets/site-theme/sass/main.scss"),
    ),
    (
        "static/robots.txt",
        include_str!("../../../assets/site-theme/static/robots.txt"),
    ),
    (
        "README.md",
        include_str!("../../../assets/site-theme/README.md"),
    ),
];

/// The starter theme as source files.
pub fn starter() -> Vec<SourceFile> {
    STARTER
        .iter()
        .map(|(p, t)| SourceFile {
            path: p.to_string(),
            content: Content::Text(t.to_string()),
        })
        .collect()
}

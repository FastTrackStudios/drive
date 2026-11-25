//! Drive's graph (docs/graph.md).
//!
//! - [`markdown`]: reading Markdown, Obsidian's included (frontmatter,
//!   headings, `[[wikilinks]]`, embeds, relative links), code left alone.
//! - [`resolve`]: what a link names within one folder of notes.

pub mod markdown;
pub mod resolve;

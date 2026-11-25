//! Drive's graph (docs/graph.md).
//!
//! - [`markdown`]: reading Markdown, Obsidian's included (frontmatter,
//!   headings, `[[wikilinks]]`, embeds, relative links), code left alone.
//! - [`resolve`]: what a link names within one folder of notes.
//! - [`extract`]: what one Markdown file says (links, embeds, aliases,
//!   headings, tags), kept by content hash.
//! - [`space`]: what a link names across the whole space.

pub mod extract;
pub mod markdown;
pub mod resolve;
pub mod space;

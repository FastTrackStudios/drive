//! Drive's graph (docs/graph.md).
//!
//! - [`markdown`]: reading Markdown, Obsidian's included (frontmatter,
//!   headings, `[[wikilinks]]`, embeds, relative links), code left alone.
//! - [`resolve`]: what a link names within one folder of notes.
//! - [`extract`]: what one Markdown file says (links, embeds, aliases,
//!   headings, tags), kept by content hash.
//! - [`space`]: what a link names across the whole space.
//! - [`graph`]: the nodes and edges, built from what the space syncs.
//! - [`index`]: asking it (backlinks, neighbours, paths, tags), in memory
//!   or (feature `turso`) in a peer's own database ([`db`]).
//! - [`refresh`]: bringing a peer's graph up to date, the same on every peer.

#[cfg(feature = "turso")]
pub mod db;
pub mod extract;
pub mod graph;
pub mod index;
pub mod markdown;
#[cfg(feature = "turso")]
pub mod refresh;
pub mod resolve;
pub mod space;

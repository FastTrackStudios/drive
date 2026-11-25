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
//! - [`embed`] (feature `embed`): vectors, a static model every peer runs,
//!   chunks and rank fusion.
//! - [`search`] (features `turso` and `embed`): hybrid search (words,
//!   meaning and the graph, fused) and the pass that embeds new texts.
//! - [`refresh`]: bringing a peer's graph up to date, the same on every peer.

#[cfg(feature = "turso")]
pub mod db;
pub mod extract;
#[cfg(feature = "embed")]
pub mod embed;
pub mod graph;
pub mod index;
pub mod markdown;
#[cfg(feature = "turso")]
pub mod refresh;
#[cfg(all(feature = "turso", feature = "embed"))]
pub mod search;
pub mod resolve;
pub mod space;

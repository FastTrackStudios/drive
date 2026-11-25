//! Drive's graph (docs/graph.md).
//!
//! - [`markdown`]: reading Markdown, Obsidian's included (frontmatter,
//!   headings, `[[wikilinks]]`, embeds, relative links), code left alone.
//! - [`resolve`]: what a link names within one folder of notes.
//! - [`adopt_tags`]: notes' frontmatter tags as Drive tags (planned; written
//!   only when the owner asks).
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

pub mod adopt_tags;
#[cfg(feature = "turso")]
pub mod db;
#[cfg(feature = "clip")]
pub mod clip;
#[cfg(feature = "embed")]
pub mod embed;
pub mod extract;
pub mod graph;
pub mod index;
pub mod markdown;
#[cfg(feature = "turso")]
pub mod refresh;
pub mod resolve;
#[cfg(all(feature = "turso", feature = "embed"))]
pub mod search;
#[cfg(feature = "share")]
pub mod share;
pub mod space;

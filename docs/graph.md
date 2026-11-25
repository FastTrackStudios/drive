# The graph

One graph across the space: notes' Markdown links, tags, and the links
apps keep in the spine. Every peer works it out for itself from data that
already syncs. Drive has no graph database and no graph sync of its own.
Written 2026-11-24.

## Every peer builds its own

A drive-node is another machine running Drive, and everything is peer to
peer, so the graph has no privileged home. The phone, the laptop, the
desktop and the always-on box all embed the same engine
(`crates/drive-graph`) with the same code and the same tables:

- **Each peer builds its own index** from what it has: the synced
  `<org>/drive:tags` and spine documents, the space's registry and Drive
  tree, and the notes whose bytes it can read. Nothing is pushed to it
  from another machine, and no machine keeps a central copy.
- **Loro documents and files are the source of truth.** The graph is a
  projection: rebuilt whenever its inputs change, never edited (the rule
  [design.md](design.md) applies to every Turso projection).
- **Queries are local.** `drive graph` and the `graph_*` MCP tools ask
  this peer's index. There are no graph endpoints, on a server or anywhere
  else.

Where a peer runs (the `drive` agent, an app's embedded client, an
always-on machine), it hosts the index the same way: `GraphDb` in a
`graph.db` beside its other local tables, or `MemoryIndex` where there is
no disk (a browser). Both implement one small trait, `Index`, and answer
alike (`index/tests.rs` asks them the same questions).

### Why no graph database

We looked at Omnigraph (Lance-based; it can run serverless), but only one
process may write a graph, and its branches merge only inside one storage
root, so two copies made offline on different devices cannot be merged.
TerminusDB, any-sync, p2panda and iroh-docs either are not graph databases
or would be a second sync system beside Loro. A graph derived from what
Loro already syncs has neither problem: every peer that has the same
documents builds the same graph.

## The edges

| Edge | From → to | Source of truth | How other peers get it |
|---|---|---|---|
| `link`, `embed` | a note → what it names | the note's contents | derived from the file; its extract is kept by content hash |
| `related` | a note → what its frontmatter names (`related:`, any `[[…]]` in a property) | the note's contents | the same |
| `fm-tag` | a note → a tag its frontmatter `tags:` names | the note's contents | the same |
| `tagged` | a thing → a tag | `<org>/drive:tags` ([tags.md](tags.md)) | Loro, already synced |
| `parent` | a tag → its parent | `<org>/drive:tags` (the hierarchy with cycles dropped) | Loro, already synced |
| `task`, `event` | a spine task or event → the document its `link` opens | the spine | Loro, already synced |
| `part` | a project → its documents and its Drive folders | the space's registry | Loro, already synced |
| `linked` | a thing → what an app or agent linked it to (its kind in `written`: `uses`, `built-on`) | `<org>/drive:links` | Loro, already synced |
| `in` | a file or folder → the Drive folder holding it | the Drive tree | Loro, already synced |

**Nodes** use the tags' keys wherever there is one, so a node and its tags
are the same thing: `file:<tree id>` (a file or folder of the Drive tree),
`content:<sampled hash>` (a file on some computer, by content),
`folder:<location>:<path>` (a folder on some computer, [tags.md](tags.md)), `doc:<id>`,
`project:<id>`. The rest are `tag:<id>`, `task:<id>`, `event:<id>`,
`label:<name>` (a frontmatter tag that names no Drive tag) and
`missing:<name>` (what a link names that does not exist yet, one node
for every `[[Nowhere]]` in any case).

A task's or event's `link` is a path in an app (`/studio/song/amazing-grace`).
It is read back to a document through the kinds' `open` paths
(`/studio/song/{id}` with prefix `studio:song:`, `drive_spine::kinds`).

## Links between things (apps and agents)

Apps' things refer to each other: an episode uses a song, a preset is
built on another, a chart belongs to a setlist. `drive_spine::links` keeps
these in one document per org, `<org>/drive:links`, keyed by what things
are (the tags' keys: `file:<tree id>`, `content:<hash>`, `doc:<id>`,
`project:<id>`), so a link follows its thing through moves, renames and
copies.

| Map | Holds |
|---|---|
| `from` | source key → { `<kind>\t<target key>` → { to, kind, by, at } } |

One (source, kind, target) is one entry: two devices making the same link
at once leave one link. The kind is the app's own word, one line; empty is
a plain reference. Every app writes the same shape (`links::link`,
`links::unlink`), so an agent needs one tool for every app's links.

## Notes: what one file says

`drive_graph::extract` reads one Markdown file into an `Extract`: its
title (frontmatter `title`, else the first `# Heading`), every outgoing
link as written (`[[Page]]`, `[[Page|label]]`, `[[Page#Heading]]`,
`[[folder/Page]]`, `![[img.png|300]]`, relative `[x](Other%20Note.md)`,
`![alt](img.png)`, frontmatter links), its aliases, headings and
frontmatter tags (`tags: [a, b]`, `tags: "#a #b"`). Code blocks and inline
code are left alone. It is a pure function of the text, the same parser
the sites use ([sites.md](sites.md); `drive_graph::markdown` and
`drive_graph::resolve`, moved out of `drive-sites` with a golden test
proving every page came out byte-identical).

### Kept by content, shared like other derived data

An extract depends only on the file's bytes, so it is kept by content: a
`Sidecar` is `{ key: <full BLAKE3 hash>, version, extract }`, the same
idea as [sidecars.md](sidecars.md)'s (content key, kind, producer
version). A peer keeps its sidecars in `graph.db`'s `extracts` table, and
a file is never parsed twice: a move, a rename or a copy reuses it.

- A peer can **give** a sidecar to another (`GraphDb::sidecar` returns the
  bytes as they are kept).
- A sidecar **received** is a cache, never an authority: it is used only
  when its key is the content asked for and its version is this
  extractor's (`Sidecar::decode`), and never in place of the bytes when
  the peer has them.
- Every peer can always make it itself from the bytes.

What a peer knows of a note:

| It has | It knows |
|---|---|
| the bytes | the note's own edges (it parses them, once) |
| no bytes, but another peer's sidecar for that content | the note's own edges |
| neither (the file lives on a computer out of reach) | only the edges that point at the note |

## Resolving links across the space

Not just one vault. A `[[Name]]` in a note is looked for, in order:

1. the note's **own folder**;
2. a path in any vault, when the link has a folder in it (`[[Wiki/Scales]]`);
3. the rest of the note's **vault** (the nearest folder above it holding
   `.obsidian/`; a note in none has the whole Drive as its vault), by
   path, file name (the shortest path among several), title and alias;
4. the **other vaults**, and full Drive paths;
5. the space's **app documents**: by id (`[[studio:song:x]]`), by a kind's
   prefix and the name's slug (`[[Amazing Grace]]` →
   `studio:song:amazing-grace`), or by title.

A relative Markdown link is a path from the note's folder (by name if
nothing is there). Hidden folders (`.obsidian/`, `.trash/`) are never
linked. What names nothing becomes a `missing:` node, so "what links to a
page not written yet" can be asked.

## The index

`graph.db`, a peer's own Turso database:

| Table | Holds |
|---|---|
| `nodes` | id, kind, label (a title or name), path (a Drive path, for files) |
| `edges` | src, dst, kind, what was written, the anchor (indexed both ways) |
| `aliases` | a lower-case name → a node (titles, aliases, file names, tag names, shorthands and aliases) |
| `texts` | each node's searchable text (its name, path, other names; a note's tags and text), under a full-text index (`USING fts`, Turso's tantivy) |
| `extracts` | content key → the extractor's version and its sidecar |

`drive_graph::refresh` is the one routine every peer runs: the notes'
content keys from the tree, the extracts it already has, the rest from
their bytes or a peer's sidecar, then `build` and the tables replaced in
one transaction (readers see the old graph or the new), and extracts of
content no longer in the space forgotten.

| Question | How |
|---|---|
| backlinks | the edges into a node (indexed) |
| neighbours, depth N | a recursive CTE walking edges either way |
| shortest path | breadth-first over the indexed edges (at most 8 steps unless told) |
| everything under a tag | a recursive CTE down `parent` edges, then `tagged` and `fm-tag` edges into any of them (child tags count, as tag search does) |
| search | the full-text index, ranked by BM25 (`fts_score`); words only (letters and digits), so nothing typed reaches the engine's query syntax; optionally only things under a tag. The in-memory index ranks the same way with its own small BM25 |

## The command line

```text
drive graph backlinks THING
drive graph neighbors THING [--depth N]
drive graph path THING THING [--max N]
drive graph tag TAG
drive graph refresh                # and what it read: kept, read now, from a peer, out of reach
drive graph search WORDS… [--tag TAG] [--limit N]
drive graph link FROM TO [--kind K]  # in drive:links (synced)
drive graph unlink FROM TO [--kind K]
```

A THING is what `drive tag` takes (a file here, `drive:PATH`,
`LOCATION:PATH`, `project:NAME`, `doc:ID`, a raw key such as
`file:<tree id>` or `content:<hash>`), a graph key (`tag:<id>`,
`missing:<name>`, `label:<name>`, `task:<id>`, `event:<id>`), or a name the
graph knows (a note's title or alias, a file's name). A TAG is a Drive
tag's name, shorthand, alias or id, or a frontmatter tag.

Each command brings this peer's graph up to date first. Offline it answers
from what it has: after three notes in a row it cannot fetch, it stops
asking and keeps the extracts it had.

## For agents

`drive mcp` has five read-only tools, `graph_search`, `graph_backlinks`,
`graph_neighbors`, `graph_path` and `graph_tag` ([agents.md](agents.md)),
taking the same THINGs, and two that write `drive:links`, `link` and
`unlink` (with `dry_run`, counted against the session's write cap). Each answer has `coverage`: how many notes this
peer could read, so an agent knows when some were out of reach.

## Not yet

- **App UI**: a backlinks panel in the inspector and a graph view.
- **Frontmatter tags → Drive tags**: offering a note's `tags:` as Drive
  tags. It writes into the synced tags document, so it will be opt-in and
  asked for first, never automatic.
- **Handing sidecars between peers.** The format and its checks are here;
  the transport waits for peer-to-peer sync (`drive-sync`, `drive-net`).
- **Refreshing as documents change**, in `drive serve` and in apps, rather
  than on each question.
- Inline `#tags` in a note's body, and block references as their own nodes.
- Markdown on computers' locations (not in the Drive tree): their bytes are
  on one computer, so other peers would know them only by shared sidecars.
- Apps writing their links: Studio, Season and Signal call `links::link`
  for what their documents refer to.

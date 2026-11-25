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

## Pages: notes everyone edits at once

A vault's notes can be **pages**: Markdown kept as live Loro text
(`drive_spine::pages`), so several people and agents edit one note at once,
offline included, and every device merges the same (Google Docs' way, on
the same documents that sync peer to peer).

- A page is one document, `<org>/drive:page:<id>`, its whole text in `body`
  (frontmatter, wikilinks and all, so it reads exactly as an Obsidian
  note).
- It sits in its vault at a Drive path: a tree file whose `page` names it
  (`tree::create_page`). Folders, tags, links and moves treat it as any
  file; wikilinks resolve to it by its name, title and aliases.
- A write takes the whole new text and applies only what differs (a text
  diff, `LoroText::update`), so an agent that changes one line keeps what
  someone else is typing further down.
- The graph reads a page's text from its document; its content key is
  `page:<BLAKE3 of the text>`, so it is parsed and embedded once per text.
- `drive serve` keeps the graph up to date: a couple of seconds after the
  tree, the registry, the spine, the tags, the links or any page changes
  (here or on another device), it is made again, so a link typed into a
  page is a backlink a moment later.

```text
drive page new PATH [FILE|-]     # a page at PATH (folders made, `.md` added)
drive page cat PATH
drive page write PATH [FILE|-]   # only what changed is applied
drive page ls
```

MCP: `page_read`, `page_write` (a diff, with `dry_run`) and `page_create`.

**On disk, for Obsidian.** In a working copy (docs/working-copies.md) a
page is a plain `.md` file: written from the page's text, and when an
editor saves it the new text goes into the page as a diff. So a vault
folder can be open in Obsidian on one computer while people and agents
edit the same pages live everywhere else.

- A page changed on disk **and** in the Drive since the last sync is
  merged, not copied aside: each sync records the page's version, the
  disk's text is applied to a fork of the page at that version and
  imported, so the CRDT keeps both edits; the merged text goes back to
  the file.
- A new `.md` file saved in an Obsidian vault (a folder at or above it
  holds `.obsidian/`) becomes a page. Other Markdown stays an ordinary file.

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
their bytes or a peer's sidecar, then `build` (the whole graph, in
memory: about 0.2 s for 50,000 files) and only the rows that differ
written, in one transaction (readers see the old graph or the new), and
extracts of content no longer in the space forgotten. A refresh of a
50,000-file space with nothing new takes about 0.6 s; the first, which
parses every note, a few seconds.

| Question | How |
|---|---|
| backlinks | the edges into a node (indexed) |
| neighbours, depth N | a recursive CTE walking edges either way |
| shortest path | breadth-first over the indexed edges (at most 8 steps unless told) |
| everything under a tag | a recursive CTE down `parent` edges, then `tagged` and `fm-tag` edges into any of them (child tags count, as tag search does) |
| search | the full-text index, ranked by BM25 (`fts_score`); words only (letters and digits), so nothing typed reaches the engine's query syntax; fused with the nearest vectors and the graph (below). The in-memory index ranks words with its own small BM25 |
| `vectors` | a text's key, the model, the chunk, the vector (`vector32`) |

## Search: words, meaning and the graph

One search, three rankings fused (reciprocal rank fusion,
`drive_graph::search::hybrid`, on every peer):

1. **Words**: the full-text index over each node's text (BM25).
2. **Meaning**: the vectors nearest the query's (cosine, Turso's
   `vector_distance_cos`), when the peer has the model.
3. **The graph**: with `near`, what the words and meaning found within two
   edges of that thing moves up.

`tag` then keeps only what is under a tag. Every node has a text (its
name, path and other names; a note's tags and text); a long one is cut
into pieces of about 1000 characters at headings and blank lines, one
vector each, and a node scores by its best piece.

### The model every peer runs

[Model2Vec](https://github.com/MinishLab/model2vec)'s **potion-base-8M**: a
tokenizer and one table of token vectors (256 dimensions, about 30 MB). A
text's vector is the mean of its tokens' rows, normalised: pure Rust
(`tokenizers` with `fancy-regex`, `safetensors`), microseconds a note, the
same on a phone, in a browser and on the always-on machines, so every
peer can embed everything it has. `drive graph model fetch` puts it in
`models/potion-base-8M` in the agent's folder (or `$DRIVE_EMBED_MODEL`);
without it, search is by words and the graph.

Vectors are kept by **text and model**: `vectors(key, model, chunk, vec)`,
the key being the text's BLAKE3 hash, so the same text anywhere is
embedded once, a changed one again, and vectors of different models never
meet. They are derived data like extracts: a peer that has the text can
always make them itself.

Machines with more to give (the always-on box, starcommand, thebattleship)
run the same model over everything they can read; heavier models (a
transformer for text, CLIP for images) are other `Embedder`s, kept under
their own model id beside it, so a peer uses them only when it has them.

## Frontmatter tags as Drive tags (when asked)

A note's `tags:` are its own words; Drive's tags are what every app and
search share. `drive graph adopt-tags` (MCP `adopt_frontmatter_tags`)
puts each note's frontmatter tags on it as Drive tags: an existing tag by
name, shorthand or alias, else a new one, nested as Obsidian nests
(`music/drums` is drums under Music). A name several Drive tags share,
with nothing to tell them apart, is left alone. Without `--apply` (or with
`dry_run`) it only says what it would do. It never runs by itself: the
tags document syncs to every device.

## The command line

```text
drive graph backlinks THING
drive graph neighbors THING [--depth N]
drive graph path THING THING [--max N]
drive graph tag TAG
drive graph refresh                # and what it read: kept, read now, from a peer, out of reach
drive graph search WORDS… [--tag TAG] [--near THING] [--limit N]
drive graph model fetch              # the embedding model, once per device
drive graph adopt-tags [--apply]     # notes' frontmatter tags as Drive tags
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
- **Handing sidecars between peers.** The format and its checks are here;
  the transport waits for peer-to-peer sync (`drive-sync`, `drive-net`).
  Vectors cannot be checked as cheaply as extracts; a peer will take them
  only from the owner's own devices, and make its own when it has the model.
- **Images by what they show**: CLIP image vectors (on the machines that
  can run it), and OCR text into the full-text index.
- An approximate nearest-neighbour index once vectors run past a few
  hundred thousand pieces (Turso's vector index is not ready yet; the
  search reads every vector of the model).
- An editor for pages in the Drive app (live cursors through presence), and
  pages as a source for sites (docs/sites.md's "Loro page documents").
- **Refreshing as documents change** in apps that embed the engine (the
  agent's service does it).
- Inline `#tags` in a note's body, and block references as their own nodes.
- Markdown on computers' locations (not in the Drive tree): their bytes are
  on one computer, so other peers would know them only by shared sidecars.
- Apps writing their links: Studio, Season and Signal call `links::link`
  for what their documents refer to.

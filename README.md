# Drive

One local-first, server-optional system for everything a person or a business
keeps: tasks, projects, calendars, people, notes, files, media, and the data of
every app built on it.

Apps are separate, focused products (Drive, Studio, Meals, Frame, Mail, wikis,
…) that embed Drive's client—none deploys a backend of its own. Every device
holds its data locally and works offline; devices sync with each other over
**Loro CRDTs** and **iroh**; optional always-on nodes and object storage make
sharing, search, and availability seamless without ever being required. Anyone
can be invited by link, live, with nothing installed.

## Goals & Main Purpose

- **Apps without backends:** Domain apps embed `drive-client` / `drive-dioxus`
  and mutate local CRDT documents with instant optimistic UI, full commit
  history, and automatic conflict-free sync.
- **One shared spine across apps:** A common schema (`Task`, `Project`, `Event`,
  `Person`, `Note`, `File`, `Thread`, `Kinds`, `Tags`, `Comments`) lets files,
  projects, review comments, tasks, and calendars link together across apps and
  org spaces.
- **Files stay where they live, reachable everywhere:** Instead of forcing every
  terabyte into one cloud bucket, the `drive` agent indexes folders and storage
  backends (`opendal`) in place, exposes them in a single FUSE mount (`~/Drive`
  and `Devices/`), enforces 3-2-1 backup protection policies, and streams byte
  ranges peer-to-peer via `iroh-blobs`.
- **Built-in media, automation & agent tooling:** Content-addressed sidecars
  (thumbnails, waveform peaks), in-app viewers (audio, video, PDF, images,
  Markdown), Hazel-style file rules, static site publishing (`dodeca`), P2P Rust
  build caches (`kache`), disk-speed local file materialization (`drive-local`),
  and a safe MCP server (`drive mcp`) for AI agents.

## Workspace Layout

| Crate / App | Role |
|---|---|
| `apps/drive` | The Drive browser, file manager, project/review hub, tag & rule UI (Dioxus web + Blitz native) |
| `crates/drive-node` | Optional always-on sync peer, SQLite/Turso index & search server, sidecar cache, share links, and static site host |
| `crates/drive-agent` | The `drive` CLI & background daemon: FUSE mount, disk indexer, live disk browsing, 3-2-1 protection, rules runner, sidecar producer, site builder, kache peer, and MCP server |
| `crates/drive-client` | Local-first Loro CRDT sync client (WASM/browser + native) |
| `crates/drive-dioxus` | Reactive Dioxus hooks (`use_doc`, `use_docs`), OAuth sign-in, presence, and `CommentsPanel` |
| `crates/drive-native` | Headless native Tokio client used by daemons and CLI tools |
| `crates/drive-local` | Local app-files client talking to the running `drive` agent for disk-speed file access and pins |
| `crates/drive-spine` | Shared document spine: tasks, events, recurrence, notes, tags/fields/queries, comments, kinds, and history |
| `crates/drive-files` | Space registry, locations, adoptions, projects, cross-org lends, share links, 3-2-1 policies, and blob storage (`opendal`) |
| `crates/drive-proto` | Wire protocol (`ToNode`, `ToClient`, `Browse`, org qualification) |
| `crates/drive-transfer` | Byte-range P2P file streaming over `iroh` / `iroh-blobs` |
| `crates/drive-sidecars` | Content-keyed derived data (image/video/PDF thumbnails, audio waveform peaks) |
| `crates/drive-view` | Embedded file viewers (PDF via `pdfcraft`, audio/video, images, Markdown, text) |
| `crates/drive-rules` | Synced file rules AST, matcher, planner, dry-run/live evaluator, and undo journal |
| `crates/drive-sites` | Static site conversion & publishing from Drive folders (`dodeca`) |
| `crates/drive-kache` | P2P Rust build cache server/peer for `kache` build groups |

## Developing & Testing Safely

If a live Drive instance is already running on your machine (`127.0.0.1:8096`,
`~/.local/share/drive-agent`, `~/Drive`), **never restart or unmount it while
developing**. See [docs/dev.md](docs/dev.md) and [AGENTS.md](AGENTS.md) for the
isolated workflow:

```sh
# 1. Default development loop (in-process, ephemeral ports & temp dirs)
cargo test -p drive-node
cargo test --workspace
cargo clippy --workspace --all-targets

# 2. Isolated live dev node + agent on port 18096 (under .dev/, gitignored)
scripts/dev.sh up            # serve mode (works in any shell)
scripts/dev.sh up --mount    # with an isolated FUSE mount at .dev/mount
eval "$(scripts/dev.sh env)" # point drive CLI, drive-local & native apps at dev
scripts/dev.sh down          # stops only the dev PIDs
```

## Documentation

- [docs/design.md](docs/design.md): overall architecture and data model.
- [docs/files.md](docs/files.md): files, locations, projects, share links, and 3-2-1 protection.
- [docs/mount.md](docs/mount.md): one folder for everything: the Drive tree and every device's locations and disks under `Devices/`.
- [docs/adopt.md](docs/adopt.md): adopting a folder in place at a logical path in the Drive tree (nothing copied or moved).
- [docs/browse.md](docs/browse.md): every computer's disks, browsed live and read-only.
- [docs/transfer.md](docs/transfer.md): file bytes between devices over `iroh-blobs`, by range, in place.
- [docs/app-files.md](docs/app-files.md): any Drive file as a real local path at disk speed, for apps (e.g. an audio engine), wherever it lives.
- [docs/sidecars.md](docs/sidecars.md): thumbnails and waveforms, derived once per file content.
- [docs/viewers.md](docs/viewers.md): embedded viewers (PDF, audio, video, images, Markdown).
- [docs/tags.md](docs/tags.md): tags, fields, and tag search.
- [docs/rules.md](docs/rules.md): file rules, synced and run by every device (dry run first; nothing deleted).
- [docs/sites.md](docs/sites.md): static sites from folders of Markdown (`dodeca`).
- [docs/kache.md](docs/kache.md): Rust build caches (`kache`) shared peer-to-peer between the owner's computers.
- [docs/agents.md](docs/agents.md): Drive as an MCP server, so AI agents can inspect and organize files safely.
- [docs/dev.md](docs/dev.md): isolated local dev stack and testing workflow.
- [docs/apps/meals.md](docs/apps/meals.md): the Meals app.
- [PLAN.md](PLAN.md): the prototype plan.
- [PROTOTYPE.md](PROTOTYPE.md): the running prototype (Drive and companion apps).

## Status & Licence

Active working prototype (see [PROTOTYPE.md](PROTOTYPE.md)). First companion
apps built on Drive: Studio (`../studio-proto`), Meals (`../meals`), and Frame
(`../frame-app`).

Workspace license: `GPL-3.0-or-later` (see [Cargo.toml](Cargo.toml)).

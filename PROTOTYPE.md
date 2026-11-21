# The prototype

Drive and four apps on it, in the browser, live for everyone: the
Liveblocks demos, on our own system. Built 2026-11-21.

## What runs

One `drive-node` (`crates/drive-node`) serves the sync websocket (`/sync`)
and each app's web build, from one address:

| Path | App | Repository |
|---|---|---|
| `/` | Drive: the inbox, the calendar, notes, everything | `apps/drive` here |
| `/studio/` | Studio: sets live (keys, arrangements, presence, following the leader) | `../studio-proto` |
| `/meals/` | Meals: building blocks, plans, prep together | `../meals` |
| `/frame/` | Frame: a multiplayer design app for control surfaces | `../frame-app` |

On THEBATTLESHIP it runs as the user service `drive-node` (port 8096),
served on the tailnet at `https://thebattleship.tail666c4b.ts.net:9444/`.

```sh
cargo build -p drive-node --release
target/release/drive-node --listen 127.0.0.1:8096 --data data \
  --app /=target/dx/drive-app/release/web/public \
  --app /studio=../studio-proto/target/dx/studio-proto/release/web/public \
  --app /meals=../meals/target/dx/meals/release/web/public \
  --app /frame=../frame-app/target/dx/frame-app/release/web/public
# each app: dx build --platform web --release [--base-path <name>]
# (dx 0.8.0-alpha.3, matching the dioxus crate)
```

## How it works

- Every document is a Loro document. The browser keeps a copy
  (localStorage), so it opens instantly and works offline; edits made
  offline are sent on reconnect (the join exchanges version vectors).
- The node is one more peer: it merges what it is sent, keeps it in Turso,
  and passes it to everyone else in the room. It never decides who wins.
- Presence (cursors, selections, drags in progress, carets, who is here)
  is JSON per person per document, never stored.
- The spine (`drive:spine`, `crates/drive-spine`) is one calendar and one
  inbox that every app writes to; the Drive app shows them.
- Frame's documents are Frame's own scene: its `LoroScene` wraps the same
  Loro document Drive syncs, and every edit is one of Frame's ops.

## The Liveblocks demos, here

| Liveblocks demo | Here |
|---|---|
| Live cursors | Frame's canvas (name tags, colours) |
| Live avatar stack | every app's bar (click yours to rename) |
| Live form selection | Drive's inbox (who edits which task), Studio (which song), Meals (which prep step) |
| Connection status | every app (Live, Connecting, Offline · edits kept) |
| Collaborative text editor | Drive's notes: merged typing, everyone's caret |
| Collaborative to-do list | Drive's inbox |
| Collaborative whiteboard / design tool | Frame (theirs has no Figma-style tool) |
| Comments | Frame's pins and threads |
| Undo / redo | Frame (your own edits only) |
| Multiplayer form | Frame's properties, Meals' builder |
| Calendar | Drive's calendar, filled by Studio and Meals |
| Offline | every app (Liveblocks presents none) |

## Not in the prototype

iroh (peer-to-peer and the local network: the browser always goes through
the node), accounts and permissions (everyone is a guest with a name),
share-link scopes, media blobs, Mail, the native and iOS builds of these
apps (drive-client is browser-only so far), Frame's GPU renderer (the
editor draws SVG; gradients show their first stop).

# The Drive tree in Finder: a File Provider

On macOS the Drive tree shows in Finder the way iCloud Drive does: a
**File Provider extension** (`NSFileProviderReplicatedExtension`) inside a
small app around the agent ([#164](https://github.com/FastTrackStudios/drive/issues/164)).
Finder draws its own cloud and download badges and offers *Download Now*
and *Remove Download*. A file lists at its real size and its bytes come
down when something opens it. No kernel extension is needed, unlike the
FUSE mount, which on a Mac needs macFUSE.

## Who does what

```text
Finder ─▶ fileproviderd ─▶ the extension (Swift, sandboxed)
                               │  C ABI
                               ▼
                         drive-fileprovider (Rust, static library)
                               │  drive_local::proto, one JSON object a line
                               ▼
                         the agent  ─▶  the Drive tree (a Loro document), the node
```

- **The extension** answers the system's callbacks and nothing else. It
  keeps no state and touches no files of its own: every question goes to
  the agent.
- **The agent** answers from the tree document, and its writes do what the
  mount's do (`crate::fileprovider` in `drive-agent`). These are its
  operations on the local app API (`drive_local::proto::Op`):

| op | answer |
|---|---|
| `fp_item {item}` | one item |
| `fp_list {parent, after?, limit?}` | a folder's items, a page at a time (`next` → the next page's `after`) |
| `fp_anchor` | the tree's version now |
| `fp_changes {anchor}` | the items updated and the ids deleted since then, and the new anchor; `expired` if that version cannot be gone back to |
| `fp_fetch {item, into}` | the file's bytes copied into `into`, and the item |
| `fp_create {parent, name, dir?, from?}` | a new folder, or a file whose bytes are read from `from` |
| `fp_modify {item, name?, parent?, from?}` | rename, move and/or new bytes |
| `fp_delete {item}` | into the Drive's Trash (nothing is ever deleted) |

An item is named by `item` in a request, not `id`: every request already
has its own `id`, its number on the connection, as a key of the same
object.

## Items

- **Ids** are tree ids; the top is `root`.
- **The Trash** is not shown. What is in it reads as deleted, and deleting
  moves there.
- **`content`** changes whenever a file's bytes do (its hash and size), and
  **`meta`** whenever its name or place does. They are the item's
  content and metadata versions for the system.
- **`here`** says whether a file's bytes are on this device already. It is
  what puts the cloud badge on a file, or takes it off.

## Changes

An **anchor** is the tree document's version (its Loro frontiers, in hex).
`fp_changes` compares the tree at that version (`LoroDoc::fork_at`) with the
tree now:
- an item that is new, or whose name, place, kind or bytes differ, is
  *updated*;
- an id no longer shown (gone, or in the Trash) is *deleted*.

An anchor naming changes the document doesn't have (another device's tree,
a reset) is *expired*, and the system then enumerates again. It is only
expired when that is true: a provider that always says "expired" makes the
system enumerate forever, as Task's first extension found.

## The macOS side (`macos/`)

```text
macos/
  FileProvider/
    Bridge.swift                   the agent through drive-fileprovider; errors mapped to the domains the system accepts
    Item.swift                     NSFileProviderItem from FpItem (ids are tree ids)
    Enumerator.swift               folders a page at a time; changes since an anchor
    FileProviderExtension.swift    the replicated extension: lookups, fetch, create, modify, delete
    Domains.swift                  the registrar tool: sync | list | clear | signal | watch | evict PATH | ping
    Info.plist, DriveFileProvider.entitlements
  Drive.entitlements               the containing app's
  build-fileprovider.sh            the Rust bridge, the .appex and the registrar, signed
  try-fileprovider.sh              a throwaway Drive.app around them, registered with the system
```

### How the pieces connect

- **The socket:** the agent serves its app API on a second socket inside the
  app group's container, `~/Library/Group Containers/28C2G63DA7.app.fasttrackstudio.drive/app.sock`.
  That folder is the only one the sandboxed extension can reach, so it needs
  no network entitlement and no absolute-path exception.
- **Staging:** fetched bytes are copied into `staging/` there, and outgoing
  edits are staged there too. The agent may not read the extension's own
  container, and the extension may not read the agent's cache.
- **Ids:** Drive uses tree ids, which survive renames and moves. (Task uses
  paths, because its tree is files on disk.)
- **Signing:** a Developer ID identity (team `28C2G63DA7`, in
  `fts-build.keychain` on airlock). The app group is team-prefixed, the form
  Developer ID builds use without a provisioning profile.

### Trying it on a Mac

```sh
KEYCHAIN_PW=… bash macos/try-fileprovider.sh          # build, sign, register "Drive"
cargo build --release -p drive-node -p drive-agent --no-default-features
drive-node --listen 127.0.0.1:18150 --data /tmp/dfp/node --org dev --iroh-local &
drive --dir /tmp/dfp/agent init --node ws://127.0.0.1:18150/sync --space dev --name Mac
drive --dir /tmp/dfp/agent serve &                    # also serves the socket in the group container
target/fileprovider/Drive.app/Contents/MacOS/Drive watch &   # signals Finder when the tree changes
ls ~/Library/CloudStorage/Drive-Drive                  # Finder's "Drive"
```

Order matters: register first, so macOS makes the group container itself
when the signed app launches. Then start the agent.

### Checked on airlock (macOS 27.2, M4), 2026-11-24

- **Writes through the folder** (`mkdir`, write a file, `mv` to rename or
  move, `rm`) land in the tree as the same edits the mount makes. A deleted
  file goes to Drive's Trash.
- **A file put into the tree elsewhere** (`drive put`) appears in the
  folder within about 2 s with `watch` running, and reading it downloads it
  through `fetchContents`.
- **`evict`** (Remove Download) leaves a placeholder: 0 KB on disk, at its
  real size. Reading it brings the bytes back. A 50 MB file wrote in about
  3 s and read back after eviction in about 1 s, with the same SHA-256.

### Found on the way

- **Errors must be in `NSFileProviderErrorDomain` or `NSCocoaErrorDomain`.**
  The system refuses any other domain ("unsupported error") and the call
  fails silently. `providerError` maps every error.
- **Files need `.allowsEvicting`,** or eviction is refused with -2008
  (`nonEvictable`).
- **The system keeps what an item last said until its metadata version
  changes.** `itemSchema` is part of every metadata version: bump it when
  capabilities or types change.
- **The extension is relaunched every minute or so,** so a poll inside it
  misses changes. Keeping Finder current needs a long-running process
  (`Domains watch` now; the menu-bar app later).

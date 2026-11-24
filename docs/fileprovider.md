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
| `fp_item {id}` | one item |
| `fp_list {parent, after?, limit?}` | a folder's items, a page at a time (`next` → the next page's `after`) |
| `fp_anchor` | the tree's version now |
| `fp_changes {anchor}` | the items updated and the ids deleted since then, and the new anchor; `expired` if that version cannot be gone back to |
| `fp_fetch {id, into}` | the file's bytes copied into `into`, and the item |
| `fp_create {parent, name, dir?, from?}` | a new folder, or a file whose bytes are read from `from` |
| `fp_modify {id, name?, parent?, from?}` | rename, move and/or new bytes |
| `fp_delete {id}` | into the Drive's Trash (nothing is ever deleted) |

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

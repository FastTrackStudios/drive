# Adopting a folder in place

A folder that lives by itself on a disk (song sessions on a project
drive, say) is taken over by Drive where it is. Written 2026-11-22.

```text
drive adopt FOLDER --as LOGICAL/PATH   # FOLDER shows at MOUNT/LOGICAL/PATH
drive adopt --list                     # every adopted folder: path, device, folder
drive unadopt LOGICAL/PATH             # the mapping only; the files stay
```

For example, `drive adopt /media/ProjectDrive/Songs --as Studio/Songs`
shows that folder's files at `~/Drive/Studio/Songs`. From then on the
owner works through `~/Drive`. `Devices/<device>/…` still shows where the
bytes physically are.

## What adopting does, and never does

- **Nothing is copied, moved, renamed or written.** No file of the folder
  is touched. Its inodes and modified times stay the same, and the tests
  check this.
- **The folder becomes a location.** If it is already a location of this
  device, or a folder inside one, that location is used and nothing is
  walked again. Otherwise it becomes a new location, named after the
  folder, and is indexed now. A failed index is not fatal: the agent's
  next pass builds it.
- **It is mapped into the Drive tree.** The space's registry
  (`drive:space:<space>`, map `adopted`) records the logical path, the
  location and the folder inside it. Because it is synced, the mapping
  survives restarts, and every device sees it.
- **Folders on the way are made.** `Studio` for `Studio/Songs` is made in
  the Drive tree if it is missing. These are empty folders, so the app
  and `drive ls` show the place too. The adopted folder itself is never
  a tree entry.

Adopting is refused when:

- the logical path is the top, or is under `Trash/` or `Devices/`;
- the logical path is already adopted, or is inside or above another
  adoption (adoptions never nest);
- the Drive tree already has an entry at that path;
- the folder is a file, the agent's own data, or inside a Drive mount.

**Views, not copies.** One location can be adopted at several logical
paths (`Studio/Songs`, and `Live/Tonight` for one song inside it). The
same files then show in each place.

**Unadopting** removes the mapping and nothing else. The folder, its
files, its location and its index stay as they were. The folders made on
the way stay in the tree, empty.

## In the mount

`adopt_fs` is a layer over the Drive tree's file system. It uses its own
inode range, from `1 << 61` up to `1 << 62`, where `Devices/` starts.

- **Listing:** a tree folder lists its own entries plus the adopted
  folders in it. An adopted folder replaces a tree entry of the same name.
- **This device's adopted folders** are the folders on disk, read and
  written in place.
  - Reads, writes, new files and folders, renames and times go straight
    to the disk.
  - Free space is the disk's own.
  - Something that would delete or replace a file is moved into the
    location's `.drive-trash` instead, the same trash file rules use
    ([rules.md](rules.md)). This covers unlink, rmdir and a rename onto
    an existing name, which is how editors save. The move is a rename on
    the same disk, so nothing is lost. Every replaced version is kept
    (no pruning). The trash is hidden in the mount; the app restores
    from it.
- **Another device's adopted folders** are listed from its index and read
  through the node. They are read-only.
- **Refused moves:**
  - An adopted folder can't be removed or renamed from the tree above it
    (`EPERM`). Use `drive unadopt`.
  - A move between an adopted folder and the tree's own files gets
    `EXDEV`, so programs copy instead.
- **Extended attributes:**
  - `user.drive.state`: `here`, `remote` or `folder`.
  - `user.drive.adopted`: the logical path.
  - `user.drive.where`: where the bytes are. That is the real path, or
    `<device>:<path>` for another device.

Code: `crates/drive-files/src/adopt.rs` (the model: paths, conflicts,
where a logical path falls), `crates/drive-agent/src/adopt.rs` (adopt,
unadopt, the logical view without a mount), and
`crates/drive-agent/src/adopt_fs.rs` (the mount layer, over the one mount
of [mount.md](mount.md)).

A location made by `drive adopt` while the agent runs is watched live
from the next registry change on (the daemon's `Watchers`), under the same
rules as every location (small, on a local disk); the others are
rescanned every ten minutes.

## Follow-ups

- **Writing to another device's adopted folder**, through the node or
  iroh. It is read-only now.
- **The app, the MCP tools and `drive ls`** show the Drive tree only: they
  see `Studio`, not the adopted `Songs` in it. They should resolve
  adoptions as the mount does (`Agent::logical_list`).
- **Remote listings from cached trees.** Another device's adopted folder
  is listed by scanning its whole index on each call; big locations want
  the daemon's per-location trees.
- **Pruning the trash history** of adopted folders: the owner's decision.
- **Availability across devices** (`--available`): left to the
  on-demand and hot-data placement work.

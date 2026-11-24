# Working copies

A working copy is a folder of the Drive tree that one device also keeps
as plain files in a folder on its disk, synced both ways. Written
2026-11-24.

```text
drive own Studio/Songs/Fire     # an adopted folder becomes the Drive's own
drive own --all                 # every folder adopted from this device
drive working                   # every working copy: drive:PATH ←→ DEVICE:FOLDER
```

## How it differs from adopting

An adoption ([adopt.md](adopt.md)) only points: the files stay outside
the tree, and other devices see them through the owning device's index.
A working copy puts them **in** the Drive tree:

- Every device sees them as ordinary files, Finder's Drive on a Mac
  included, with their bytes in the storages (and the node).
- They can be moved and renamed anywhere in the Drive. The binding is to
  the tree folder's id, so it follows.
- On the owning device the folder stays where it was, and programs keep
  editing it in place. The mount there shows that folder itself
  (`Agent::in_place`), read and written on disk.

## The binding

The space's registry (`drive:space:<space>`) holds the bindings under
`working`: the tree folder's id → the device, the location the folder is
in, and the folder inside it (`drive_files::working`).

## Reconciling

One pass (`Agent::reconcile`) compares three views of the folder: its
files on disk, the tree under its root, and both as they were at the last
sync. The last-sync view is the device's own,
`<agent dir>/working/<root>.json`, and is never synced. `working::plan`
decides; the agent then acts:

| On disk | In the tree | What happens |
|---|---|---|
| new or changed | unchanged | hashed, its bytes kept (storages, node), the tree file made or updated |
| unchanged | new or changed | its bytes fetched and written into place (whole file, then renamed) |
| changed | changed | if the bytes are the same, nothing; otherwise **both kept**: the disk's version as `name (conflict DEVICE DATE).ext` (also put in the tree), the tree's at the name |
| gone | unchanged | the tree file goes to the Drive's Trash |
| unchanged | gone | the file goes to the folder's `.drive-trash` (a rename, never a delete) |
| gone | changed (or changed / gone) | the change wins |
| unchanged | moved or renamed | moved or renamed on disk the same way |
| new, with the bytes of a file gone from it | | the tree file is moved or renamed (it keeps its id) |

Folders follow their files: a folder made on one side is made on the
other.

It runs a few seconds after the folder (its location's watcher) or the
tree changes, and every ten minutes.

### Safety

- **One reconcile of a working copy at a time**, across processes too: a
  lock on `<root>.lock`, taken before anything is read.
- **No reconcile from a copy behind the last sync.** If the last-sync view
  names a tree file this process's copy of the tree does not have yet, it
  does nothing; otherwise it would read those files as gone.
- **The tree is committed before the last-sync view is saved**, so a crash
  leaves the view behind the tree (redone next time), never ahead of it.
- **Every file is checked again right before it is acted on.** A file
  being written is left for the next pass, and is recorded from its size
  and time before the read. A file that changed on disk or in the Drive
  since the walk is left alone.
- **Nothing on disk is overwritten without a copy.** A file replaced by
  the Drive's version is moved into `.drive-trash` first.
- **No pass sends more than max(20, 5 %) of the files to a trash.** A
  folder that went missing, or a copy of the tree that is wrong, is caught.
  The pass says so and leaves them.
- `drive own` needs the node reachable, so its tree edits reach the agent
  running beside it.

A working copy's files are not copied into the agent's cache. The file on
disk is this device's copy of their bytes.

## Owning an adopted folder

`drive own PATH` runs on the device whose disk has the folder. It removes
the adoption, makes a tree folder in its place, records the binding, and
reconciles once. That first reconcile puts every file in the tree. No file
on disk is moved or changed.

## Finding files with Drive down

`drive where` ([where.md](where.md)) knows working copies. For a path
inside one it gives the device and the real path on its disk, and who
holds the bytes.

## Not yet

- Files are read whole into memory to hash and upload them. That is fine
  for session folders (the largest file in today's songs is 120 MB); big
  media should stream.
- A folder emptied on one side stays, empty, on the other.
- `.drive-trash` is never pruned. Every version replaced from the Drive is
  kept.
- A bulk delete over the trash limit is refused on every pass and only
  logged; there is no command to let it through yet.
- Two processes reconciling the same working copy, both with current copies
  of the tree, are kept apart by the lock, but each plans from its own copy.
  This happens only while `drive own` runs beside the agent.
- On file systems with 1–2 s timestamps (exFAT, some network shares), a
  same-size save right after a sync can be missed until the file changes
  again.

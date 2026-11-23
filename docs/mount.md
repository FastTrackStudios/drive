# The mount: one folder for everything

`drive mount MOUNTPOINT` shows everything Drive has as one folder. Written
2026-11-22.

```text
MOUNTPOINT/
  …                                   the Drive tree: the space's files
  Devices/
    <device>/
      Locations/<location>/…          the folders it indexes
      Disks/<disk>/…                  every disk it has, live
```

## What is where

- **The top** is the Drive tree, the main way in: the space's own files,
  readable and writable, organised however the owner likes. The same
  file may show in more than one place here (views, not copies).
- **`Devices/`** shows where bytes physically are, for looking and raw
  access. It is read-only and not the normal way in:
  - `Locations/` holds the folders each device indexes, built from the
    indexes this device keeps a copy of. So it lists every file even
    while that device is off, and finding things costs nothing. Reading
    one of this device's files reads it in place. Another device's file
    is brought through the node and kept in the agent's cache.
  - `Disks/` holds each device's disks as browsing sees them
    ([browse.md](browse.md)): the home folder, the system disk and every
    disk mounted on it.
    - This device's disks are read here, under browsing's own rules:
      confined to the disk, and never `/proc`, `/sys`, `/dev` or the
      agent's own data.
    - Another device's disks are read through the node's `/browse`
      routes.
    - Disk folders are named by the disk's own name. Two disks with the
      same name get ` (2)`, ` (3)`, … added.
- **Devices** listed are those with a location or a disk in the registry,
  so browsers that only sign in are not shown.

## Never hanging

- **Waiting:** another device's disk is asked with a short timeout:
  4 s when it is on, 1.5 s for the node's memory of it when it is off.
  - An off device still lists its locations (from the index) and its
    disks (from the node's memory, or the registry).
  - Inside an off device's disk you see the folders the node has seen
    before. Everything else fails at once with `EHOSTDOWN` ("Host is
    down").
- **Caching:** listings are kept for 10 s, and a device's disks for 30 s.
  Another device's file is read 4 MiB at a time, and the last 8 pieces
  are kept.
- **Threads:** the mount runs 4 threads, so a slow answer holds one of
  them and not the whole folder.
- **No loops:** Drive's own mount points are never entered. This holds
  from `Disks/`, which would otherwise reach `~/Drive` through the home
  folder, and from another device's browsing. The check is by name,
  before anything on disk is looked at.

## The two sides

The tree and `Devices/` are two file systems in one inode space:

- **Inodes:** the tree's inodes are below `1 << 62`, and every inode of
  `Devices/` is above it. Each call goes to the side its inode is on.
- **The top folder** belongs to both. Its listing is the tree's top with
  `Devices` added, and looking up `Devices` there opens the devices'
  side.
- **Moves** between the sides are refused with `EXDEV`, so programs copy
  instead, as between two disks.
- **Writes** of any kind under `Devices/` get `EROFS`.

**A tree entry named `Devices`.** If the tree's top folder has its own
entry named `Devices`, it is not lost: it shows as `Devices (in Drive)`.
Opening, renaming, deleting or writing through that name reaches it. A
new top entry can't be called `Devices` (`EEXIST`). If the tree also has
a literal `Devices (in Drive)`, that entry stays hidden in the mount, but
the app and the CLI still show it.

## Options

- `--no-devices` leaves out the `Devices/` folder.
- `--computers PATH` also mounts the `Devices/` folder alone at `PATH`,
  with the same layout. It was the old separate Computers mount.

Code: `crates/drive-agent/src/one.rs` (the composite),
`devices.rs` (`Devices/`), `fs.rs` (the tree).

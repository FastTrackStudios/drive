# Finding files with Drive down

`drive where` says where a file of the Drive really is: the device and
the path on its disk, or who holds its bytes. It works with Drive down:
no agent running, no node reachable.

```text
drive where Studio/Songs/Fire - JT Wright      # a path of the Drive
  on THEBATTLESHIP: /run/media/Development/vaults/studio-demo/recital-songs/fire  (adopted from Recital songs (Development))

drive where Studio/Setlists/Crescendum.setlist
  244 B  blake3 ae3e0ca5…
  stored on Starcommand
  cached on THEBATTLESHIP
  stored on node

drive where --find "drive to la"               # by name: the tree and every computer's index
Starcommand · Task:fasttrackaudio/session/drive-to-la/Drive To LA.session
  on Starcommand: /mnt/storage/Task/fasttrackaudio/session/drive-to-la/Drive To LA.session
```

## What it reads

Only this computer's own copies, and it never writes:

- **The tree and the registry:** the snapshots in `<agent dir>/state/docs/`
  (`<space>/drive:files`, `<space>/drive:space`). The agent writes each one
  whole and then renames it into place, so reading them beside a running
  agent is safe. The registry has the devices, locations, storages,
  adoptions and holdings (who keeps which blob).
- **The location indexes**, for `--find`: `<agent dir>/indexes.db`. It is
  opened only if it is there, and nothing in it is made or changed
  (`IndexDb::open_existing`). Every computer's index is in it, so a file on
  a computer that is off is found too.

## What it answers

- **In an adopted folder** ([adopt.md](adopt.md)): the device, and the real
  path (the location's path, the adopted folder, the rest of the path).
- **A file of the Drive tree:** its size and BLAKE3 hash, and each holder:
  a device that has it in its cache (`cached`, `pinned`), or a storage
  (`stored`). For a storage that is a folder on a device it gives the
  blob's real path (`<storage>/blobs/ab/cd/<hash>`).
- **A file in a location** (`--find`): the device and its real path, and
  its Drive path when it is inside an adopted folder.

The answers are as this computer last synced them. A change made
elsewhere since then is not in them.

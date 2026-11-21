# Files, devices and storage

How Drive becomes a Dropbox: your devices registered to you, storage you
add (a folder, a disk, a bucket), and every file visible in the computer's
own file browser, whether or not its bytes are on this machine, with its
sync state shown. Written 2026-11-21.

## The pieces

| Piece | What it is |
|---|---|
| **Space** | Whose files: a person's or an org's. One file tree per space. |
| **Device** | A computer, phone or server, with its own key. Registered to a space by pairing. |
| **Storage** | Somewhere blobs live, reached through OpenDAL: a folder on a device, a NAS, S3, R2, B2, WebDAV, SFTP, … Registered to a space; run by one device (the one holding its credentials). |
| **File tree** | One Loro document per space (`drive:files:<space>`): a movable tree of folders and files. A file entry holds its name, size, times, MIME type and its content's BLAKE3 hash, never its bytes. |
| **Blob** | A file's bytes, named by their BLAKE3 hash, kept in one or more storages (`blobs/ab/cd/<hash>`). Same bytes, one blob, wherever they come from. |
| **Agent** | `drive-agent`, the program on each computer: keeps the device's cache, moves blobs to and from storage, and shows the tree to the operating system. |

## Registering a device

1. The first device creates the space: it makes its key and becomes the
   space's first device.
2. Another device asks to join: the agent (or app) shows a short **pairing
   code**; on a device already in the space you enter it (or scan its QR
   code). The node matches the two and the new device's key is added to
   the space's **device registry** (`drive:devices:<space>`), signed by the
   device that approved it.
3. Each device records there: its name, kind (laptop, phone, server, NAS),
   OS, its roles (client, always-on, storage) and when it was last seen.
   Online status is live presence.

With accounts (later), signing in replaces the code: the account vouches
for the new device's key.

## Registering storage

- A storage is an **OpenDAL operator**: a scheme (`fs`, `s3`, `webdav`,
  `sftp`, `b2`, `gcs`, …) and its options.
- **Credentials never sync.** The registry (`drive:storage:<space>`) holds
  only what others may see: name, scheme, where (bucket, root, host),
  which device runs it, its capacity and usage. The keys stay in that
  device's own config.
- **Who can reach it:** a storage on a device's disk is reached through
  that device (its agent serves the blobs); a cloud bucket is reached by
  the device holding its keys, which hands others short-lived signed URLs
  where the service has them.
- **Placement rules** say what goes where: "every blob to the NAS and to
  R2", "this folder only to the NAS", "the last 30 days on the laptop".

## Bytes: hashing and moving

- **Hash:** BLAKE3. A sampled hash (size + first, middle and last 64 KiB)
  finds likely duplicates fast; the full hash, checked before anything is
  trusted, reused or deleted, is the blob's name (Spacedrive's method).
- **Upload:** a new or changed file is hashed, its blob written to the
  placement's storages (skipped where the hash already exists), then its
  entry in the tree updated with the new hash: the tree change is the
  commit point.
- **Download:** on open (or pin), the agent fetches the blob from the
  nearest storage that has it, checks the hash, and keeps it in its cache.
- Large media later move in verified chunks (BLAKE3/Bao, `iroh-blobs`).

## Sync states (what the file browser shows)

| State | Meaning |
|---|---|
| **Cloud only** | In the tree; its bytes are not on this device. Opening it downloads it. |
| **Downloading** | Being fetched. |
| **Local** | Its bytes are in this device's cache (may be evicted when space is short). |
| **Pinned** | Always kept here ("Make available offline"). |
| **Uploading** | Changed here; its blob is going to storage. |
| **Synced** | Its blob is in at least one storage the placement asks for. |
| **Conflict** | Two devices changed it at once: both versions kept, one renamed. |

## Showing the tree to the operating system

| Platform | Integration | Status in the file browser |
|---|---|---|
| **Linux** | A FUSE file system (`fuser`) mounted at `~/Drive`: every file listed; bytes fetched on first read | Extended attributes (`user.drive.state`), `drive-agent status`, and file-manager extensions (Nautilus, Dolphin) for emblems |
| **macOS** | A File Provider extension (`NSFileProviderReplicatedExtension`, as iCloud Drive and Dropbox use) in a small Swift app around the Rust agent | Finder's own cloud and download badges, "Download now", "Remove download" |
| **Windows** | The Cloud Files API (placeholders and hydration; the `cloud-filter` crate) | Explorer's own status column and overlay icons |
| **Anywhere** | A WebDAV server in the agent: mountable from Finder, Files on iOS, Windows, Linux | None (a plain network drive) |

## Prototype scope

The Linux path first (this is where it can be tried today), with every
piece above it shared by all platforms: a native client, the file tree
and blob store, device pairing, storage registration with OpenDAL (local
folders and S3-compatible), the agent mounting `~/Drive` with on-demand
download and pinning, states as extended attributes and a CLI, and the
Drive app's Files, Devices and Storage views showing the same states live.

## Built (prototype, 2026-11-21)

- `drive-native`: the client for native programs (tokio, websocket,
  copies on disk, reconnect), tested against an in-process node.
- `drive-files`: the tree (a Loro tree; concurrent same-name files both
  kept, listed as `name (2)`), blobs by BLAKE3 over OpenDAL (fs, S3,
  WebDAV, memory; tampered bytes refused), sampled and full hashing, the
  registry (devices, pairing codes, storages, holdings), storage configs
  whose credentials never leave the device. Builds for the browser
  without its OpenDAL half (`default-features = false`).
- `drive-node`: also a storage: `PUT/GET/HEAD /blobs/<hash>`, uploads
  refused unless they match their hash, `?name=` for downloads.
- `drive-agent`:
  - `init`, `invite`, `join --code`, `devices`, `storages`,
    `storage add fs|s3|webdav` (tested with a test blob before it is
    registered), `put`, `ls`, `status`, `pin`, `unpin`;
  - `mount PATH`: the FUSE file system. Every file listed; reading
    downloads (from this device's storages, then the node) and checks the
    hash; writing uploads on close; states in `user.drive.state`, holders
    in `user.drive.holders`; stale mounts cleared; clean unmount on stop.
- The Drive app: Files (browse, upload, download, where each file's bytes
  are, live), Devices (online, what each keeps, pairing codes), Storage.

Tried on THEBATTLESHIP: the space `cody` mounted at `~/Drive` (user
service `drive-agent`), a second device paired by code with its own
storage, a file added there showing as cloud-only here and downloading on
open, a browser upload appearing in `~/Drive`.

## Every computer's files (built, 2026-11-21)

Spacedrive's idea, on Drive: files stay where they are, and every
computer's **index** travels, so you can search and browse what is on
Voyager while it is off.

- **Locations** (`locations` in the registry): a folder a device indexes in
  place (`drive-agent location add Samples ~/Samples`), or a folder in a
  storage it runs (`--storage "Garage Deep"`: an S3 bucket, a WebDAV share,
  walked through OpenDAL, sampled hashes read as byte ranges).
- **Volumes**: each device's disks (sysinfo), with size and free space;
  an unplugged drive keeps its last report and its locations' indexes.
- **The index**: every file and folder, its size, time and sampled BLAKE3
  hash. Ignore rules: `.gitignore`, `.ignore`, `.driveignore`, hidden
  files, caches and build output. Re-walks read only new or changed files.
  Published as one blob (postcard + zstd) and as rows in the node's Turso
  database. A watcher re-indexes a location a few seconds after it
  changes; a full pass runs every ten minutes.
- **Search** (`GET /search`, `drive-agent search`, the app's Computers
  tab): every device's files at once, each with its device, location,
  whether that device is online, and how many copies exist.
- **Fetch** (`GET /fetch/<space>/<location>?path=`): the node asks the
  owning device's agent, over its existing connection, to put the file on
  the node, then serves it. Device off: `503`, "the device that has this
  file is offline".
- **The Computers folder** (`drive-agent mount ~/Drive --computers
  ~/Computers`): `Computers/<device>/<location>/…`, read-only, built from
  the indexes this device keeps a copy of (it lists everything offline).
  Reading this device's files reads them in place; another computer's are
  fetched through the node and cached. `user.drive.state` is `here`,
  `cached`, `remote` or `offline`; `user.drive.copies` lists every location
  holding the same content.

- **Browsing** (docs/browse.md): any folder of any disk of a computer
  that is on, live and read-only, without indexing it; "Index this
  folder" makes it a location.

## Copies and protection (built, 2026-11-21)

- **Copies** are found by content (sampled hash + size) across every
  index: `drive-agent copies LOCATION PATH`, `GET /copies`, the copies
  panel in the app.
- **Sites** (`drive-agent site HOLDER SITE`): where a device or storage
  physically is (`studio`, `home`, `offsite-vault`).
- **Policies** (`drive-agent policy add 3-2-1 --scope Samples`): every file
  in scope should be on at least N **copies** (locations), M **media**
  (distinct disks or storages: two folders on one disk are one medium),
  and K **offsite** (media at another site than the file's own). The
  3-2-1 rule is 3, 2, 1.
- **The report** (`drive-agent protection`, `GET /protection`, the app's
  Protection tab): per policy and location, how many files meet it, the
  bytes at risk, and the largest files that fall short with what they lack.
- **Backups** (`drive-agent backup LOCATION STORAGE`): copy a location
  into a storage (skipping what is there), registered and indexed as a
  location of its own, so its files count as copies. Tried with a real
  Garage bucket (S3) as the offsite store.

Caveat: copies by sampled hash are likely copies. Two takes of the same
length that differ only between the samples would look alike; the next
step confirms a copy with the full hash before the report counts it (and
before any copy is used in place of another).

## What each device keeps: roles (built, 2026-11-21)

- **Storage servers** (`drive-agent role storage-server`: the NAS, an
  always-on box with big disks) keep as much as they can: every file's
  bytes in the tree (smallest first, keeping a tenth of the disk free),
  never letting any go, and a **mirror** of every other computer's
  locations, copied while that computer is on and registered as a
  location of its own (`Projects (mirror)`), so its files count as copies.
- **Workstations** (`role workstation`, the default: Battleship, Voyager,
  airlock) keep what they are working on: the files under their **active
  folders** (`drive-agent active add Songs/Song A`) and their pins.
  Anything else they opened goes three days after its last use, and only
  once a storage or storage server holds it: a workstation never drops the
  last copy.
- The agent runs a placement pass every minute (`drive-agent place` runs
  one now); the Drive app's Devices view shows each device's role and
  active folders.

## Apps on Drive's catalog

Drive stays the data platform; apps build on its catalog:

- **A sample library catalogue** (an Atlas-style app): reads every
  device's indexes (`/search`, `/list`, extension filters), keeps its own
  data in its own Drive documents (`samples:<space>`: tags, BPM, key,
  favourites, which instrument a sample belongs to, analysis results),
  and manages big libraries (orchestral, keys) as units: where each
  library is installed, how many copies, which devices should have it.
  Drive gives it the files, copies, protection and fetch; the app owns the
  musical meaning.
- **iPhone and iPad** are devices too. An iOS agent (the app, with a File
  Provider extension for the Files app) indexes the app's own containers
  and the folders the user grants it (security-scoped bookmarks: iOS lets
  an app see nothing else), reports its free space, and can receive a
  library: "put this Keys library on the iPad" is a placement the agent
  carries out by downloading the library's files into its container. Same
  registry, same index, same protection report.

## Next

- **File-manager status:** a Nautilus extension (nautilus-python) and a
  Dolphin overlay plugin reading `user.drive.state` for emblems and a
  column; "Make available offline" in the context menu (pin/unpin).
- **macOS:** the File Provider extension (Swift, around the agent), so
  Finder shows cloud and download badges natively.
- **Windows:** the Cloud Files API (`cloud-filter`).
- Large files in chunks (BLAKE3/Bao, `iroh-blobs`) and streaming reads
  instead of whole-blob downloads; eviction when the cache is full;
  placement rules; conflicts (both versions kept); signed device keys.
- The agent as a fleet service (it runs as a transient user unit now).
- Confirm copies with the full hash; read an offline file from a
  confirmed copy on an online device.
- Placement: "this library on these devices" as a rule the agents carry
  out, and the protection report suggesting backups that would fix it.
- The iOS/iPadOS agent and File Provider extension.

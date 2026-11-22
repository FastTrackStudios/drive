# Browsing every computer's disks

Any computer in an org, and every disk plugged into it, browsed live from
the Drive app in any browser, without indexing it first: walk its folders,
see thumbnails and waveforms, Quick Look a file, download it, and with one
click make a folder a location ("Index this folder"). Written 2026-11-21.

Locations ([files.md](files.md)) are folders a device indexes so they can
be searched and counted as copies while it is off. Browsing is the other
half: anything on the disk, now, read-only, while the device is on.

## In the app

- **The sidebar** lists, under each computer, its locations and then its
  **volumes**: the home folder, the system disk (`/`) and every disk it
  has mounted (internal and plugged in), each with an icon (home, system,
  plugged-in drive, other disk) and a bar of how full it is (yellow past
  90 %).
- **A volume** opens in the main area with the browser's own grid or list,
  breadcrumbs, sorting, the inspector, Quick Look (Space; arrows walk),
  thumbnails and waveforms, and Download. Folders of more than 500 entries
  show a page at a time ("Show 500 more"). Dot files and system folders
  are hidden unless "Hidden" is ticked.
- **Index this folder** (the folder's header, or the inspector for a
  picked folder) asks the device to add the folder as a location, as
  `drive location add` does, and index it; "Open location" goes to
  it.
- **Offline:** the header says "Voyager is offline · last seen". Folders
  seen before still list (from the node's memory of them) and their
  thumbnails already made still show; a folder never seen says the
  computer is offline. Files cannot be read and nothing can be indexed
  until it is back.

Code: `apps/drive/src/browse.rs` (`DeviceVolumes`, `VolumeFolder`,
`InspectorActions`), hooked into the browser as `Place::Volume` and
`Src::Browse`; styles in `assets/browse.css`.

## The protocol

One request and one answer added to `drive-proto` (no version bump: an
agent from before skips a message it cannot decode, and the node times
out with "its agent may be older than browsing"):

```text
ToClient::Browse  { req, ask: Browse }
ToNode::Browsed   { req, result: Result<Browsed, String> }

Browse::Roots                                      -> Browsed::Roots(Vec<Root>)
Browse::List     { root, path, offset, limit, hidden } -> Browsed::Listing
Browse::Read     { root, path, start, len }        -> Browsed::Chunk (len 0: size and time only)
Browse::Key      { root, path }                    -> Browsed::Key (the sampled content key)
Browse::Sidecars { root, path, kinds, force }      -> Browsed::Key (made and put on the node)
Browse::Index    { root, path, name }              -> Browsed::Key (the new location's id)
```

`root` is a root's mount point exactly as `Roots` named it; `path` is
relative to it. A listing is folders first, then by name (case folded),
at most `LIST_MAX` (2000) entries a page; only the page's entries are
stat'ed, so a folder of 100 000 files costs one directory read. A read is
at most `READ_MAX` (4 MiB).

## The node's routes

All under `/browse/{space}/{device}/`:

| Route | What |
|---|---|
| `GET roots` | `{online, device_name, roots: [{mount, name, fs, total, free, removable, home}]}`; offline: the last roots seen, else the registry's volumes |
| `GET list?root=&path=&offset=&limit=&hidden=1` | `{online, path, entries: [{name, dir, size, mtime, link}], total, offset, hidden}`; offline: the last answer for that page with `as_of` and `why`, else `503 {"state": "offline", "why": "Voyager is offline"}` |
| `GET file?root=&path=&name=` | the bytes, streamed 4 MiB at a time from the device; `Range` honoured (`206`, `Content-Range`), so audio and video seek. Always `attachment`, `Content-Security-Policy: sandbox`, `nosniff`: a browsed HTML file never runs on the node's origin |
| `GET sidecar/{kind}?root=&path=&size=&mtime=` | a thumbnail or peaks; see below |
| `POST index` `{root, path, name}` | `{location}`; `409` when the folder already is one |

Errors carry `{"state", "why"}`: `offline` (503), `timeout` (504),
`refused` (403: outside what the device lets be browsed), `missing` (404).

### Sidecars of a browsed file

Made by the device, kept by the node exactly like a location's
([sidecars.md](sidecars.md)): keyed by the file's sampled content key, so
a browsed file and the same file in a location or the space's tree share
one thumbnail. The node remembers each browsed file's key by (device,
root, path, size, mtime) in `data/browse.db`, so a thumbnail already made
is served without asking the device at all, and still while it is off.
Otherwise the device says the key (`Browse::Key`: three 64 KiB reads), the
node serves the sidecar if it has it, and else asks the device to make it
(`Browse::Sidecars`, the agent's low-priority producer).

## Who may browse

Only signed-in **members of the device's own org**: the issuer's
membership, checked by the browse routes' own guard (`browse::guard`).
People of another org that this one shared a project with may open that
project, not the disks of its computers. Without an issuer (a development
node) everyone may, as for everything else. Every refusal logs one
`tracing::warn` (`auth.outcome = unauthenticated | denied`, or
`browse.outcome = refused` for a path the device refused).

## Read-only and confined (the agent)

`crates/drive-agent/src/browse.rs` answers every request; two rules hold:

- **Confined.** The root must be one of the device's roots right now (its
  volumes' mount points, `/`, the home folder). The path may hold only
  plain names (`..` and absolute paths are refused outright); it is then
  resolved on disk (`canonicalize`, which follows symlinks) and refused
  unless it is still inside the root. A symlink pointing inside the root
  is followed; one pointing out is refused. `/proc`, `/sys`, `/dev` and the
  agent's own data folder (its config holds its sign-in) are never listed
  or read, whichever root reaches them; only regular files are read.
- **Read-only.** Files are opened with `File::open` (read access only).
  Nothing in the module creates, writes, renames, deletes, or changes the
  permissions or times of anything. "Index this folder" writes only the
  registry (a new location) and the index on the node, as `location add`
  does; walking reads. Sidecars are made from a read of the file and put
  on the node, never next to the file.

Hidden by default (shown with `hidden=1`): names starting with `.` (so
`.Spotlight-V100`, `.fseventsd`, `.Trashes`, `.DS_Store`), `System Volume
Information`, `$RECYCLE.BIN`, `lost+found`, `Thumbs.db`, `desktop.ini`.

## Tests

| Test | Holds |
|---|---|
| `drive-agent` `browse::tests::paths_stay_inside_their_volume` | other roots, `..` however spelled, symlinks out, the agent's own folder: refused; a symlink inside: followed |
| `browse::tests::browsing_changes_nothing` | every operation on a tree made read-only (files 0444, folders 0555) succeeds, and the tree (names, sizes, times, bytes) is the same after |
| `browse::tests::nothing_here_writes` | the module's source holds no writing call (`fs::write`, `OpenOptions`, `remove_*`, `rename`, `set_permissions`, …) |
| `browse::tests::listing_pages_and_hides` | folders first, hidden names left out and counted, 2500 entries in pages of 2000 |
| `browse::tests::reads_ranges_of_files_only` | byte ranges; folders refused |
| `drive-node` `browse::tests::only_members_of_the_devices_org_browse` | no token or a bad one: 401; a member of another org: 403; a member: through |
| `drive-native` `tests/browse.rs` | a node and a device in one process: roots, pages, refusals, a whole file streamed and a range, Index, then the device gone: the folder seen still lists (offline), one never seen is 503, a file is 503 |
| `drive-proto` `new_messages_leave_old_ones_alone` | an old agent's decoder still reads every old message and fails only on the new one |

## Next

- Byte ranges stream from the device a chunk at a time; a whole file is
  not kept on the node. A device with an iroh endpoint now serves them
  over iroh-blobs instead, verified, and `/fetch` too
  ([transfer.md](transfer.md)).
- Search inside a browsed folder (the device walking on request), and
  thumbnails for a whole folder ahead of scrolling.
- Watching a browsed folder live (the agent's watcher, a subscription).

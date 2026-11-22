# Files for apps: any Drive file as a local path

How an app (first: Studio's audio engine) gets any Drive file as a real
local path at disk speed, wherever the file lives (#48). Written
2026-11-22.

## In short

- **The running agent** (`drive serve` or `drive mount`) serves a local app
  API, **loopback only**: a Unix socket in its folder (`app.sock`, mode
  600; a program of another user is refused), or a `127.0.0.1` port where
  there are no Unix sockets, where asked for (`apps.port`), or when the
  agent's folder is too deep for a socket path. The port's number and a
  token are in `app.port` (mode 600), and a connection must give the
  token first. A non-loopback address is never bound, and a non-loopback
  peer is refused.
- **Apps link `drive-local`**, a small blocking client (std, serde and
  serde_json only; no async runtime). It finds the agent by its folder
  (`DRIVE_AGENT_DIR`, else the agent's default). When no agent answers, it
  fails at once with `Error::NotRunning` ("start it with `drive serve`").
- **`open(target)`** gives `LocalFile { path, size, hash, how }`:

  | Where the file is | The path | `how` |
  |---|---|---|
  | one of this device's locations, or a path on its disks | the file itself, never copied. Read it, never write it: it is the owner's file. | `local` |
  | the Drive tree, its bytes in the agent's cache | `<agent>/cache/<blake3>`, the copy the mount and `drive status PATH` see | `cached` |
  | the Drive tree, not here | fetched into that cache: from a device that keeps it (iroh), else the node (HTTP, by range), else one of this device's storages | `fetched` |
  | another computer | that computer makes it fetchable, and it comes over iroh-blobs into the app cache (`<agent>/app-cache/objects/…`) | `fetched`, then `cached` |
  | another computer that is off | the copy fetched before, if any | `cached` |

  The mount is never the answer. It is FUSE without passthrough, so
  reading through it is not disk speed. A cached file is the same bytes at
  a plain path.
- **Targets** are the names the MCP tools take: `drive:PATH`,
  `LOCATION:PATH`, `content:HASH` (a full BLAKE3, or the sampled hash an
  index has), `file:<tree id>`, an absolute path here, and `DEVICE:/path`
  (a browsed file of another computer).
- **Fetches** are verified (each 16 KiB group against the BLAKE3 tree as
  it comes, then the whole hash) and **resume**. A fetch writes
  `partial/<hash>.part` and becomes a cache entry only once every byte has
  come and the hash checks out, so a partial file is never handed out. A
  fetch that stopped (the app cancelled, the agent was killed, the network
  went) continues from the partial's length the next time anyone asks.
- **Leases.** Whatever an app opens or warms is pinned while its
  connection lives (or until it releases it). The app cache's eviction
  and the placement pass never let go of a pinned file. `drive status`
  and `drive pins` show what is pinned, by which app, and why.

Code: `crates/drive-local` (the client and the wire types) and
`crates/drive-agent/src/apps/` (the server: `resolve.rs`, `cache.rs`,
`leases.rs`, `peer.rs`, `warm.rs`, `watch.rs`).

## The API

```rust
let drive = drive_local::Client::connect("studio")?;        // or connect_in(dir, app)

drive.open("Samples:Kits/Acoustic/kick.wav")?;               // LocalFile
drive.open_with(target, OpenOptions { lease: true, hash: true })?;
drive.resolve_many(&media_list)?;                            // Vec<Resolved>, in order

let job = drive.warm(Warm {
    project: Some("Amazing Grace".into()),                   // or targets, folder
    kinds: vec!["audio".into()],                             // Drive's kinds: audio, video, image, …
    priority: Priority::High,                                // 1, 3 or 8 files at a time
    ..Default::default()
})?;
job.started;                                                 // files, bytes, how many already here
while let Some(ev) = job.recv() { /* Event::Progress(p) … Event::Done(p) */ }
job.cancel()?;

let w = drive.watch("drive:Songs/Amazing Grace/Take 3.wav")?;
w.recv();                                                    // Event::Changed { size, hash } | Event::Gone

drive.release(target)?; drive.release_all()?;
drive.pins()?; drive.status()?;
```

`Progress` has `files`, `files_done`, `files_failed`, `bytes`, `bytes_done`,
`rate` (bytes/s), `eta_ms`, and, when the event is about one file, `file`:
its target, its state (`queued`, `fetching`, `ready`, `failed`,
`cancelled`), its bytes, and the `LocalFile` once it is ready. Bytes move
in an event at most every 250 ms. A file's state changes send one event
each.

`hash` is always set for cached and fetched files. For a file opened in
place it is set only when `hash: true` is asked for. That costs one read
of the file, the first time per size and time. Without it, opening a file
of this device's own reads no byte of it.

### The wire

One JSON object per line, both ways (`drive_local::proto`). Requests carry
an `id` chosen by the app. Each gets one answer, `{"id":N,"ok":…}` or
`{"id":N,"err":"…"}`. A warm job's and a watch's events carry the
request's id (`job`, `watch`), and a job's answer always comes before its
first event. The first request must be `hello` (the app's name, and the
token on a port). Anything else first closes the connection.

```text
{"id":1,"op":"hello","app":"studio"}
{"id":2,"op":"open","target":"Samples:Kits/kick.wav"}
{"id":3,"op":"warm","project":"Amazing Grace","kinds":["audio"],"priority":"high"}
{"event":"progress","job":3,"files":3,"files_done":1,"bytes":3090000,"bytes_done":1200000,"rate":…,"eta_ms":…,"file":{…}}
{"event":"done","job":3,…}
```

## For an audio engine

```rust
use drive_local::{Client, Event, Priority, Warm};

/// Load a session: every clip's media as a path the engine can mmap.
fn load_session(clips: &[String]) -> Result<Vec<std::path::PathBuf>, drive_local::Error> {
    // One connection for the session's life: its leases keep the media
    // from being evicted while the session is open.
    let drive = Client::connect("studio-engine")?;

    // Everything already here answers at once (in place or cached). The
    // rest are fetched before the answer, 16 files at a time.
    let files = drive.resolve_many(clips)?;
    let mut paths = Vec::new();
    for r in files {
        match r.file {
            Some(f) => {
                // How::Local: the owner's own file. Read it, never write it.
                paths.push(f.path);
            }
            None => return Err(drive_local::Error::Failed(format!(
                "{}: {}", r.target, r.error.unwrap_or_default()
            ))),
        }
    }
    Ok(paths)
}

/// Before a gig: bring a whole setlist's audio here, with a progress bar.
fn prepare(drive: &Client, project: &str) -> Result<(), drive_local::Error> {
    let job = drive.warm(Warm {
        project: Some(project.into()),
        kinds: vec!["audio".into()],
        priority: Priority::High,
        ..Default::default()
    })?;
    while let Some(ev) = job.recv() {
        match ev {
            Event::Progress(p) => draw_bar(p.bytes_done, p.bytes, p.eta_ms),
            Event::Done(p) if p.files_failed > 0 => warn_missing(p.files_failed),
            Event::Done(_) => break,
            _ => {}
        }
    }
    Ok(())
}

/// Reload a clip when another computer records a new version of it.
fn follow(drive: &Client, target: &str, reload: impl Fn(&std::path::Path)) -> Result<(), drive_local::Error> {
    let w = drive.watch(target)?;
    while let Some(Event::Changed { .. }) = w.recv() {
        // A new version is a new path (cached content is named by its
        // hash) or the same file in place: open again either way.
        let f = drive.open(target)?;
        reload(&f.path);
    }
    Ok(())
}
```

`crates/drive-local/examples/engine.rs` is a runnable version that also
times each step (`cargo run -p drive-local --example engine -- --dir
AGENT_DIR TARGET…`).

## The app cache

- In the agent's folder: `<agent>/app-cache/objects/<h[..2]>/<h>`, named by
  the full BLAKE3. Its journal is `index.jsonl`, compacted when it opens.
  Fetches in progress are in `partial/`.
- **Budget:** 100 GiB unless set (`drive apps budget GIB`, or
  `apps.budget_gib` in `config.json`; read when the agent starts). Before
  a fetch, room is made for it. After it, the least recently used entries
  that no app holds are let go until the cache is at 90 % of its budget.
  When everything left is held, the cache stays over its budget and says
  so in one `warn`, rather than evict a held file.
- **Names.** It also remembers what each `LOCATION:PATH` and
  `DEVICE:/path` was when it was fetched (hash, size, time). That is how a
  file of a computer that is off still opens from the cache.
- **Derived data only.** This is the one place, with the kache store, where
  the agent deletes files on its own. Only plain files under `objects/`
  and `partial/` of that folder are deleted, and every removal goes through
  one function that checks this (a symlink is never followed). Partials
  untouched for a week go when the cache opens. A location's files and the
  user's files are never touched.
- **Drive tree files** stay in the agent's existing cache
  (`<agent>/cache/<hash>`), which the mount, pins and the placement pass
  already manage. Leases make placement skip them.

## Between computers

Each agent's iroh endpoint speaks one more protocol, `drive/apps/1`, behind
the same door as its files: only the space's devices and nodes get in at
the handshake. One JSON request per stream:

```text
{"provide_location":{"location":"<id>","path":"Kits/kick.wav"}}
{"provide_path":{"path":"/Volumes/Samples/kick.wav"}}
{"blob":"<blake3>"}
  -> {"hash":"<blake3>","size":N,"mtime":MS} | {"err":"…"}
```

An answer means the file is fetchable from that device now, as an
iroh-blobs blob named by its full BLAKE3, served read-only in place
([transfer.md](transfer.md)). Each request is confined as the node's are:

- a location's file stays inside its folder;
- a browsed file stays inside the root it is under (the device's disks
  and home; never `/proc`, `/sys`, `/dev` or the agent's own folder);
- a blob comes only from the agent's own caches.

The first ask for a file hashes it there, once per size and time.

## Watching

- A file of this device's own: its size and time, checked every second.
- A Drive tree file: its hash in the tree, every second.
- Another computer's file: that computer is asked every 5 s. It hashes
  the file again only when its size or time changed.

Each new version sends `Event::Changed { size, hash }`. A file that is gone
sends `Event::Gone`. A computer that is off sends nothing until it is back.

## The command line

```bash
drive open TARGET [--hash]       # prints the path; how, size, hash and time on stderr
drive warm TARGET… [--project NAME] [--kind audio]… [--priority low|normal|high]
drive pins                       # what apps hold, by app, and why
drive status                     # the app cache: files, bytes, budget, partials, apps, pins
drive apps budget GIB
```

The command line's own opens and warms hold nothing once it exits: what
came stays in the cache, unpinned.

## Tests

| Test | Holds |
|---|---|
| `apps::tests::open_resolves_each_kind_of_source` | a node and two agents in one process, over iroh. An own location file opens in place (no copy; hash on request). An absolute path and `DEVICE:/path` here are local. Another computer's location file is fetched into the app cache, then cached. `content:` works by full and by sampled hash. A browsed file of another computer opens when it is under one of its roots and is refused otherwise. A tree file comes from the node, and a tree file only another device keeps comes from that device. With that device off, the cached copy still opens, and an uncached file says it cannot be reached. |
| `a_dropped_fetch_resumes_and_partials_are_never_handed_out` | a 48 MB fetch aborted part way leaves a partial and no cache entry. A right partial resumes from its exact length. A wrong partial is caught by the hash and let go, and the next ask succeeds. |
| `pinned_files_are_not_evicted_and_leases_end_with_the_app` | through the socket: an app's open is pinned (shown in `pins` and `status`). The budget is shrunk to nothing: the held file stays and an unheld one goes. The app disconnects, its lease ends, and the file can go. `release` works without disconnecting. |
| `only_this_user_on_loopback_gets_in` | non-loopback binds are refused. Another uid is refused. The socket is 0600 and a second agent does not take it over. On the port, the right token gets in and a wrong one is refused. |
| `location_files_are_never_written` | location files made 0444 in 0555 folders. Open with hash, `resolve_many`, warm, watch, and another computer fetching them all leave every name, byte, time and mode as it was, and nothing is copied into the cache. |
| `warming_a_project_streams_progress_and_watches_see_new_versions` | a project's audio (3 `.wav`, not the note) warmed from the other computer at high priority: fetching and ready events per file, totals, done. Warmed again, all are here and held. A local file's new version and a tree file's new hash reach their watchers. |
| `apps::cache::tests::*`, `apps::leases::tests::*` | LRU skips pinned entries; over budget rather than evict held ones; removals outside the cache refused; a planted symlink not followed; names and partials survive a restart; leases end by release or disconnect. |
| `drive-local` `no_agent_is_a_clear_error`, `proto::tests::lines_round_trip` | no agent (or a stale socket) is `NotRunning`; the wire's shapes. |

## Measured (a workstation, 2026-11-22)

Release builds. A scratch node on its own port, and two agents with their
own folders over scratch data, joined by pairing code. They talked over
iroh with direct addresses (`DRIVE_IROH_LOCAL=1`, loopback). Their folders
were deep enough that apps reached them on the loopback port, not the
socket.

| | |
|---|---|
| connect (hello answered) | 0.24–0.97 ms |
| `open`, this device's own file (mean of 1000 on one connection) | 29 µs |
| `open`, another computer's file already cached (mean of 1000) | 174 µs: one iroh round trip, asking that computer for the file's current version |
| `resolve_many`, 4 files all here | 0.25 ms (own) / 0.41 ms (cached) |
| `drive open` of a cached or own file, the whole command | 4–6 ms (mostly starting the process) |
| fetch of a 1 GiB file from the second agent (first ask: hashed there, then moved, verified) | 2.37 s (≈450 MB/s) |
| the same after `kill -9` of the fetching agent at 312 MB, restarted, opened again | 1.51 s, resumed at 311 984 128 bytes, identical bytes |
| warm 40 × 2 MB files from the second agent | 0.27 s |
| a watched file of the other computer changed | event within one 5 s poll |

## Next

- A cached file of another computer is checked with that computer on
  every open (174 µs). When its index is fresh and its size and time
  match, the cached copy could answer without asking.
- A location in a storage (a bucket), and agents older than this protocol:
  fetch through the node's `/fetch` (by range) instead of refusing.
- A copy of the same content on this device (another location, by
  sampled hash, confirmed by full hash) used in place of a fetch.
- Warm priorities across jobs (a high job first), not only within one.
- The mount with FUSE passthrough (Linux 6.9+) for cached files, so the
  mount's own paths are disk speed too.
- Apps on iOS and in a browser (no socket there): the in-process agent's
  API directly.

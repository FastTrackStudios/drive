# Moving file bytes: iroh-blobs

How a file's bytes get from the computer that has it to the node, and from
the node to a browser, without being uploaded whole first (#26). Written
2026-11-21.

## In short

- **Every agent runs an iroh endpoint** that serves files of its locations
  and of its browsed volumes as BLAKE3-verified blobs (iroh-blobs),
  **read-only and in place**: nothing is copied. The full BLAKE3 hash and
  its outboard (the hash tree that lets any range be checked) are made the
  first time a file is asked for and kept in the agent's own folder,
  remembered by (path, size, mtime), so a changed file is hashed again. The
  sampled hash stays the quick identity (indexes, copies, sidecars).
- **The node fetches exactly the byte ranges** an HTTP client asks for,
  verified chunk by chunk, and streams them as they come: `/fetch`,
  `/browse/.../file`, Quick Look's audio and video seeking, downloads,
  share links, the zip. The browser keeps plain HTTP with `Range`.
- **Only the org's own node and devices may connect** (their endpoint ids
  in the space registry); anyone else is refused at the handshake.
- **Connections are direct when they can be** (same LAN or tailnet: iroh
  tries every address an endpoint has) and go through a relay otherwise:
  n0's public relays for now, our own later (a setting).
- **Agents from before** (no endpoint in the registry) keep the old paths.
- Document sync (Loro) stays on the existing websocket.
- The endpoint also answers kache build-cache lookups from the device's
  build group (ALPN `drive/kache/1`, behind the same door;
  [kache.md](kache.md)).

Code: `crates/drive-transfer` (the endpoint, the provider, the fetcher),
`crates/drive-agent/src/transfer.rs`, `crates/drive-node/src/transfer.rs`.

## What we checked before building (2026-11-21)

| | |
|---|---|
| Versions | `iroh` 1.5.0, `iroh-blobs` 0.105.0 (its `fs-store` feature), `bao-tree` 0.17. Architect pins `iroh = "1"` (1.1.0 in its lock): the same major, no clash in the fleet. |
| In-place serving | Yes: `ImportMode::TryReference` makes an *external* entry. The store opens the file read-only (`OpenOptions::new().read(true)`) to compute the outboard into its own folder, then serves ranges from the file where it is. The store's deletes only touch files under its own folder; it never renames or writes an external file (only `export`, which Drive never calls, moves files, and only owned ones). Files at or below 16 KiB are copied into the store's database instead: still nothing written to the file. |
| Ranges | `GetRequest::blob_ranges(hash, ChunkRanges)`: any set of 1 KiB chunks, sent in 16 KiB chunk groups with the hash-tree nodes needed to check them. |
| Resumption | Not a feature of a single request; a request for the remaining range is. `Fetcher` re-dials and asks for the rest from the last verified byte it delivered (5 tries without progress). |
| Writes from peers | Push requests are disabled by default (`EventMask::DEFAULT`); observe requests too. |
| Relays | `presets::N0`: n0's production relays and its DNS/pkarr address lookup. `RelayMode::custom([url])` for our own; `iroh-relay` (same version) is the server to run on the VPS later. |
| wasm | iroh runs in browsers (relay-only, no direct UDP); the iroh-blobs fs store does not (native only). Browsers keep HTTP from the node. |
| Licence | MIT OR Apache-2.0 (iroh, iroh-blobs, iroh-relay, bao-tree): compatible with Drive's GPL-3.0-or-later. |
| Size | Release builds, stripped: `drive-agent` 39.0 → 54.4 MB (+15.4), `drive-node` 58.4 → 71.2 MB (+12.8): QUIC (noq), rustls with ring, redb, the relay client, portmapper, metrics, DNS and pkarr lookup. |

## The registry

Next to each device's entry in `drive:space:<space>` (`devices.<id>`):

| Key | What |
|---|---|
| `iroh` | the device's endpoint id |
| `iroh_addr` | its address as it last said it (JSON: id, relay URL, direct addresses), refreshed when it changes |

And `nodes`: endpoint id → when that node last said it keeps the space. A
node writes itself there when it starts (every space it keeps) and when an
agent connects. An agent lets in exactly the ids in `nodes` and in its
devices' `iroh` (`drive_files::space::allowed_endpoints`); everyone else is
refused with one `warn` (`transfer.outcome = "refused"`, the peer's short
id). Nothing about a device's own entry changed shape: readers from before
skip the two new keys.

## The protocol

Two requests added at the end of `drive_proto::Browse`, one answer added
at the end of `Browsed` (no version bump, as browsing did):

```text
Browse::Provide          { root, path }      -> Browsed::Provided { hash, size, mtime, endpoint }
Browse::ProvideLocation  { location, path }  -> Browsed::Provided
```

The agent confines both as browsing does (`browse::Rules`): the root is
one of its volumes, `/` or home (`Provide`), or the location's folder
(`ProvideLocation`); plain names only, resolved on disk and refused if they
leave it; `/proc`, `/sys`, `/dev` and the agent's own folder never. A
location in a storage (a bucket) answers "in a storage" and the node takes
the old path for it. `hash` is the file's full BLAKE3, which is also its
blob name on the node, so a file the node already keeps is served from
there without any transfer. `endpoint` is the agent's address now.

The node sends these only to devices whose registry entry has `iroh`. An
agent from before never sees them (it would skip a message it cannot
decode, and the node would wait).

## On the node

| Route | Device with an endpoint | Device without |
|---|---|---|
| `/browse/{space}/{device}/file` | the range from the device over iroh; nothing kept | 4 MiB reads over the agent's websocket (as before) |
| `/fetch/{space}/{location}` | kept blob if the node has it, else the range over iroh; a whole-file answer is written to the blob store as it passes, a range starts a background fetch of the whole file | the agent uploads the whole file, then it is served (as before, now with `Range`) |
| `/s/{space}/{token}/file` | as `/fetch` | as before |
| `/s/{space}/{token}/zip` | each file streamed into the zip as it comes, and kept | as before |

A blob is written under a temporary key and renamed to its own only once
every byte came and the hash checks out (`BlobStore::writer`): a client
that leaves early never leaves half a blob, and the rest is fetched in the
background. A range of a kept blob is read from the node's disk as it is
(`BlobStore::read_range`), not by reading and checking the whole blob each
time: blobs are checked as they are kept. Each transfer logs one event
when it ends (`transfer.path` direct or relay, `transfer.bytes`,
`transfer.first_byte_ms`, `transfer.ms`).

## Settings

| Where | Setting | What |
|---|---|---|
| node | `--relay URL` | our own relay instead of n0's |
| node | `--iroh-local` | no relay and no address lookup: direct addresses only |
| node | `--no-iroh` | no endpoint: every file the old way |
| agent | `relay` in `config.json`, or `DRIVE_RELAY` | our own relay |
| agent | `DRIVE_IROH_LOCAL=1` | direct addresses only |

The agent's endpoint key is `transfer/iroh.key` in its folder, the node's
`data/iroh.key`; the agent's blob store (outboards, the list of what was
hashed) is `transfer/` in its folder.

## Measured (a workstation, 2026-11-21)

A scratch node and agent on the same machine (a direct connection over
loopback), release builds, the files in the page cache. A 2-hour WAV
(48 kHz, 24-bit, stereo: 2.07 GB) and a sample library (4000 files, 40
folders, 1.29 GB). The node's blob store emptied before each run.

| | Old path | iroh, first ask (hashing) | iroh, hash known |
|---|---|---|---|
| 2-hour WAV, `/fetch` whole: first byte | 27.5 s | 0.65 s | 0.025 s |
| 2-hour WAV, `/fetch` whole: total | 27.8 s (75 MB/s) | 3.1 s (676 MB/s) | 2.6 s (813 MB/s) |
| 2-hour WAV, `/fetch` 1 MB at 1 GB (a seek) | 9.4 s | 2 ms | 2 ms |
| the same, seeking again at 1.5 GB | 8.5 s (uploaded whole again) | 2 ms | 4 ms |
| 2-hour WAV, `/browse` whole | 3.8 s (549 MB/s; first byte 23 ms) | 2.4 s (866 MB/s) | 2.6 s (792 MB/s) |
| 2-hour WAV, `/browse` 1 MB at 1 GB | 3 ms | 2 ms | 2 ms |
| Library, every file by `/fetch`, one at a time | 14.6 s (88 MB/s; first byte median 3.6 ms) | 7.4 s (175 MB/s; 1.3 ms) | 6.2–6.5 s (196–206 MB/s; 1.0–1.1 ms) |

Hashing the 2.07 GB file took 0.64 s from the page cache; from a cold disk
it is the disk's read time. Dialing through n0's relay only (the direct
addresses left out of the address dialled) reached the first byte of a
32 MB file in 224 ms, and iroh then moved the connection to a direct path
on its own (`through_n0s_relay`, an ignored test: it needs the internet).
Between machines the numbers will be the network's: direct over the LAN or
tailnet when possible, through the relay otherwise.

## Tests

| Test | Holds |
|---|---|
| `drive-transfer` `ranges_are_exactly_the_files_bytes` | ranges (one byte, across chunk groups, past the end, the whole file) are the file's bytes; the hash is the file's BLAKE3; a second ask does not hash again |
| `a_changed_file_is_hashed_again` | a new size or time: a new hash, the new bytes |
| `strangers_are_refused` | an endpoint not allowed gets nothing; an allowed one does |
| `a_dropped_connection_resumes_where_it_stopped` | a 48 MB range, the connection closed twice mid-way: every byte arrives once, in order |
| `serving_changes_nothing` | files made read-only (0444 in 0555 folders) served whole and in ranges; names, sizes, times and bytes the same after |
| `provider_never_writes` | the provider's source holds no writing call, no `ImportMode::Copy`, no `export`; pushes stay disabled |
| `through_n0s_relay` (ignored) | dialing through n0's relay works |
| `drive-agent` `transfer::tests::location_files_stay_inside_their_folder` | `..`, absolute paths, symlinks out, another root and the agent's folder refused |
| `drive-files` `endpoints_beside_devices_and_nodes`, `blobs_written_as_they_arrive`, `ranges_and_sizes` | the registry keys; a blob kept only whole and matching; ranged reads |
| `drive-proto` `provide_leaves_browsing_alone` | the old `Browse` decoder reads every old request and fails only on the new ones |
| `drive-native` `tests/transfer.rs` | a node and a device in one process: browsed ranges (and 416), a location's range streamed, the whole file kept on the node and then served from there; a root outside refused |

## Next

- Our own relay (`iroh-relay`), and its URL in every agent's config and
  the node's flags (#35).
- Smaller binaries: iroh without metrics and portmapper, if iroh-blobs
  lets its iroh features be trimmed (#36).
- Devices fetching from each other directly (a laptop opening a file on
  the NAS), storage servers' mirrors by range, and the tree's own blobs
  over iroh-blobs too (#37).
- Re-hashing a file served under a stale hash, compacting the list of
  hashed files, hashing media ahead of the first ask, streaming whole
  kept blobs (#38).

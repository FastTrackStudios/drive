# Sidecars

Data derived from a file's content — thumbnails and waveform peaks for
now; transcripts, OCR text, PDF text and EXIF later — made once per
content, kept as blobs, and found again by that content wherever the file
goes: moved, renamed, copied, or on another computer. Written 2026-11-21.

## The key

A sidecar is keyed by **(content key, kind, producer version)**.

| Part | What it is |
|---|---|
| **Content key** | The file's full BLAKE3 hash (a file of a space's tree, or a blob the node keeps), or `s` + its **sampled** hash (a computer's file, which only its location's index knows; the sampled hash covers the size: `drive_files::hash::sampled`). A producer that reads the whole file records the sidecar under both keys, so a computer's file and the same bytes in the tree share one sidecar. |
| **Kind** | `thumb-256`, `thumb-1024`, `peaks` (`drive_sidecars::Kind`). |
| **Version** | The producer's version for that kind (`Kind::version`). A sidecar made by an older producer is stale: it is made again when next asked for (and served meanwhile only when making fails), and `purge --stale` drops them. |

## Where they are kept: a table on the node

The node keeps the list in a Turso database of its own, `data/sidecars.db`
(one row per content key and kind: version, blob, MIME type, size, when it
was made and last read; plus `sidecar_misses`, what no producer could
read), and the bytes as blobs in `data/sidecars/` — a folder apart from the
files' blobs, so purging can never remove a file.

Why a table, not a Loro document per space:

- **It is a cache.** Sidecars come and go (purges, new producer versions,
  read times). A CRDT keeps the history of every change, and every
  browser that opens the space would sync it; a table on the node forgets.
- **The lookups are the node's.** "Is there a thumbnail for this key?",
  "which of these 500 keys lack peaks?", "the least recently read until
  2 GB is free" are queries, answered where the blobs are.
- **Content is not a space's.** The same bytes in two orgs are one sidecar.
  Reading a sidecar needs its content key (64 hex characters), as reading
  a blob does; asking a device to make one needs membership in its space.

## Producers run where the bytes are

| Bytes | Producer |
|---|---|
| A blob the node keeps (a file of a tree, a computer's file kept available) | the node (`drive-node`'s `sidecars` module), at most two at once |
| A file in a location of a computer | that computer's agent, asked by the node over the agent's connection (`ToClient::Sidecars`), on a low-priority thread (nice 10); it uploads with `PUT /sidecar/…` |
| A file in a storage location (a bucket) | its agent, downloading it first (up to 256 MB; not in the background) |

The producers themselves are `drive-sidecars` with its `produce` feature
(native only; the browser uses the crate without features, for the keys
and the peaks format).

| Files | Thumbnail | Notes |
|---|---|---|
| PNG, JPEG, GIF, WebP, BMP, TIFF | the image, turned upright by its EXIF orientation | `image` crate; decoded whole, 1 GB allocation cap, files up to 256 MB |
| HEIC / HEIF | none (no generator) | no pure-Rust HEVC decoder is good enough yet; libheif is a C library |
| PDF | the first page on white | `pdfcraft-render` (the pinned pdfcraft revision drive-view uses) |
| Video (MP4, MOV, MKV, WebM, …) | a frame near the middle | `ffmpeg`/`ffprobe` when the computer has them (`$DRIVE_FFMPEG` or the `PATH`; the devenv shell has `ffmpeg-headless`); without them: no generator. 60 s timeout. |
| Audio (WAV, AIFF, FLAC, MP3, AAC/ALAC in MP4, Ogg, CAF, MKA) | the embedded cover art (ID3 APIC, FLAC PICTURE, MP4 `covr`), else the waveform drawn | `symphonia` (pure Rust) |
| Text, Markdown, code | the first lines on a page | Inter from `pdfcraft-fonts`, drawn with `ab_glyph`; binary files refused |

Thumbnails come in two sizes, about 256 and 1024 px on the longer side
(never enlarged): **JPEG** (quality 82) when the picture is opaque,
**PNG** when it has transparency. Not WebP: the `image` crate only encodes
lossless WebP, which for a photo is several times a JPEG's size, and there
is no maintained pure-Rust lossy WebP encoder to use instead.

### Peaks

`peaks` is a compact binary (`drive_sidecars::peaks`, magic `DWPK`): the
sample rate, the length, and a few zoom levels of (min, max) per bucket as
signed bytes, mono or stereo (more channels are folded to two). The
finest level has at most 8192 buckets (each at least 256 samples); each
coarser level is 4× coarser, down to 256 buckets. A song is about 20 KB; a
three-hour recording the same, because the builder halves its finest
level whenever it would grow past the cap.

It is built in **one streaming pass**: symphonia decodes a packet at a
time into the builder, so memory stays flat however long the file is (a
2-hour, 230 MB WAV: under a second in a release build, and the whole
`drive-agent sidecars` pass over 250 MB of mixed files peaked at 94 MB of
memory, most of it the images). `craft-audio` (docs/viewers.md's survey)
was not used: its `decode` takes the whole file in memory.

## When they are made

- **On demand.** The app asks `GET /sidecar/{key}/{kind}` with what the
  node needs to make it (`name` for the extension; for a computer's file
  `space`, `location`, `path`, and `hash` when the node keeps a copy). The
  node makes it, or asks the device that has the file, and answers with
  it. One producer per content at a time; concurrent requests wait for it.
- **In the background, at low priority.** The node goes through every
  space's tree every two minutes (at most 50 files a pass); each agent
  goes through its own locations ten minutes apart (at most 200 files a
  pass, only when no reindex is pending), asking the node which keys lack
  sidecars (`POST /sidecars/missing`) and making those, one file at a time.
  Neither blocks indexing or sync. `drive-node --no-backfill` turns the
  node's off.
- **By hand.** `drive-agent sidecars [--force] [--kind thumb|peaks] [--max N]`.
- **Not again** when nothing reads a file: a "no generator" answer is
  remembered for a day per (key, kind) (`sidecar_misses`), or until a
  newer producer version.

## The routes

| Route | Who | What |
|---|---|---|
| `GET /sidecar/{key}/{kind}` | anyone with the key; asking a device (`space=`) needs membership | the sidecar (`image/jpeg`, `image/png`, or `application/octet-stream` for peaks; `nosniff`, cached an hour), else `404` with `{"state": "missing" \| "no-generator" \| "offline" \| "failed", "why"}`. `make=0`: only look. |
| `PUT /sidecar/{key}/{kind}?version=&mime=&also=` | signed in | keep an agent's sidecar (`also`: the same content's other key). Only the three MIME types above are accepted. |
| `POST /sidecars/missing` `{keys, kinds}` | signed in | which (key, kind) still need making |
| `POST /sidecars/miss` `{key, why}` | signed in | an agent found no producer for this content |
| `GET /sidecars/stats` | signed in | how many, the bytes they take (by kind), misses |
| `POST /sidecars/purge` | signed in | see below |

## Purging

Sidecars are a cache: anything can go, and comes back when wanted.
`POST /sidecars/purge` with any of

- `all: true` — everything (and every remembered miss);
- `older_than_days: N` — not read for N days (reads are recorded once a
  minute or two, not on every request);
- `stale: true` — made by an older producer;
- `budget_bytes: B` — then the least recently read, a whole blob at a time,
  until what stays fits in B;
- `kind: "thumb" | "peaks" | "thumb-256" | …` — narrows any of the above.

A blob goes when no row names it any more. The app's Overview shows the
space sidecars take, with a Clear button (`all`).

## In the app

- Grid cells and list rows draw the item's icon and, over it once loaded,
  its `thumb-256` (`loading="lazy"`: only items scrolled into view ask for
  theirs). No thumbnail, or none can be made: the icon stays.
- The inspector shows `thumb-1024` in place of the big icon when it does
  not read the file itself; a video's viewer uses it as its poster.
- The audio viewer (inspector and Quick Look) draws the waveform from
  `peaks` and only streams the file to play it; it decodes the file in the
  browser (as before) only when no peaks can be had and the file is small
  enough (96 MB).

## Next

- External readers (Immich, Jellyfin, Paperless) sharing sidecars where the
  formats allow.
- Transcripts, OCR and PDF text, EXIF as sidecars; search reading them.
- Deeper waveform zoom than the 8192-bucket finest level for long files
  (fetch a range of a finer level).
- HEIC, camera raw and PSD thumbnails (`craft-image-codecs`, libheif behind
  a feature).
- Confirming a sampled key with the full hash before a sidecar made from a
  computer's file is shared with the tree's copy.

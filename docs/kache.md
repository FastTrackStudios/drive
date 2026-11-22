# Sharing kache build caches between computers

How the owner's computers share their Rust build caches peer to peer through
Drive, with no cloud bucket (#45). Written 2026-11-21.

## In short

- **kache** (github.com/kunobi-ninja/kache) is the fleet's rustc wrapper.
  Its remote cache speaks S3, GCS, OCI or a shared folder only. It has no
  peer-to-peer backend and no plugin interface.
- So **each Drive agent offers kache a local S3 endpoint**
  (`http://127.0.0.1:9778`, bucket `kache`, loopback only) and does the
  sharing behind it.
- **Objects kache stores** are kept in the agent's own folder
  (`<agent>/kache`), under a size budget (20 GiB unless set). The least
  recently used are let go first.
- **An object not kept here** is asked of the device's **build group**:
  the other devices whose builds share keys. All of them are asked at once
  over the agent's iroh endpoint, with a 600 ms timeout. The object is
  fetched from the first that has it as a BLAKE3-verified blob (iroh-blobs,
  [transfer.md](transfer.md)), then kept here too.
- **Groups** are set per device in the space registry: `build_group` beside
  the device's entry. kache keys include the target triple and the
  toolchain, so two groups would never share a key anyway. The groups only
  save asking computers that cannot have it.

  | Group | Computers |
  |---|---|
  | `linux-x86_64` | THEBATTLESHIP, the VPS |
  | `darwin-arm64` | Airlock, Voyager |

- **A build never waits on it.** A miss with every peer answering costs one
  round trip. A peer that does not answer is waited for 600 ms at most,
  once, and then skipped for 30 s. An agent that is not running refuses the
  connection at once, and kache compiles.

Code: `crates/drive-kache` (the store, the S3 subset, the lookups) and
`crates/drive-agent/src/kache.rs` (the registry's groups, the daemon, the
command line).

## What kache asks of an S3 remote

Read from kache's source at v2.0.0-rc.3, the version the fleet pinned
(`src/remote_backend.rs`, `src/remote_layout.rs`, `src/remote.rs`,
`src/remote_pack.rs`, `src/remote_resilience.rs`), and compared with 2.3.0:
the same calls. THEBATTLESHIP runs 2.1.1 now.

The client is **OpenDAL's S3 service** (`opendal-service-s3` 0.59.5). It
uses path-style addressing for a custom endpoint, one attempt per operation
(kache owns retries), Content-MD5 on writes, and SigV4 signing.

| Call | Wire | What kache uses it for |
|---|---|---|
| exists | `HEAD /kache/<key>` | is an entry there (its v3 manifest) |
| get | `GET /kache/<key>`, no Range | manifests, packs (`.tar.zst`, up to 8 GiB), merged build manifests, predictions |
| put | `PUT /kache/<key>`, one request (never multipart: OpenDAL's writer sends a single buffer as one PutObject) | everything it stores |
| create-only put | `PUT` with `If-None-Match: *` (412 = already there) | v3 packs and manifests, which never change |
| replace-if-unchanged | `GET` for the ETag, then `PUT` with `If-Match: "<etag>"` (412 = changed, retry up to N times) | merged build manifests and shards (`_manifests/v3/…`): read, merged, written back |
| list | `GET /kache?list-type=2&prefix=…[&continuation-token=…]`, recursive (no delimiter) | `kache sync`, and prefetch when `remote_key_listing` is on (off by default for S3) |

There is **no DELETE, no multipart upload and no copy**: kache never
deletes remote objects, and `gc` is local. When a store answers
`501 NotImplemented` to a conditional write, kache falls back to a plain
PUT.

Keys look like this (default prefix `artifacts`):

```text
artifacts/v3/manifests/<crate>/<key>.json
artifacts/v3/packs/<crate>/<key>.tar.zst
artifacts/v3/predictions/<identity>.json
artifacts/_manifests/v3/<namespace>/…            (merged, If-Match)
artifacts/v4/prefetch/{packs,pack-meta,catalogs}/…
```

**Timeouts, and what kache does when a remote is slow or missing:**

- Connecting has a 3.1 s timeout.
- Reads have a 30 s inactivity timeout, not a total one.
- A synchronous demand from the compiler wrapper has a hard **3 s
  end-to-end ceiling**. Past it the daemon reports a miss and rustc
  compiles.
- A 404 is a miss. It is remembered for 60 s
  (`remote_negative_ttl_secs`), so parallel wrappers do not stampede.
- Timeouts and 5xx feed a per-direction breaker. Once it trips, reads are
  skipped for 45 s and writes stay queued.
- Uploads happen in the daemon's background queue. `kache daemon stop`
  gives the queue up to 30 s to drain.
- Packs are verified against the BLAKE3 hashes in their manifest when
  restored.

So the endpoint's one hard rule is that **misses must answer fast**. It
answers a miss within the lookup timeout (600 ms), well inside kache's
3 s.

**Merges and races.** Packs and manifests are written create-only, so the
first upload of a key wins and later ones are a harmless 412. Merged
manifests are read-modify-write tied by ETag. Here the ETag is the
object's BLAKE3, quoted, so one copy has the same tag on every device.
Each device keeps its own copy of a merged manifest: a write replaces the
local copy only, and a device that has none takes the first peer's. These
manifests only steer prefetch, so a device-local view of them costs some
prefetch hits, never correctness.

## Design as built

```text
kache (rustc wrapper / daemon)
   │  S3 over HTTP, 127.0.0.1:9778, bucket "kache"
   ▼
drive-agent ── drive_kache::s3 ── Store (<agent>/kache: objects/, index.jsonl, tmp/)
   │                     │ miss
   │                     ▼
   │        lookups: ALPN drive/kache/1 on the agent's iroh endpoint
   │        {"has":key} → {"size","hash"} to every group peer at once (600 ms)
   │                     │ found
   │                     ▼
   └──────── iroh-blobs GET <hash> from that peer (BLAKE3-verified, resumable),
             kept here under the budget, then served to kache
```

- **HEAD** answers from the local copy. Otherwise it asks the group and
  answers with the peer's size and ETag, without fetching.
- **GET** answers from the local copy. Otherwise one fetch per key runs at a
  time (parallel asks share it). The fetch runs in its own task, so it
  finishes and is kept even if kache gave up waiting.
- **PUT** streams the body to `tmp/`, hashes it as it goes, checks it
  against `Content-Length`, checks the condition, and renames it into
  `objects/<b3(key)[..2]>/<b3(key)>`. A create-only PUT of a key kept here
  is refused before its body is read.
- **LIST** merges the local keys with every peer's keys that arrive within
  2 s. The merged list is kept for 10 s for the pages of one listing.
- **Peers' doors.** The lookups run behind the same gate as the agent's
  files: only the space's devices and nodes get in at the handshake. The
  lookup handler then refuses any device outside this device's build group.
  A peer answers `has` only after its provider has hashed the kept file and
  the hash still matches. A kept file that no longer matches is let go, and
  the peer says it does not have it.
- **Warm connections.** Every 20 s the agent dials any group peer it has no
  connection to, so a lookup does not pay for dialing.
- **Nothing is announced.** No per-key index goes into the registry. Keys
  churn with every build and would bloat a synced document. A peer finds a
  key by asking when it misses. LIST is the one call that sees only the
  peers online now.
- **Eviction** is least recently used. A read or a peer's ask counts as a
  use, written to the journal at most once a minute per object. It deletes
  only files the store made: plain files (never a symlink) under `objects/`
  and `tmp/` of `<agent>/kache`. Every removal goes through one function
  that checks this, and a test plants user files, a stray file and a
  symlink to check it. The cache is derived data, so this is the one place
  Drive deletes files on its own.
- **Loopback only.** The listener binds `127.0.0.1` (any other address is
  refused at start), and any request whose peer address is not loopback
  gets `403`, with one `warn`. Signatures are not checked, so any
  credentials do. Every local user can use the endpoint, the same trust as
  THEBATTLESHIP's shared kache store, whose users already write to one
  store.

## Setting it up

On each computer, once (the registry syncs it to the others):

```bash
drive kache group auto     # linux-x86_64 on THEBATTLESHIP and the VPS, darwin-arm64 on the Macs
drive kache status         # the group, who shares it, what is kept here
drive kache config         # the block for kache, below
```

`kache group NAME` sets any name, and `kache group off` leaves the group.
The running agent starts or stops serving within 30 s of the change. The
VPS needs a Drive agent in the space like the other computers.

| Where | Setting | What |
|---|---|---|
| `config.json` | `kache.port`, `drive kache port N` | the loopback port (9778) |
| `config.json` | `kache.budget_gib`, `drive kache budget N` | how much is kept here (20) |
| `config.json` | `kache.lookup_ms` | how long a miss waits for the group (600) |
| registry | `devices.<id>.build_group` | the device's group |

### kache's side

Every kache process on the computer reads this block:

```toml
[cache.remote]
type = "s3"
bucket = "kache"
endpoint = "http://127.0.0.1:9778"
region = "us-east-1"
```

kache's S3 client needs credentials to sign requests. Its config file
holds no keys (only a `profile` naming an AWS profile), so set
`KACHE_S3_ACCESS_KEY` and `KACHE_S3_SECRET_KEY` (or use the AWS chain). Any value does, because the agent
does not check signatures. A daemon kache starts keeps these variables. A
plain-http endpoint on loopback does not trigger kache's plain-http
warning.

### The nix-config change (not applied)

Suggested for `~/.flake`. Nothing there has been edited.

```nix
# hosts/THEBATTLESHIP/kache.nix, hosts/airlock/kache.nix, hosts/voyager/kache.nix,
# and the VPS's host: after the existing [cache] table of
# environment.etc."kache/config.toml".text, add
''
  # Shared with this computer's build group through its Drive agent
  # (FastTrackStudios/drive docs/kache.md): no cloud bucket.
  [cache.remote]
  type = "s3"
  bucket = "kache"
  endpoint = "http://127.0.0.1:9778"
  region = "us-east-1"
''

# Signing keys kache insists on; not secrets (the agent serves loopback
# only and checks no signature). NixOS and nix-darwin both:
environment.variables = {
  KACHE_S3_ACCESS_KEY = "drive";
  KACHE_S3_SECRET_KEY = "drive";
};

# hosts/THEBATTLESHIP/github-runners.nix: the runners' units set their own
# environment (RUSTC_WRAPPER, KACHE_RUNTIME_DIR, …); add the two keys there:
#   KACHE_S3_ACCESS_KEY = "drive";
#   KACHE_S3_SECRET_KEY = "drive";
```

If the Drive agent is not running, kache's connection to 127.0.0.1 is
refused at once. kache counts that against its breaker and compiles: builds
work as they do now, without sharing. Turn sharing off by removing the
block, or with `drive kache group off`.

## Tests

| Test | Holds |
|---|---|
| `drive-kache` `s3_surface::every_call_kache_makes` | with OpenDAL's S3 client, built as kache builds it: HEAD and GET of a missing key are NotFound; create-only PUT, then refused (412 → ConditionNotMatch); HEAD gives size and a strong ETag; If-Match replace, a stale tag refused; plain PUT replaces; ListObjectsV2 recursive over 3 pages (2501 keys) |
| `another_bucket_and_other_calls_are_refused` | another bucket 404 NoSuchBucket; DELETE and multipart 501 |
| `listens_on_loopback_only` | binding a non-loopback address is refused |
| `two_agents::a_peers_object_is_fetched_on_a_miss` | two agents in one process: a 3 MB object PUT on A is found by HEAD on B, fetched by GET, kept on B, the same ETag on both; B's LIST includes A's keys |
| `a_corrupted_copy_is_not_served` | a byte flipped in A's kept file: B gets 404 and keeps nothing, and A lets its bad copy go |
| `misses_answer_quickly` | the miss latency bound (below) |
| `devices_outside_the_group_get_nothing` | a device whose group does not include it gets 404 |
| `a_fetched_object_counts_against_the_budget` | objects fetched from peers are evicted least recently used first |
| `store::tests::least_recently_used_goes_first_and_only_cache_files_go` | LRU order; a smaller budget lets more go; `config.json`, the blob cache, a stray file and a planted symlink are untouched; removals outside the cache folder are refused |
| `store::tests::conditions`, `reopened_store_keeps_its_objects` | create-only and If-Match on the store; the journal survives a restart; orphans and leftover temporary files are cleaned only inside the cache folder |
| `real_kache::a_crate_built_on_one_device_is_a_hit_on_the_other` (ignored: needs `kache` on PATH) | real kache, its own store, runtime dir and config per side (no host layer): a two-crate workspace built on A uploads 6 objects to A's endpoint; the same crate in another folder on B, with an empty store, is **2 of 2 crates from cache, 0 compiled**, the packs fetched from A |
| `drive-files` `build_groups_beside_devices`, `drive-agent` `kache::tests::*` | the registry key; a device's peers are its group, itself and other groups left out |

Run the real-kache test with
`cargo test -p drive-kache --test real_kache -- --ignored --nocapture`.

## Measured (THEBATTLESHIP, 2026-11-21)

Debug build, two agents in one process over loopback (direct iroh
connection), kache 2.1.1.

| | |
|---|---|
| a miss, the peer answering (worst of 20) | 17 ms |
| a miss, the peer unreachable: first ask | 404 ms (lookup timeout 400 ms in the test) |
| the same, next ask (the peer skipped for 30 s) | 2.4 ms |
| a miss, no peers | 1.9 ms |
| real kache, B's build from A's cache (2 crates, 1 MB of packs) | 0.94 s wall; 100% hits |

## Next

- Merged build manifests are per device (see "Merges and races"). Merging
  them across peers would need kache's merge, so the endpoint would
  understand their JSON.
- Prefetch: when a HEAD finds a manifest on a peer, start fetching its pack
  before kache's GET arrives.
- The iroh-blobs store's outboards for files let go: the tag is deleted,
  but the store is not garbage-collected yet (shared with #38).
- Signature checking with a per-computer key, if more than the owner's
  users ever build on these computers.

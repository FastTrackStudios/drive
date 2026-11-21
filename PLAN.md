# Drive: the prototype plan

How to prove the design ([docs/design.md](docs/design.md)) with the least
code, hardest parts first. Each milestone ends in a demo anyone can watch,
and adds crates the next ones reuse. Written 2026-11-20.

## Ground rules

- **Rust, one Cargo workspace,** crates prefixed `drive-` (the table in
  the design). Apps live in their own repositories and depend on
  `drive-client` by path while prototyping.
- **Use the crates directly:** `loro`, `iroh`, `iroh-blobs`, `turso`,
  `opendal`, `blake3`, Stalwart Labs' mail crates. Read Architect's sync
  code and lift what works; depend on none of Architect.
- **Dioxus 0.8 alpha** for every app UI, through `drive-dioxus`; nothing
  below it knows about Dioxus.
- **Every crate tested on its own;** every milestone has an automated
  test of its demo (two or three peers in one process, then real ones).
- **Prototype, not product:** no billing, no hardened security, no
  polished UI. But the protocol (document schemas, the sync exchange,
  blob hashes) is versioned from the first commit.
- Commit locally; no remote until asked.

## M0 — the workspace

- `drive-core`: ids, device keys (iroh's ed25519), signing and checking,
  links (to a document, a blob, an email by Message-ID).
- `drive-doc`: typed documents over Loro with a schema version and
  migrations; the first type, `studio.Set` (songs as a movable list; key,
  arrangement and leads per song as maps).
- `drive-store`: Turso tables for snapshots, update logs and one
  projection (sets by date).

**Done when:** a test makes a Set, edits it, saves and reopens it from
Turso, and migrates a v1 Set to v2.

## M1 — two devices, no server

- `drive-sync`: exchange Loro version vectors and updates for a set of
  documents, over any byte stream; live subscriptions push each update.
- `drive-net`: an iroh endpoint; local discovery (mDNS); pairing by
  ticket (a QR code or a link).
- `drive-client` (first cut): open a typed document, edit it, watch it.
- `drive-dioxus`: a hook that turns a document into a signal.
- **Studio:** the set page reads and writes its Set through
  `drive-client` instead of in-memory data.

**Demo:** Studio on a laptop and in the iPhone Simulator, paired by
ticket on the same network, no internet: change a song's key on one; the
other updates live. Turn the network off, edit both, turn it back on:
both converge.

## M2 — presence

- Loro's ephemeral store over the same connection (QUIC datagrams):
  who is here, what each is looking at.

**Demo:** the set page shows the other person ("Mia is here"), and which
song each is on, live.

## M3 — the node and the relay

- `drive-node`: the headless binary, roles client + always-on peer +
  relay (`iroh-relay`), placement rule "keep everything I am given".
- Deployed to the VPS.

**Demo:** the laptop edits, goes offline; the phone, never online at the
same time, comes up later and gets the edit from the node.

## M4 — share links and guests

- Capabilities in `drive-core`: scope, level, audience, expiry, signed.
- The web role in `drive-node`: serves Studio's web build limited to the
  link's scope; guests sync over a websocket and get a temporary key.
- Rate limits and the owner's lock on public edit links.

**Demo:** a set opened from a link in a phone browser, nothing installed,
no account: the guest sees edits and presence live, and their own edit
(as "Guest · Mia") reaches the owner.

## M5 — the spine and the Drive app

- Spine types in `drive-doc`: Task, Project, Event, Person, Note.
- Cards: an app declares, as data, how its types show elsewhere.
- **Studio** writes Events for services and rehearsals, Tasks for its
  to-dos.
- **The Drive app v0:** inbox (quick capture), tasks, projects, calendar;
  Studio's cards; a tap opens Studio.
- Two apps on one device: desktop over a local connection; iOS through
  an App Group folder.

**Demo:** plan a set in Studio; it appears in the Drive app's calendar
with its tasks; capture "ask Jake about the bridge" in the Drive app's
inbox and link it to the set, on one device with no internet.

## M6 — media

- `drive-blob`: BLAKE3 (sampled to find candidates, full to confirm);
  OpenDAL stores (local disk, S3); `iroh-blobs` between nodes; signed
  URLs for browsers; indexing an existing bucket without moving it.

**Demo:** a mix added on the laptop plays on the phone, fetched from the
node or straight from S3, checked against its hash.

## M7 — accounts, orgs, permissions

- `drive-auth`: sign-in (the central auth over OIDC; which one is an open
  question), device attestation, orgs and signed memberships, roles,
  checked by every node; node registry with owners and placement rules.
- The account directory: find your devices anywhere.

**Demo:** a new phone signs in and finds the laptop and the node; a
member removed from an org stops receiving its new edits.

## M8 — Mail

- `drive-mail`: IMAP and JMAP accounts, a cache in Turso, sending with
  SMTP; links by Message-ID.
- **The Mail app v0:** read, reply, "make a task", "file to project".

**Demo:** an email becomes a task in the Drive app's inbox, linked back
to the thread.

## M9 — Meals

- **The Meals app v0** on Drive: building blocks and their workflows
  (steps with scaled quantities, active and passive time, "done when",
  variants, sub-workflows), combinations, plans, the prep session's
  timeline, cook mode, the shopping list
  ([docs/apps/meals.md](docs/apps/meals.md)).

**Demo:** plan a week from blocks; the prep session's timeline overlaps
the teriyaki's simmer with the chicken's prep; two phones run cook mode
together, live; the session is in the calendar and the shopping list in
the inbox; a guest opens the plan by link.

## After the prototype

The hosted cloud (tenants, quotas, billing), bookings, agents' scoped
tokens and MCP, the agent wiki, end-to-end encryption per org,
Stalwart as an optional companion.

## Open questions (from the design)

1. A public name beside Google Drive and Spacedrive.
2. The central auth: fts-auth, Kanidm or Rauthy.
3. Encryption: optional per org, or on by default.
4. Extract Architect's sync layer, or write `drive-sync` fresh.
5. Schema migrations: per-type functions, or declarative.
6. How much of Task's data model to carry over.

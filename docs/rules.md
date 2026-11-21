# File rules

Hazel-style rules (and Forel's, whose ideas, not code, this follows: it
is GPL): a rule says which devices and places it looks at, which files it
takes and what it does to them. Unlike Hazel, the rules are one synced
document per org, so every device runs the same rules over its own files,
and the node runs them over the Drive tree. Written 2026-11-21 (#20).

## Safety first

Much of what Drive holds exists once. So, in this version:

- **Nothing deletes or overwrites, ever.** No action unlinks a file of
  yours or writes over one.
- **Every new rule is off and a dry run.** A dry run works out what the
  rule would do, shows it in the app and changes nothing. A person turns
  it on and switches it to live (the app asks first; `drive-agent rules
  run NAME --live` refuses a rule that is not on and live).
- **Moves and renames of a computer's files are off per device**
  (`"allow_rule_moves": true` in that device's agent `config.json`, set
  by hand, like `allow_evict`). Even on, a move is: copy, check the copy
  byte for byte (BLAKE3 against the original), and only then rename the
  original into `.drive-trash/` at the location's root, at its relative
  path. It is never unlinked; the agent's indexer skips `.drive-trash`
  (a hidden folder). A move to another computer is not built yet (copy
  instead).
- **Trash** is only offered for files of the Drive tree, where it is the
  Drive's Trash folder (not a deletion). For a computer's files it is not
  offered at all.
- **Copies never overwrite.** A copy is written to a hidden temporary name
  beside its destination, checked, then given its name by a hard link that
  fails if the name is taken; a taken name gets " (2)", " (3)", ….
- **Undo** is journalled for every reversible action. It refuses when the
  file changed since (its BLAKE3 differs), or when the rule is live and
  would do it again at once. Undoing a copy puts the copy in
  `.drive-trash` (or the Drive's Trash), it does not delete it.

The only files the rules code ever unlinks are its own temporary names
(the extra link once a copy has its name, a copy that failed its check).

Tests that hold these lines: `crates/drive-rules/src/tests.rs` (a new rule
is off and a dry run; moves blocked unless allowed, including in the
preview; Trash never offered for a computer's files; templates cannot
climb out of their destination; the Drive tree's actions keep every entry;
copies take free names; undo of tags and fields), `crates/drive-agent/src/rules_fs.rs`
(a copy never overwrites and is verified; a bundle is checked whole; bytes
must match their hash; trash is a rename and restore never overwrites, and
every byte that was there is still somewhere), `crates/drive-rules/src/guard.rs`
(loop protection) and `crates/drive-node/src/rules.rs` (a dry run over the
Drive tree edits nothing; a live run moves, and undo puts it back).

## The model

`drive_rules` (pure, native and wasm). Two documents per org:

| Document | Holds |
|---|---|
| `<org>/drive:rules` | `rules`: id → { name, enabled, mode, scope, trigger, when, actions, created, by }; `requests`: id → { at, live, by } ("Run now") |
| `<org>/drive:rules:journal` | `entries` (every action: device, rule, before, after, status, undo), `previews` (each rule's latest dry run per device), `runs` (each run's counts per device), `available` (files a rule wants highly available) |

Edits record who and when (`drive_spine::history`). A rule's name, on/off
and mode are separate keys, so switching a rule live on one device and
renaming it on another both land; its scope, conditions and actions are
edited as a whole (the last save wins).

A rule:

- **Scope.** Devices: all, by role (`workstation`, `storage-server`), or
  named. Places (none: everything): a location and a folder in it, a Drive
  folder, a project (its parts), things with a tag. Paths are never
  absolute: a location means the right folder on whichever device has it.
- **Trigger.** When files change (a device's watcher; the Drive tree's
  edits on the node), every N minutes, and always "Run now". A rule
  switched on, made live or edited runs once at once.
- **Conditions.** Groups, each all-of or any-of its conditions, the groups
  taken all-of or any-of; each condition can be negated. Name and path
  (is, contains, starts with, ends with, a regular expression), extension,
  Drive kind (and the kinds under it), size, created / modified / added
  (in the last N units, more than N ago, before or after a day), a tag (or
  one under it), a field (#19), project membership, how many copies Drive
  knows of, which device holds it. A date the file system does not record
  never matches, negated or not.
- **Actions**, in order: add or remove tags, set a field, add to a
  project, copy to a place (a folder of a location on this or another
  device, or of the Drive), move, rename, make sidecars (thumbnail,
  waveform), keep highly available (#13: recorded now, carried out by the
  agent's "available" pass; public storage when there is one), Drive
  Trash (Drive files only). Names and folders are templates: `{name}`
  (without the extension) `{ext}` `{year}` `{month}` `{day}` (of its
  creation, else its modification) `{kind}` `{project}` `{device}`. A
  template's output cannot leave its destination (`..` is dropped,
  separators in a name become `-`).

The evaluator (`Matcher`): a file's facts and a rule in, whether it
matches and a plan out, step by step, each step with what it says, and
whether it is held back (and why) or already so. A move that goes ahead
changes where the later steps act. The app's preview, the agents and the
node all decide with this.

## Who runs what

- **Each device's agent** (`drive-agent serve`) runs the rules whose scope
  includes it over its own locations (on disk; locations in a storage are
  not yet): after a location is indexed again, on schedules, on requests;
  it fetches copies other devices' rules sent it (from the node, checked),
  retries uploads that did not reach the node, and undoes what it did when
  asked. One runner per device: the command line asks a running `serve`.
- **The node** runs the rules that look at the Drive tree, every 15 s, as
  document edits (tags, fields, moves and copies inside the tree, the
  Trash); a copy to a computer's location waits in the journal for that
  computer.
- **Waiting.** A file modified in the last minute is left until the next
  pass (it may still be being written). Temporary, lock and partial files
  (`.part`, `.crdownload`, `~$x`, `.~lock.x#`, `.DS_Store`, …) are never
  taken. Symbolic links are not followed.
- **Bundles** (`.app`, `.logicx`, `.band`, `.photoslibrary`, `.RPP-bak`
  folders, plug-ins, …) are one item: never looked inside, copied and
  checked whole. For now a bundle is only copied to another folder of the
  same computer, and is not tagged (it has no content hash) or kept
  available.
- **Loop protection.** A file a rule just touched is left alone by every
  rule for 10 minutes; a rule never takes what it made (by path or by
  content); and a rule remembers what it handled (content, size and
  time), leaving an unchanged file alone, so a copy rule copies once.
- **Offline.** A copy to a device that is off waits in the journal
  ("waiting for Voyager"), and the device fetches it when it comes on;
  an upload the node did not take is tried again every pass.

## The app

Rules, in the sidebar's Space section:

- the list: each rule's name, its actions, how many files match where, an
  on/off switch and Dry run | Live (going live asks first);
- a rule's page: the rule in words, "What it would do" (the Drive tree's
  dry run worked out in the browser, each device's from its agent; held
  back steps say why), "What it did" (each device's last run, and every
  action with its status and Undo), Dry run now, Run now (live rules),
  Edit, Remove;
- the editor: name; where (devices, places); when; which files (groups of
  conditions); do (actions, reorderable). On a phone it is a sheet.

## The command line

```text
drive-agent rules ls                      # every rule: on/off, dry run/live, last runs
drive-agent rules show NAME               # the rule (JSON), each device's latest dry run
drive-agent rules run NAME [--live]       # run it now: here, and asked of every device and the node
drive-agent rules journal [NAME]          # what rules did, newest first
drive-agent rules undo ID                 # take an action back (or ask the device that holds it)
drive-agent rules add FILE.json           # a new rule (off, dry run); locations, devices, tags and projects by name
drive-agent rules on|off NAME
drive-agent rules live|dry-run NAME
drive-agent rules rm NAME                 # the journal stays
```

A rule as JSON:

```json
{
  "name": "File mixes",
  "scope": { "devices": { "by": "all" },
             "places": [ { "kind": "location", "location": "Downloads", "path": "" } ] },
  "trigger": { "on_change": true, "every_minutes": 0 },
  "when": { "all": true, "groups": [ { "all": false, "conditions": [
      { "test": { "test": "name", "text": { "op": "contains", "value": "mix" } } },
      { "not": true, "test": { "test": "size", "cmp": "less", "bytes": 1000 } } ] } ] },
  "actions": [
    { "do": "add-tags", "tags": ["Mixes"] },
    { "do": "copy", "to": { "kind": "location", "location": "Archive", "path": "Mixes/{year}" }, "name": "" },
    { "do": "rename", "name": "{year}-{month}-{day} {name}.{ext}" }
  ],
  "enabled": false, "mode": "dry-run", "id": "", "created": 0, "by": ""
}
```

## Not yet

- Moves to another computer or between a computer and the Drive (copy
  works); moves into a storage's location; rules over locations in a
  storage (S3, WebDAV).
- Bundles to another computer, tagging bundles, keeping them available.
- Conditions on content text (sidecars #1, OCR #2), last opened, and where
  a file came from (download URL, source app); actions to make a share
  link, publish, and run a script (opted-in devices only).
- "Added" for a computer's files (when Drive first saw it) is not recorded
  yet: a condition on it never matches there.
- Consecutive moves (rename, then move) each keep their original in
  `.drive-trash`; they could be one.

# Tags, fields and tag search

Names people put on files, documents and projects, and named values
beside them, for finding things again. The ideas come from TagStudio
(parent tags, aliases, shorthands, colours, category tags, fields, a
search language); nothing of its code or text is used. Written
2026-11-21.

Where Drive differs: TagStudio keeps one SQLite file per library, knows
an entry by its path (a rename breaks the link) and has one user. Drive
keeps every tag in a synced Loro document per org, keys a file by what it
is rather than where it is, and lets several people tag at once.

## The model

One document per org, `<org>/drive:tags` (`drive_spine::tags`):

| Map | Holds |
|---|---|
| `tags` | tag id → `{ name, shorthand, aliases: {alias → true}, color, parents: {tag id → true}, category, hidden, created, by }` |
| `targets` | target key → `{ tags: {tag id → { by, at }}, fields: {field id → { name, kind, value, by, at }} }` |

**Targets** are keyed so a tag follows the thing:

| Thing | Key | The tag follows it through |
|---|---|---|
| A file or folder in the Drive tree | `file:<tree id>` | moves and renames inside the Drive |
| A file on a computer (a location's index) | `content:<sampled hash>` | moves, renames, copies, other devices: every copy shows the same tags |
| An app's document | `doc:<id>` | the document |
| A project | `project:<id>` | the project |

The sampled hash includes the size (see [files.md](files.md)); two files
that differ only between the samples share tags, as they share the
"copies" count. Folders on computers have no content hash and are not
tagged yet.

**Tags** have a name, a shorthand, aliases, a colour from a fixed palette
(red, orange, amber, yellow, lime, green, teal, cyan, blue, indigo,
violet, pink, brown, grey, or none), any number of parents, and two
switches: *category* (the inspector groups a thing's tags under the
category tags above them) and *hidden* (things with it leave search
results unless the search names it). Two tags are built in: **Archived**
(hidden) and **Favorite**; they cannot be deleted.

- A parent that would make a cycle is refused (`TagError::Cycle`).
- Two people can still make one together (A under B on one device, B under
  A on another). Every device then drops the same closing edge (edges
  taken in a fixed order), so all read one hierarchy; the tag manager
  lists the dropped edges.
- Two tags may share a name; they are shown with a parent in brackets,
  `Freddy (FNAF)` (the parent's shorthand if it has one).
- Deleting a tag takes it off everything and out of every tag's parents.

**Fields** have a free name and a kind: text, long text, date
(`YYYY-MM-DD`), date and time (`YYYY-MM-DDTHH:MM`). A thing can have any
number. Templates offered: Title, Artist, Album, Source, Description,
Notes, Date created, Date released, Recorded.

**Who and when.** Each assignment and field records who (`by`: the signed-in
person, or the browser's or device's name) and when (`at`, ms), and every
change carries its author in the document's history (`drive_spine::history`).
Concurrent tagging merges: two people tagging one file at once both land;
a rename and a recolour of one tag both land.

## The search language

`drive_spine::tags::query`: a pure parser and evaluator.

| Write | Finds |
|---|---|
| `drums` | things tagged Drums, or any tag under it (by name, shorthand or alias, any case). A word that names no tag matches names instead |
| `drums snare` | both (implicit AND); `AND` works too |
| `drums OR keys` | either |
| `NOT archived` | without it |
| `(drums OR keys) NOT wip` | brackets |
| `"bass guitar"`, `bass_guitar` | a tag with a space in its name |
| `tag:mix` | only the tag (no name fallback) |
| `name:take` | the name contains it |
| `path:Mixes/`, `path:*.wav` | the path contains it, or matches the glob (`*` any run, `?` one character, whole path) |
| `kind:audio` | a kind by id or name; broader kinds count (`kind:audio` finds MIDI) |
| `special:untagged` | no tags; `special:empty`: no tags and no fields |
| `artist:jane` | a field named Artist containing "jane" (`date_created:2026-11`) |

Operators are case-insensitive. Text matches are **smartcase**: all lower
case matches any case, a capital makes it case-sensitive. Precedence:
NOT, then AND, then OR. Errors say what is wrong (`a ( is not closed`,
`AND needs something after it`).

### In the search box

There is no toggle: the one box takes both. Plain words that name no tag
are a name search, exactly as before (one request to the node's index).
As soon as a word names a tag, or the search uses the language
(operators, brackets, `path:`, `kind:`, `special:`, a field), it runs as a
tag search and says so ("tag search" beside the count). The reason for no
toggle: the language is a superset of the old search, so a plain search
cannot be misread.

What a tag search looks at: the whole Drive tree; every file on any
computer whose content carries a tag (`/search?sampled=…`); and the files
the node's name index finds for its words, path pieces (the longest
literal of a glob, `.wav` of `*.wav`) and kinds. `NOT`, `special:` and
field searches therefore see the Drive and tagged files, not every file on
every computer (the results say so).

## The app

- **Inspector:** a thing's tags as coloured chips, grouped under their
  category tags; × removes one; a click on a chip opens the tag. **+ Tag**
  opens a field: type, Enter adds the top match (arrows pick another),
  a name that matches nothing offers "Create", Enter on an empty field
  (or Escape) closes it. Fields below, edited in place; **+ Field** adds a
  template or any name.
- **Sidebar:** Tags, nested by parent, with how many things carry each
  (counting a thing under every tag above its own). A tag opens its
  things, with links up to its parents and down to its children.
  **Manage** opens the tag manager.
- **Tag manager:** every tag with its parents and count; pick one to
  rename it, set its shorthand, colour, aliases and parents (a parent that
  would make a cycle is not offered, and typed exactly it is refused with
  the reason), make it a category or hidden, or delete it (it asks first,
  saying how many things it is on).
- **Projects:** a project's page has its tags.

## The command line

```text
drive-agent tag ls                          # every tag, nested, with counts
drive-agent tag ls THING                    # the tags and fields on a thing
drive-agent tag add THING TAG…              # makes tags that do not exist yet
drive-agent tag rm THING TAG…
drive-agent tag new NAME [--parent TAG]… [--color red] [--shorthand S] [--alias A]… [--category] [--hidden]
drive-agent tag field THING NAME VALUE [--kind text|multiline|date|datetime]
drive-agent tag search QUERY…
```

A THING is a file on this computer (tagged by its content), `drive:PATH`
(the Drive tree), `LOCATION:PATH` (a file of an indexed location),
`project:NAME`, `doc:ID`, or a raw key (`file:<tree id>`,
`content:<hash>`).

## Not yet

- Tagging many selected things at once (the browser has single selection).
- Tags on folders of computers' locations (they have no content hash).
- A tag search over every file of every computer for `NOT` and
  `special:` (needs the tags beside the node's index rows).
- Confirming a content key with the full hash, as copies will be.

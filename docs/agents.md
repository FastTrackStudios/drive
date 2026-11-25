# AI agents: Drive as an MCP server

An AI agent (Claude Code, or any client that speaks the Model Context
Protocol) can see the owner's files on every computer through Drive and
help organise them, without being able to lose anything. Written
2026-11-22 (#43).

`drive mcp` is an MCP server on stdio. It runs wherever the agent
runs and acts as that computer's Drive device: on the node's machine it
reaches the node over loopback, elsewhere with the device's session
(`drive sign-in`). It reads the space's synced documents (the
registry, tags, rules and their journal, projects) and asks the node's
HTTP API for the rest (search, copies, live browsing, indexing a folder).

## Adding it to Claude Code

The computer needs a working agent first (`drive init` or `join`,
see [files.md](files.md)). Then:

```sh
claude mcp add drive -- drive mcp
```

With a non-default agent folder, or a name for the history:

```sh
claude mcp add drive -- drive --dir ~/.local/share/drive mcp --as claude-code
```

The same as JSON (a project's `.mcp.json`, or `claude mcp add-json`):

```json
{
  "mcpServers": {
    "drive": {
      "type": "stdio",
      "command": "drive-agent",
      "args": ["mcp"]
    }
  }
}
```

## Other MCP clients

Any client that runs a stdio server takes the same command and arguments:
`drive [--dir DIR] mcp [--as NAME] [--max-writes N] [--page-max N]
[--timeout SECONDS]`. Clients configured with an `mcpServers` map
(Claude Desktop's `claude_desktop_config.json`, Cursor's `.cursor/mcp.json`)
take the entry above, `{"command": "drive-agent", "args": ["mcp"]}`; give
the full path to `drive-agent` when the client does not run with your
`PATH`.

| Flag | Default | What |
|---|---|---|
| `--as NAME` | the name the client gives (`clientInfo.name`) | who the history says made each change: `agent: NAME on DEVICE` |
| `--max-writes N` | 500 | write operations one session may make |
| `--page-max N` | 500 (at most 2000) | the most items a page of any listing holds |
| `--timeout SECONDS` | 90 | how long one tool call may take |

stdout is the protocol's; the server's own events go to stderr through
`tracing` (`RUST_LOG=drive=debug` for more): one `mcp.tool` span per
call with `mcp.tool`, `mcp.client`, `mcp.dry_run`, `mcp.outcome` (`ok`,
`error`, `refused`, `unknown`), `mcp.writes` and `mcp.ms`; a refusal is
one `warn`.

## Naming things

Every tool names files the same way:

| Write | Means |
|---|---|
| `drive:Songs/mix.wav` | a file or folder of the Drive tree |
| `Samples:Drums/kick.wav` | a file of a location (an indexed folder of some computer), relative to it |
| `project:Spring EP` | a project |
| `doc:studio:song:x` | an app's document |
| `content:<sampled hash>` | a content, wherever its copies are |
| `/home/me/file.wav` | a file on this computer (tagged by its content) |

## The tools

Listings are paged: `offset` and `limit` in, `total`, `offset`,
`next_offset` (null on the last page) and `items` out.

### Seeing

| Tool | When to use it |
|---|---|
| `devices` | Start a survey: every computer, whether its agent is connected, its roles, its disks with size and free space. |
| `locations` | The indexed folders of every computer (device, path, files, bytes, online, last indexed). Their names are how files are named. |
| `browse` | A folder a page at a time: a location from its index (works while its computer is off), or a device's disks live (`device`; then `root` and `path`). |
| `search` | Drive's search language ([tags.md](tags.md)) over every computer's files, the Drive, documents and projects: `kind:audio NOT archived`, `path:*.wav`, `special:untagged`, `artist:jane`. |
| `file_info` | One thing: kind, size, dates, content key, every copy and where, tags, fields, projects, which sidecars exist. |
| `folder_summary` | A folder in numbers before proposing anything: by kind and extension, largest, oldest, newest, duplicates inside. |
| `duplicates` | The same content in several places, most wasted space first (by sampled hash and size: likely, not certain). |
| `projects` | Projects, their parts on whichever computers, documents, tags. |
| `tags` | Every tag with parents, aliases, colour, switches and counts; with `thing`, the tags and fields on it. |
| `rules` | File rules: on or off, dry run or live, what they do, each device's last run; with `rule`, one in full. |
| `rule_preview` | A rule's dry run per device: each file and each step, held-back steps with why. Asks for a fresh one (never live) and waits a little. |
| `journal` | What rules did, newest first, with status and whether it can be undone. |
| `graph_search` | Full-text search of notes' text, titles and aliases, file and folder names, tag names and documents' titles, best first; `tag` narrows to things under a tag. |
| `graph_backlinks` | What points at a thing: notes linking or embedding it, things tagged with it, tasks, events and projects naming it ([graph.md](graph.md)). |
| `graph_neighbors` | Everything within `depth` edges of a thing, either way. |
| `graph_path` | A shortest chain of edges between two things. |
| `graph_tag` | Everything under a tag (child tags too), Drive tags and notes' frontmatter tags. |

### Organising

Every write tool takes `dry_run: true`, which says what it would do and
changes nothing (and costs no writes).

| Tool | What it does |
|---|---|
| `tag`, `untag` | Put tags on things or take them off; a computer's file is tagged by its content, so every copy shows it. `create_missing` makes missing tags. All the things and tags are checked first: all of it, or nothing. |
| `page_create`, `page_write` | Make a page (a Markdown note kept as live text) at a Drive path, or give one its new text: only what differs is applied, so others' edits elsewhere in it stay. `page_read` reads one. |
| `link`, `unlink` | Say one thing refers to another in an app's words (`kind`: uses, built-on), in `drive:links`; the graph tools see it ([graph.md](graph.md)). |
| `set_field` | A named value on a thing (Artist, Source, Date created, …); the old value is in the answer. |
| `create_tag` | A tag with parents, colour, shorthand, aliases, category and hidden switches; a duplicate (same name and parents) is refused. |
| `create_project`, `add_to_project` | A project; a folder of a location as one of its parts, or an app document. |
| `index_folder` | Make a folder of a computer a location (indexing only reads it). |
| `propose_rule` | A file rule in [rules.md](rules.md)'s JSON, made **off and as a dry run** whatever the JSON says, with its dry run returned. |
| `plan`, `apply_plan` | A one-off reorganisation: a batch of `tag`, `untag`, `set_field`, `add_to_project`, `copy`, `move` and `rename` operations, previewed first, then applied. |

A plan's operations each come out as one of:

- **apply**: tags, fields and projects (through the tools above, with their
  checks), and copies, moves and renames inside the Drive tree. A copy is a
  new entry sharing the same blob, checked to be whole on the node; a taken
  name gets " (2)"; folders the move needs are made; nothing is removed.
- **rule**: a computer's files are only ever moved, copied or renamed by
  file rules, so these become proposed rules (one per place and action,
  their files by exact path, not run on change), off and dry runs.
- **refused**, with why: `delete`, `remove`, `trash`, `unlink`, `erase`,
  `purge`, `empty_trash`, `overwrite`, `replace`, `truncate`, `wipe`,
  `shred`; any operation with `overwrite: true`; moves into the Trash;
  moves across computers or between a computer and the Drive (not built
  yet: copy instead); whole folders copied; anything unknown.

### The prompt and resources

- Prompt `organize` (optional `focus`): the safety model and a workflow:
  survey (`devices`, `locations`, `projects`, `tags`, `rules`), summarise
  (`folder_summary`, `duplicates`, `search`) and tell the person, propose
  (reuse tags, projects for pieces of work, rules for what should keep
  happening), preview (`dry_run`, `plan`, `propose_rule`), then act only on
  what the person agreed to and ask them to switch rules on and live.
- Resources `drive://safety` and `drive://search-language`.

## The safety model

1. **Nothing deletes, trashes or overwrites.** No tool does: there is no
   delete, trash or remove tool (untag takes a tag off, the tag stays), a
   plan refuses those operations, copies take a free name, and Drive-tree
   moves only change where an entry is listed.
2. **Rules stay a person's decision.** `propose_rule` and `apply_plan`
   create rules off and as dry runs; no tool switches a rule on or makes
   it live, and every run an agent asks for is a dry run. A person does
   that in the Drive app (Rules) or with `drive rules on NAME` and
   `drive rules live NAME`. A rule that trashes is refused.
3. **A computer's files move only by rules,** so only after a person
   switches the rule on and live, and only on devices whose owner set
   `allow_rule_moves` in the agent's config; even then a move is a verified
   copy and the original goes to the location's `.drive-trash`
   ([rules.md](rules.md)).
4. **Limits.** A write cap per session (each tag on each thing, field,
   project, location, rule and plan operation is one; a call that would
   pass the cap does nothing); paged listings with a maximum page; a
   timeout on every call; unknown arguments are refused.
5. **Who did it.** Every change records `agent: <client> on <device>` as its
   author (tag assignments' and fields' `by`, a rule's `by`, the documents'
   history), so the history view and `drive tag ls THING` show what
   an agent did.
6. **Same permissions as the device.** The server is this device: it sees
   what the device's agent sees and can do nothing the CLI cannot. Live
   browsing stays read-only and confined to the device's volumes
   ([browse.md](browse.md)).

The server keeps its own copies of the documents (`mcp/state` in the
agent's folder, seeded from the agent's), so it never writes the files a
running `serve` writes, and starts offline at once.

## Tests

`crates/drive-agent/src/mcp/tests.rs`, against a node in the same process
and a device over a scratch folder:

| Test | Holds |
|---|---|
| `speaks_mcp_over_lines` | initialize (the client's name taken for the history), tools/list, prompts/get, resources/read, an unknown method, a line that is not JSON, a tool call with no Drive set up |
| `tools_are_described_and_none_destroys` | every tool there, described, with an object schema whose required arguments exist; `dry_run` on exactly the write tools; no tool named for deleting, trashing or switching rules; `destructiveHint` false |
| `nothing_here_destroys` | the module's source calls nothing that trashes, deletes tags, projects, parts, locations, devices, storages or rules, renames or removes files, makes a rule live, asks for undo, or purges |
| `reads_page_and_summarise` | pages of a 122-file folder, the page cap, unknown arguments and `..` refused, the summary's counts, largest and duplicates, search, copies |
| `writes_record_the_agent_and_dry_runs_change_nothing` | a dry run changes nothing and costs nothing; missing tags refused unless asked; a content tag shows on every copy; the agent is the author in `by` and in history; bad dates and duplicate projects refused |
| `the_write_cap_holds` | a call past the cap is refused whole; dry runs are free |
| `a_proposed_rule_is_off_and_a_dry_run_whatever_it_says` | a rule sent `enabled: true, mode: live` is saved off and a dry run by the agent; its dry run holds moves back; the run asked for is not live; the disk is untouched; trash rules, taken names and any "turn on" tool refused |
| `a_plan_refuses_what_destroys_and_turns_device_moves_into_rules` | delete, trash, overwrite, empty_trash, shred and moves into the Trash refused; planning and a dry-run apply change nothing; a Drive copy takes " (2)" and a move keeps every entry; device moves become rules off and dry runs; the disk is untouched; a plan applies once |

## Not yet

- Undo for what an agent did with tags and fields (the history has it; a
  tool would read it back).
- Moves across computers and into the Drive, as rules can do them
  ([rules.md](rules.md) "Not yet").
- Streaming progress for long calls, and resources for each location
  (an MCP client could then attach a folder's summary as context).

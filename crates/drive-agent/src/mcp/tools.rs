//! The tools: names, what each is for, their input schemas, and the one
//! place every call goes through. Also the `organize` prompt and the
//! resources.

use serde_json::{Value, json};

use super::ctx::Ctx;
use super::{graph, plan, read, write};

/// What the server tells the client about itself (`initialize`).
pub const INSTRUCTIONS: &str = "Drive: the owner's files on every computer (each computer's indexed folders, its disks, the Drive tree), with tags, fields, projects and file rules. Read freely; organise only through these tools, which never delete, trash or overwrite anything. Rules you propose stay off and a dry run until a person switches them on and live in the Drive app or CLI; say so when you propose one. Start with the `organize` prompt for the workflow. Name things as drive:PATH (the Drive tree), LOCATION:PATH (a computer's indexed folder), project:NAME, doc:ID or content:HASH. The graph_* tools follow links between notes, tags, tasks and projects.";

pub const SAFETY: &str = "\
# Drive's safety model for AI agents

- Nothing deletes, trashes or overwrites. No tool does; a plan's delete, trash, remove or overwrite operations are refused.
- Copies never overwrite: a taken name gets \" (2)\". A Drive copy shares the same verified blob.
- Moves inside the Drive tree only change where an entry is listed; every file stays.
- A computer's files are moved and copied only by rules. A rule an agent proposes is created OFF and as a DRY RUN; its preview shows what it would do. No tool can switch a rule on or make it live: a person does that, in the Drive app (Rules) or with `drive rules on NAME` and `drive rules live NAME`.
- Even a live rule moves a computer's files only on devices whose owner set `allow_rule_moves`, and a move is a verified copy with the original kept in the location's `.drive-trash`.
- Every write tool takes `dry_run: true` to show what it would do without doing it.
- A session has a cap on writes; listings are paged; calls time out.
- Every change records `agent: <client> on <device>` as its author in the documents' history.
";

pub const SEARCH_LANGUAGE: &str = "\
# The search language

- `drums`: tagged Drums (or a tag under it); a word that names no tag matches names.
- `drums snare` (AND), `drums OR keys`, `NOT archived`, brackets `(a OR b) NOT c`.
- `\"bass guitar\"` or `bass_guitar`: a tag with a space.
- `tag:mix` (only the tag), `name:take`, `path:Mixes/`, `path:*.wav` (glob over the whole path).
- `kind:audio` (broader kinds count: kind:audio finds MIDI), `special:untagged`, `special:empty`.
- `artist:jane`: a field named Artist containing jane.
- Lower case matches any case; a capital makes it case-sensitive.
- NOT, special: and field searches see the Drive tree and tagged files, not every file on every computer.
";

pub struct Def {
    pub name: &'static str,
    pub title: &'static str,
    pub description: &'static str,
    pub write: bool,
    pub schema: Value,
}

fn props(p: Value, required: &[&str]) -> Value {
    json!({ "type": "object", "properties": p, "required": required, "additionalProperties": false })
}

fn paged(mut p: Value) -> Value {
    p["offset"] = json!({ "type": "integer", "minimum": 0, "description": "Skip this many items (from next_offset of the page before)." });
    p["limit"] = json!({ "type": "integer", "minimum": 1, "description": "Items per page (default 50, at most the session's page size)." });
    p
}

fn thing(what: &str) -> Value {
    json!({ "type": "string", "description": format!("{what}: drive:PATH, LOCATION:PATH, project:NAME, doc:ID, content:HASH, or an absolute path of a file on this computer.") })
}

fn strings(what: &str) -> Value {
    json!({ "type": "array", "items": { "type": "string" }, "description": what })
}

pub fn defs() -> Vec<Def> {
    let mut v = vec![
        Def {
            name: "devices",
            title: "Computers",
            description: "Every computer (and phone, server) in the space: whether it is on now, its roles (workstation, storage-server), its disks with size and free space, and how many indexed folders it has. Start a survey here.",
            write: false,
            schema: props(json!({}), &[]),
        },
        Def {
            name: "locations",
            title: "Indexed folders",
            description: "The indexed folders (locations) of every computer: device, path, files, bytes, whether its device is on, when it was last indexed. Location names are how you name a computer's files (LOCATION:path).",
            write: false,
            schema: props(
                json!({ "device": { "type": "string", "description": "Only this device's (name or id)." } }),
                &[],
            ),
        },
        Def {
            name: "browse",
            title: "Browse a folder",
            description: "List a folder, a page at a time. Give `drive` (a folder of the Drive tree, \"\" for its top) to list the Drive as the mount shows it, adopted folders included (each says where its files are); `location` (+ `path`) to list a location from its index (works while its computer is off), or `device` to browse its disks live: no `root` lists the device's volumes; `root` (a volume's mount point from that list) + `path` lists a folder of it (read-only; the device must be on).",
            write: false,
            schema: props(
                paged(json!({
                    "drive": { "type": "string", "description": "A folder of the Drive tree (like Studio/Songs; \"\" for the top)." },
                    "location": { "type": "string", "description": "A location (name or id)." },
                    "device": { "type": "string", "description": "A device (name or id), to browse its disks live." },
                    "root": { "type": "string", "description": "With device: a volume's mount point, as `browse` with only `device` lists them." },
                    "path": { "type": "string", "description": "A folder relative to the location or root (\"\" for the top)." },
                    "hidden": { "type": "boolean", "description": "With device: include hidden files." }
                })),
                &[],
            ),
        },
        Def {
            name: "search",
            title: "Search everything",
            description: "Search every computer's indexed files, the Drive tree, documents and projects with Drive's search language: plain words match names; tag names, `tag:`, `name:`, `path:*.wav`, `kind:audio`, `special:untagged`, `field:value`, AND/OR/NOT and brackets (resource drive://search-language). Results carry where each file is, its device, whether that device is on, and how many copies exist.",
            write: false,
            schema: props(
                paged(
                    json!({ "query": { "type": "string", "description": "The search, e.g. `kind:audio path:Mixes/ NOT archived`." } }),
                ),
                &["query"],
            ),
        },
        Def {
            name: "file_info",
            title: "About one file",
            description: "Everything Drive knows about one thing: kind, size, dates, content key, every copy and where (device, location, online, site), its tags and fields, the projects it belongs to, and which sidecars (thumbnail, waveform) exist.",
            write: false,
            schema: props(json!({ "thing": thing("The file") }), &["thing"]),
        },
        Def {
            name: "folder_summary",
            title: "Summarise a folder",
            description: "A folder in numbers, from its index: files, folders, bytes, by kind and by extension, the largest files, the oldest and newest, and duplicates inside it. Give `location` (+ `path`), or `drive_path` for a folder of the Drive tree. Use before proposing how to organise a folder.",
            write: false,
            schema: props(
                json!({
                    "location": { "type": "string" },
                    "path": { "type": "string", "description": "Inside the location (\"\" for all of it)." },
                    "drive_path": { "type": "string", "description": "A Drive folder instead (\"\" for the whole Drive)." },
                    "top": { "type": "integer", "minimum": 1, "maximum": 50, "description": "How many largest/oldest/newest to list (default 10)." }
                }),
                &[],
            ),
        },
        Def {
            name: "duplicates",
            title: "Duplicates",
            description: "Files with the same content in several places, across every computer's indexes, most wasted space first. Narrow with `location` (+ `path`): groups with at least one copy there. Same content is judged by a sampled hash and the size (likely, not certain: confirm before acting).",
            write: false,
            schema: props(
                paged(json!({
                    "location": { "type": "string" },
                    "path": { "type": "string" },
                    "min_size": { "type": "integer", "minimum": 0, "description": "Ignore files smaller than this (bytes; default 1)." }
                })),
                &[],
            ),
        },
        Def {
            name: "projects",
            title: "Projects",
            description: "Every project: its parts (folders on whichever computer has them), its documents, the orgs it is shared with, and its tags.",
            write: false,
            schema: props(paged(json!({})), &[]),
        },
        Def {
            name: "tags",
            title: "Tags",
            description: "Every tag with its parents, shorthand, aliases, colour, category/hidden switches and how many things carry it; or, with `thing`, the tags and fields on that thing.",
            write: false,
            schema: props(
                paged(json!({ "thing": thing("Only what is on this thing") })),
                &[],
            ),
        },
        Def {
            name: "rules",
            title: "File rules",
            description: "Every file rule: on or off, dry run or live, what it does, who made it, and each device's last run. With `rule`, that rule in full (its JSON).",
            write: false,
            schema: props(
                json!({ "rule": { "type": "string", "description": "A rule's name or id." } }),
                &[],
            ),
        },
        Def {
            name: "rule_preview",
            title: "A rule's dry run",
            description: "What a rule would do, per device (and the node for the Drive tree): each file it matches and each step, held-back steps with why. `refresh` (default true) asks every device for a new dry run and waits up to `wait_seconds` for them; it never runs a rule live.",
            write: false,
            schema: props(
                paged(json!({
                    "rule": { "type": "string" },
                    "refresh": { "type": "boolean" },
                    "wait_seconds": { "type": "integer", "minimum": 0, "maximum": 60 }
                })),
                &["rule"],
            ),
        },
        Def {
            name: "journal",
            title: "What rules did",
            description: "The rules' journal, newest first: each action, on which device, its status, and whether it can be undone.",
            write: false,
            schema: props(
                paged(json!({
                    "rule": { "type": "string" },
                    "device": { "type": "string" }
                })),
                &[],
            ),
        },
        Def {
            name: "graph_search",
            title: "Search notes and things by their words",
            description: "Full-text search over this computer's graph of the space: notes' text, titles and aliases, file and folder names and paths, tag names, documents' titles, ranked best first. `tag` keeps only things under a tag (child tags too, Drive or frontmatter). Words only: no query syntax.",
            write: false,
            schema: props(
                paged(json!({
                    "query": { "type": "string", "description": "The words to find." },
                    "tag": { "type": "string", "description": "Only things under this tag (name, shorthand, alias, id, or a frontmatter tag)." }
                })),
                &["query"],
            ),
        },
        Def {
            name: "graph_backlinks",
            title: "What links here",
            description: "Everything that points at a thing in this computer's graph of the space: notes linking or embedding it ([[wikilinks]], relative Markdown links, frontmatter related:), things carrying a tag, tasks and events naming a document, projects holding it. Each with the edge (its kind and what was written). `coverage` says how many notes could be read here.",
            write: false,
            schema: props(
                paged(json!({ "thing": json!({ "type": "string", "description": "drive:PATH, LOCATION:PATH, project:NAME, doc:ID, content:HASH, a graph key (tag:ID, missing:NAME, label:NAME, task:ID, event:ID) or a name the graph knows (a note's title or alias, a file name)." }) })),
                &["thing"],
            ),
        },
        Def {
            name: "graph_neighbors",
            title: "Neighbours in the graph",
            description: "Everything within `depth` edges of a thing (links, embeds, tags, tag parents, tasks, events, projects), either way, nearest first.",
            write: false,
            schema: props(
                paged(json!({
                    "thing": thing("Where to start"),
                    "depth": { "type": "integer", "minimum": 1, "maximum": 4, "description": "How many edges away (default 1)." }
                })),
                &["thing"],
            ),
        },
        Def {
            name: "graph_path",
            title: "How two things connect",
            description: "A shortest chain of edges (links, tags, tasks, projects, either way) from one thing to another, or found: false.",
            write: false,
            schema: props(
                json!({
                    "from": thing("Where the path starts"),
                    "to": thing("Where it ends"),
                    "max": { "type": "integer", "minimum": 1, "maximum": 8, "description": "The longest path looked for (default 8)." }
                }),
                &["from", "to"],
            ),
        },
        Def {
            name: "graph_tag",
            title: "Everything under a tag",
            description: "Every thing carrying a tag or any tag under it, by a Drive tag (drive:tags) or a note's frontmatter `tags:` (one that names no Drive tag is found as written).",
            write: false,
            schema: props(
                paged(json!({ "tag": { "type": "string", "description": "A tag's name, shorthand, alias or id, or a frontmatter tag." } })),
                &["tag"],
            ),
        },
        Def {
            name: "link",
            title: "Link two things",
            description: "Say that one thing refers to another, in an app's words (`kind`: uses, built-on, in-setlist; empty: a plain reference): an episode uses a song, a preset is built on another. Kept by what the things are (as tags are), synced to every device, and read by the graph_* tools.",
            write: true,
            schema: props(
                json!({
                    "from": thing("The thing that refers"),
                    "to": thing("What it refers to"),
                    "kind": { "type": "string", "description": "What the link says (one line); empty: a plain reference." }
                }),
                &["from", "to"],
            ),
        },
        Def {
            name: "unlink",
            title: "Unlink two things",
            description: "Take away a link made with `link` (the same from, to and kind). The things themselves are untouched.",
            write: true,
            schema: props(
                json!({
                    "from": thing("The thing that refers"),
                    "to": thing("What it refers to"),
                    "kind": { "type": "string", "description": "The link's kind, as it was made." }
                }),
                &["from", "to"],
            ),
        },
        Def {
            name: "tag",
            title: "Tag things",
            description: "Put tags on things (a computer's file is tagged by its content, so every copy shows it). Tags are named by name, shorthand, alias or id; `create_missing` makes tags that do not exist yet (else they are refused).",
            write: true,
            schema: props(
                json!({
                    "things": strings("What to tag (drive:PATH, LOCATION:PATH, project:NAME, doc:ID, content:HASH)."),
                    "tags": strings("Tags to put on each."),
                    "create_missing": { "type": "boolean" }
                }),
                &["things", "tags"],
            ),
        },
        Def {
            name: "untag",
            title: "Untag things",
            description: "Take tags off things (the tags themselves stay).",
            write: true,
            schema: props(
                json!({ "things": strings("What to untag."), "tags": strings("Tags to take off.") }),
                &["things", "tags"],
            ),
        },
        Def {
            name: "set_field",
            title: "Set a field",
            description: "Set a named value on a thing (Artist, Source, Date created, Notes, …): changed if the thing has a field of that name, else added. Kinds: text, multiline, date (YYYY-MM-DD), datetime (YYYY-MM-DDTHH:MM).",
            write: true,
            schema: props(
                json!({
                    "thing": thing("The thing"),
                    "name": { "type": "string" },
                    "value": { "type": "string" },
                    "kind": { "type": "string", "enum": ["text", "multiline", "date", "datetime"] }
                }),
                &["thing", "name", "value"],
            ),
        },
        Def {
            name: "create_tag",
            title: "Make a tag",
            description: "Make a tag, under parent tags, with a colour, shorthand and aliases. A tag of that name under the same parents is refused (use it instead).",
            write: true,
            schema: props(
                json!({
                    "name": { "type": "string" },
                    "parents": strings("Parent tags (name or id)."),
                    "color": { "type": "string", "enum": ["none", "red", "orange", "amber", "yellow", "lime", "green", "teal", "cyan", "blue", "indigo", "violet", "pink", "brown", "grey"] },
                    "shorthand": { "type": "string" },
                    "aliases": strings("Other names it is found by."),
                    "category": { "type": "boolean", "description": "Groups a thing's tags under it in the inspector." },
                    "hidden": { "type": "boolean", "description": "Things with it leave search results unless a search names it." }
                }),
                &["name"],
            ),
        },
        Def {
            name: "add_to_project",
            title: "Add to a project",
            description: "Add a folder of a location to a project as a part (`location`, `path`, `label`), or an app document (`doc`).",
            write: true,
            schema: props(
                json!({
                    "project": { "type": "string" },
                    "location": { "type": "string" },
                    "path": { "type": "string" },
                    "label": { "type": "string", "description": "What the part is: Audio, Footage, Client mixes." },
                    "doc": { "type": "string", "description": "An app document id instead (studio:song:x)." }
                }),
                &["project"],
            ),
        },
        Def {
            name: "create_project",
            title: "Start a project",
            description: "Start a project (a song, a video, a sample library: one piece of work whose parts can be on any computer). A project of the same name is refused.",
            write: true,
            schema: props(
                json!({
                    "name": { "type": "string" },
                    "kind": { "type": "string", "description": "A schema.org type: MusicRecording, Movie, Collection, Project (default)." }
                }),
                &["name"],
            ),
        },
        Def {
            name: "index_folder",
            title: "Index a folder",
            description: "Make a folder of a computer a location, so Drive indexes it (read-only to the folder: indexing only reads). Find `root` and `path` with `browse` on the device. The device must be on.",
            write: true,
            schema: props(
                json!({
                    "device": { "type": "string" },
                    "root": { "type": "string", "description": "A volume's mount point (from browse)." },
                    "path": { "type": "string", "description": "The folder inside it." },
                    "name": { "type": "string", "description": "The new location's name." }
                }),
                &["device", "root", "path", "name"],
            ),
        },
        Def {
            name: "propose_rule",
            title: "Propose a file rule",
            description: "Make a file rule (docs/rules.md's JSON: name, scope, trigger, when, actions; locations, devices, tags and projects by name). It is ALWAYS created off and as a dry run, and its dry run is returned. Only a person can switch it on and live, in the Drive app or with `drive rules on|live NAME`: ask them to. Trash actions are refused.",
            write: true,
            schema: props(
                json!({
                    "rule": { "type": "object", "description": "The rule: {name, scope: {devices: {by: all|roles|named, list}, places: [{kind: location, location, path} | {kind: drive, path} | {kind: project, project} | {kind: tag, tag}]}, trigger: {on_change, every_minutes}, when: {all, groups: [{all, conditions: [{not, test: {test: name|path|extension|kind|size|date|tag|field|project|copies|held-by, …}}]}]}, actions: [{do: add-tags|remove-tags|set-field|add-to-project|copy|move|rename|sidecars|highly-available, …}]}." },
                    "wait_seconds": { "type": "integer", "minimum": 0, "maximum": 60, "description": "How long to wait for devices' dry runs (default 15)." }
                }),
                &["rule"],
            ),
        },
        Def {
            name: "plan",
            title: "Plan a reorganisation",
            description: "Preview a one-off reorganisation: a batch of operations, each `{op, …}`: tag {thing, tags}, untag {thing, tags}, set_field {thing, name, value}, add_to_project {project, location, path} , copy {from, to, name?}, move {from, to, name?}, rename {from, name}. `to` is a folder: drive:FOLDER or LOCATION:FOLDER. Each comes back as `apply` (done by apply_plan), `rule` (a computer's files: becomes a proposed rule, off and a dry run) or `refused` with why (delete, trash, overwrite and anything unknown are always refused). Returns a plan id for apply_plan.",
            write: false,
            schema: props(
                json!({
                    "title": { "type": "string" },
                    "operations": { "type": "array", "items": { "type": "object" } }
                }),
                &["operations"],
            ),
        },
        Def {
            name: "apply_plan",
            title: "Apply a plan",
            description: "Carry out a plan made by `plan` in this session: its `apply` operations (tags, fields, projects, Drive-tree copies and moves, which never overwrite), and its `rule` operations as proposed rules (off, dry run). Refused operations are skipped. Show the person the plan first.",
            write: true,
            schema: props(json!({ "plan_id": { "type": "string" } }), &["plan_id"]),
        },
    ];
    for d in &mut v {
        if d.write {
            d.schema["properties"]["dry_run"] = json!({ "type": "boolean", "description": "Say what would happen; change nothing." });
        }
    }
    v
}

pub fn find(name: &str) -> Option<&'static str> {
    defs().into_iter().find(|d| d.name == name).map(|d| d.name)
}

/// `tools/list`.
pub fn list() -> Vec<Value> {
    defs()
        .into_iter()
        .map(|d| {
            json!({
                "name": d.name,
                "title": d.title,
                "description": d.description,
                "inputSchema": d.schema,
                "annotations": {
                    "title": d.title,
                    "readOnlyHint": !d.write,
                    "destructiveHint": false,
                    "idempotentHint": !d.write,
                    "openWorldHint": false
                }
            })
        })
        .collect()
}

/// Every call: the arguments checked against the tool's schema's names,
/// then the tool.
pub async fn run(ctx: &Ctx, name: &str, args: &Value) -> Result<Value, String> {
    if let Some(def) = defs().into_iter().find(|d| d.name == name)
        && let (Some(obj), Some(known)) = (args.as_object(), def.schema["properties"].as_object())
    {
        let unknown: Vec<&String> = obj.keys().filter(|k| !known.contains_key(*k)).collect();
        if !unknown.is_empty() {
            return Err(format!(
                "{name} takes no {}; it takes {}",
                unknown
                    .iter()
                    .map(|s| s.as_str())
                    .collect::<Vec<_>>()
                    .join(", "),
                known.keys().cloned().collect::<Vec<_>>().join(", ")
            ));
        }
        for r in def.schema["required"].as_array().into_iter().flatten() {
            let r = r.as_str().unwrap_or_default();
            if obj.get(r).is_none_or(Value::is_null) {
                return Err(format!("{name} needs {r}"));
            }
        }
    }
    match name {
        "devices" => read::devices(ctx).await,
        "locations" => read::locations(ctx, args).await,
        "browse" => read::browse(ctx, args).await,
        "search" => read::search(ctx, args).await,
        "file_info" => read::file_info(ctx, args).await,
        "folder_summary" => read::folder_summary(ctx, args).await,
        "duplicates" => read::duplicates(ctx, args).await,
        "projects" => read::projects(ctx, args).await,
        "tags" => read::tags(ctx, args).await,
        "rules" => read::rules(ctx, args).await,
        "rule_preview" => read::rule_preview(ctx, args).await,
        "journal" => read::journal(ctx, args).await,
        "graph_search" => graph::search(ctx, args).await,
        "graph_backlinks" => graph::backlinks(ctx, args).await,
        "graph_neighbors" => graph::neighbors(ctx, args).await,
        "graph_path" => graph::path(ctx, args).await,
        "graph_tag" => graph::tagged(ctx, args).await,
        "link" => graph::link(ctx, args, true).await,
        "unlink" => graph::link(ctx, args, false).await,
        "tag" => write::tag(ctx, args, true).await,
        "untag" => write::tag(ctx, args, false).await,
        "set_field" => write::set_field(ctx, args).await,
        "create_tag" => write::create_tag(ctx, args).await,
        "add_to_project" => write::add_to_project(ctx, args).await,
        "create_project" => write::create_project(ctx, args).await,
        "index_folder" => write::index_folder(ctx, args).await,
        "propose_rule" => write::propose_rule(ctx, args).await,
        "plan" => plan::plan(ctx, args).await,
        "apply_plan" => plan::apply(ctx, args).await,
        other => Err(format!("no tool {other}")),
    }
}

// ── the prompt ──────────────────────────────────────────────────────────

pub fn organize_prompt_info() -> Value {
    json!({
        "name": "organize",
        "title": "Organise files with Drive",
        "description": "How to survey, summarise and organise the owner's files across their computers with Drive, safely.",
        "arguments": [
            { "name": "focus", "description": "What to organise (a location, a folder, a kind of file, a project); empty: start with a survey.", "required": false }
        ]
    })
}

pub fn organize_prompt(params: &Value) -> Result<Value, (i64, String)> {
    if params["name"].as_str() != Some("organize") {
        return Err((-32602, "no such prompt (there is `organize`)".into()));
    }
    let focus = params["arguments"]["focus"].as_str().unwrap_or("").trim();
    let focus = if focus.is_empty() {
        "Start with a survey of everything.".to_string()
    } else {
        format!("Focus on: {focus}.")
    };
    let text = format!(
        "You are organising the owner's files with Drive, across all their computers. {focus}\n\n\
{SAFETY}\n\
# A good workflow\n\n\
1. Survey: `devices` (which computers are on, their disks and free space), `locations` (the indexed folders), `projects`, `tags`, `rules`.\n\
2. Summarise before proposing anything: `folder_summary` on the folders that matter, `duplicates`, `search` (e.g. `special:untagged kind:audio`), `file_info` on examples. Tell the person what you found in a few lines, with numbers.\n\
3. Propose: tags (reuse existing ones; `create_tag` only what is missing, under sensible parents), fields, projects for pieces of work, and rules for anything that should keep happening. Say what each would change and why.\n\
4. Preview: every write tool takes `dry_run: true`; a one-off batch is `plan` (it shows what applies, what becomes a rule and what is refused); a rule is `propose_rule` (it comes back with its dry run per device; `rule_preview` refreshes it).\n\
5. Ask, then act: apply only what the person agreed to (`apply_plan`, `tag`, …). For rules, tell them the rule's name and that it is off and a dry run: they switch it on and live themselves in the Drive app (Rules) or with `drive rules on NAME` and `drive rules live NAME`. Moves of a computer's files also need `allow_rule_moves` on that device.\n\n\
Name things as drive:PATH, LOCATION:PATH, project:NAME, doc:ID or content:HASH. Listings are paged (`offset`, `limit`, `next_offset`). Same content is judged by a sampled hash: confirm before calling two files the same."
    );
    Ok(json!({
        "description": "Organise files with Drive, safely",
        "messages": [ { "role": "user", "content": { "type": "text", "text": text } } ]
    }))
}

// ── resources ───────────────────────────────────────────────────────────

pub fn resources() -> Vec<Value> {
    vec![
        json!({ "uri": "drive://safety", "name": "safety", "title": "Drive's safety model for agents", "mimeType": "text/markdown" }),
        json!({ "uri": "drive://search-language", "name": "search-language", "title": "Drive's search language", "mimeType": "text/markdown" }),
    ]
}

pub fn read_resource(params: &Value) -> Result<Value, (i64, String)> {
    let uri = params["uri"].as_str().unwrap_or_default();
    let text = match uri {
        "drive://safety" => SAFETY,
        "drive://search-language" => SEARCH_LANGUAGE,
        _ => return Err((-32002, format!("no resource {uri}"))),
    };
    Ok(json!({ "contents": [ { "uri": uri, "mimeType": "text/markdown", "text": text } ] }))
}

pub fn opt_str<'a>(args: &'a Value, k: &str) -> Option<&'a str> {
    args[k].as_str().map(str::trim).filter(|s| !s.is_empty())
}

pub fn str_list(args: &Value, k: &str) -> Vec<String> {
    match &args[k] {
        Value::Array(v) => v
            .iter()
            .filter_map(|x| {
                x.as_str()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string)
            })
            .collect(),
        Value::String(s) if !s.trim().is_empty() => vec![s.trim().to_string()],
        _ => Vec::new(),
    }
}

pub fn dry(args: &Value) -> bool {
    args["dry_run"].as_bool().unwrap_or(false)
}

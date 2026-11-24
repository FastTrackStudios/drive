//! `drive tag …`: tags and fields from the command line, for scripts
//! and agents. The same document the Drive app edits (`<space>/drive:tags`),
//! so a tag added here shows in every browser at once.
//!
//! ```text
//! drive tag ls                          # every tag, nested, with counts
//! drive tag ls THING                    # the tags and fields on a thing
//! drive tag add THING TAG…              # makes a tag that does not exist yet
//! drive tag rm THING TAG…
//! drive tag new NAME [--parent TAG]… [--color red] [--shorthand S] [--alias A]…
//!                          [--category] [--hidden]
//! drive tag field THING NAME VALUE [--kind text|multiline|date|datetime]
//! drive tag search QUERY…               # the search language (docs/tags.md)
//! ```
//!
//! A THING is a file on this computer (tagged by its content, so every copy
//! anywhere shows the tag), a path in the Drive (`drive:Songs/mix.wav`), a
//! file of a location (`LOCATION:path/in/it`), a project by name
//! (`project:NAME`), an app document (`doc:studio:song:x`), or a raw key
//! (`file:<tree id>`, `content:<hash>`).

use std::path::Path;

use drive_files::{space, tree};
use drive_spine::kinds::Kinds;
use drive_spine::tags::query::{self, Search, Subject};
use drive_spine::tags::{self, Color, FieldKind, Library, NewTag, Target};
use loro::LoroDoc;

use crate::agent::Agent;
use crate::{flag, human, node_get, out, settle, start};

struct Tags {
    a: Agent,
    id: String,
    doc: LoroDoc,
    by: String,
}

impl Tags {
    async fn open(dir: &Path) -> Result<Tags, String> {
        let a = start(dir).await?;
        let (space, by) = {
            let cfg = a.cfg.lock().expect("cfg");
            (
                cfg.space.clone(),
                cfg.signed_in_as
                    .clone()
                    .unwrap_or_else(|| cfg.device_name.clone()),
            )
        };
        let id = tags::tags_doc(&space);
        let doc = a.drive.open(&id).await;
        let _ = tokio::time::timeout(std::time::Duration::from_secs(4), a.drive.synced(&id)).await;
        Ok(Tags { a, id, doc, by })
    }

    async fn done(&self) {
        self.a.drive.commit(&self.id);
        settle(&self.a).await;
    }

    /// A tag by name, shorthand, alias or id; made when `create` and none is.
    fn tag(&self, name: &str, create: bool) -> Result<String, String> {
        let lib = Library::load(&self.doc);
        if lib.get(name).is_some() {
            return Ok(name.to_string());
        }
        match lib.resolve(name).as_slice() {
            [one] => Ok(one.clone()),
            [] if create => tags::create(
                &self.doc,
                NewTag {
                    name,
                    ..NewTag::default()
                },
                &self.by,
                drive_files::now_ms(),
            )
            .map_err(|e| e.to_string()),
            [] => Err(format!("no tag {name}")),
            many => Err(format!(
                "{name} could be {}; give its id",
                many.iter()
                    .map(|id| format!("{} ({id})", lib.label(id)))
                    .collect::<Vec<_>>()
                    .join(", ")
            )),
        }
    }

    async fn target(&self, thing: &str) -> Result<Target, String> {
        if let Some(t) = Target::parse(thing)
            && !matches!(t, Target::Project(_))
        {
            return Ok(t);
        }
        if let Some(name) = thing.strip_prefix("project:") {
            let p = drive_files::projects::projects(&self.a.space)
                .into_iter()
                .find(|p| p.name == name || p.id == name)
                .ok_or(format!("no project {name}"))?;
            return Ok(Target::Project(p.id));
        }
        if let Some(path) = thing.strip_prefix("drive:") {
            let e = tree::resolve(&self.a.files, path)
                .ok_or(format!("no {path} in the Drive"))?
                .ok_or("the Drive's top folder cannot be tagged")?;
            return Ok(Target::File(tree::id_string(e.id)));
        }
        let local = Path::new(thing);
        if local.is_file() {
            let h = drive_files::hash::sampled(local).map_err(|e| format!("{thing}: {e}"))?;
            return Ok(Target::Content(h));
        }
        if let Some((lname, path)) = thing.split_once(':')
            && let Some(loc) = space::locations(&self.a.space)
                .into_iter()
                .find(|l| l.name == lname || l.id == lname)
        {
            self.a.sync_indexes().await;
            let e = self
                .a
                .index_entry(&loc.id, path)
                .await
                .ok_or(format!("{path} is not in {lname} (or it has no index yet)"))?;
            if e.dir || e.sampled.is_empty() {
                return Err(format!(
                    "{path} has no content hash (a folder, or unreadable): only files can be tagged on a computer"
                ));
            }
            return Ok(Target::Content(e.sampled));
        }
        if let Some(Some(e)) = tree::resolve(&self.a.files, thing) {
            return Ok(Target::File(tree::id_string(e.id)));
        }
        Err(format!(
            "{thing}: not a file here, a Drive path, LOCATION:PATH, project:NAME or doc:ID"
        ))
    }
}

fn print_tree(
    lib: &Library,
    counts: &std::collections::HashMap<String, usize>,
    id: &str,
    depth: usize,
    seen: &mut Vec<String>,
) {
    let Some(t) = lib.get(id) else { return };
    let mut notes = Vec::new();
    if !t.shorthand.is_empty() {
        notes.push(format!("[{}]", t.shorthand));
    }
    if !t.aliases.is_empty() {
        notes.push(format!("aka {}", t.aliases.join(", ")));
    }
    if t.color != Color::None {
        notes.push(t.color.id().to_string());
    }
    if t.category {
        notes.push("category".into());
    }
    if t.hidden {
        notes.push("hidden".into());
    }
    out(format!(
        "{}{} ({}){}  {}",
        "  ".repeat(depth),
        t.name,
        counts.get(id).copied().unwrap_or(0),
        if notes.is_empty() {
            String::new()
        } else {
            format!("  {}", notes.join(" · "))
        },
        t.id
    ));
    if seen.contains(&t.id) {
        return;
    }
    seen.push(t.id.clone());
    for c in lib.children(id) {
        print_tree(lib, counts, &c.id, depth + 1, seen);
    }
    seen.pop();
}

pub async fn command(dir: &Path, args: &[String]) -> Result<(), String> {
    let sub = args.get(1).map(String::as_str).unwrap_or("ls");
    let rest: Vec<String> = args.iter().skip(2).cloned().collect();
    match sub {
        "ls" => {
            let t = Tags::open(dir).await?;
            let lib = Library::load(&t.doc);
            if let Some(thing) = rest.first() {
                let target = t.target(thing).await?;
                let on = lib.on(&target);
                out(target.key());
                for a in &on.tags {
                    out(format!("  #{}  (by {}, {})", lib.label(&a.tag), a.by, a.at));
                }
                for f in &on.fields {
                    out(format!("  {}: {}  ({})", f.name, f.value, f.kind.label()));
                }
                if on.is_empty() {
                    out("  nothing on it");
                }
            } else {
                let counts = lib.counts();
                for r in lib.roots() {
                    print_tree(&lib, &counts, &r.id, 0, &mut Vec::new());
                }
                for (c, p) in lib.broken() {
                    out(format!(
                        "! {} under {} closed a cycle and is ignored",
                        lib.label(c),
                        lib.label(p)
                    ));
                }
            }
        }
        "add" | "rm" => {
            let (thing, names) = rest.split_first().ok_or("tag add|rm THING TAG…")?;
            if names.is_empty() {
                return Err("tag add|rm THING TAG…".into());
            }
            let t = Tags::open(dir).await?;
            let target = t.target(thing).await?;
            for n in names {
                let id = t.tag(n, sub == "add")?;
                if sub == "add" {
                    tags::tag(&t.doc, &target, &id, &t.by, drive_files::now_ms())
                } else {
                    tags::untag(&t.doc, &target, &id)
                }
                .map_err(|e| e.to_string())?;
            }
            t.done().await;
            let lib = Library::load(&t.doc);
            let on: Vec<String> = lib
                .on(&target)
                .tags
                .iter()
                .map(|a| lib.label(&a.tag))
                .collect();
            out(format!("{}: {}", target.key(), on.join(", ")));
        }
        "new" => {
            let name = rest.first().ok_or("tag new NAME")?;
            let t = Tags::open(dir).await?;
            let mut parents = Vec::new();
            for (i, a) in rest.iter().enumerate() {
                if a == "--parent"
                    && let Some(p) = rest.get(i + 1)
                {
                    parents.push(t.tag(p, false)?);
                }
            }
            let id = tags::create(
                &t.doc,
                NewTag {
                    name,
                    shorthand: &flag(&rest, "--shorthand").unwrap_or_default(),
                    color: Color::parse(&flag(&rest, "--color").unwrap_or_default()),
                    parents: &parents,
                    category: rest.iter().any(|a| a == "--category"),
                },
                &t.by,
                drive_files::now_ms(),
            )
            .map_err(|e| e.to_string())?;
            if rest.iter().any(|a| a == "--hidden") {
                tags::set_hidden(&t.doc, &id, true).map_err(|e| e.to_string())?;
            }
            for (i, a) in rest.iter().enumerate() {
                if a == "--alias"
                    && let Some(al) = rest.get(i + 1)
                {
                    tags::add_alias(&t.doc, &id, al).map_err(|e| e.to_string())?;
                }
            }
            t.done().await;
            out(format!("{name} ({id})"));
        }
        "field" => {
            let [thing, name, value, ..] = rest.as_slice() else {
                return Err(
                    "tag field THING NAME VALUE [--kind text|multiline|date|datetime]".into(),
                );
            };
            let kind = FieldKind::parse(&flag(&rest, "--kind").unwrap_or_default());
            let t = Tags::open(dir).await?;
            let target = t.target(thing).await?;
            let lib = Library::load(&t.doc);
            let now = drive_files::now_ms();
            match lib
                .on(&target)
                .fields
                .iter()
                .find(|f| f.name.eq_ignore_ascii_case(name))
            {
                Some(f) => tags::set_field(&t.doc, &target, &f.id, value, &t.by, now),
                None => tags::add_field(&t.doc, &target, name, kind, value, &t.by, now).map(|_| ()),
            }
            .map_err(|e| e.to_string())?;
            t.done().await;
            out(format!("{}: {name} = {value}", target.key()));
        }
        "search" => {
            let q = rest.join(" ");
            let t = Tags::open(dir).await?;
            search(&t, &q).await?;
        }
        other => return Err(format!("tag {other}? (ls, add, rm, new, field, search)")),
    }
    Ok(())
}

async fn search(t: &Tags, q: &str) -> Result<(), String> {
    let lib = Library::load(&t.doc);
    let Some(s) = Search::new(q, &lib).map_err(|e| e.message)? else {
        return Err("search for what?".into());
    };
    let kinds = Kinds::load(&t.a.spine);
    let mut found = 0;
    // The Drive.
    for (path, e) in tree::files(&t.a.files) {
        let on = lib.on(&Target::File(tree::id_string(e.id)));
        let k = kinds.of_file(&e.name, false);
        let words = query::kind_words(&kinds, k);
        if s.matches(&Subject {
            name: &e.name,
            path: &path,
            kinds: &words,
            on: &on,
        }) {
            found += 1;
            out(format!("  drive  {:>9}  {path}", human(e.size)));
        }
    }
    // Every computer's files: the tagged ones, and what names and kinds find.
    let cfg = t.a.cfg.lock().expect("cfg").clone();
    let hints = s.hints();
    let mut asks: Vec<Vec<(&str, String)>> = Vec::new();
    if hints.tagged {
        let hashes: Vec<String> = lib
            .targets()
            .filter_map(|(t, _)| match t {
                Target::Content(h) => Some(h),
                _ => None,
            })
            .collect();
        if !hashes.is_empty() {
            asks.push(vec![("sampled", hashes.join(","))]);
        }
    }
    for w in &hints.words {
        asks.push(vec![("q", w.clone())]);
    }
    for k in &hints.kinds {
        asks.push(vec![("exts", query::kind_extensions(&kinds, k).join(","))]);
    }
    let mut seen = std::collections::HashSet::new();
    for ask in asks {
        let mut params: Vec<(&str, &str)> = vec![("space", cfg.space.as_str()), ("limit", "500")];
        params.extend(ask.iter().map(|(k, v)| (*k, v.as_str())));
        let v = node_get(&cfg, "/search", &params).await?;
        for h in v["results"].as_array().into_iter().flatten() {
            let path = h["path"].as_str().unwrap_or_default();
            let loc = h["location"].as_str().unwrap_or_default();
            if h["dir"].as_bool() == Some(true) || !seen.insert(format!("{loc}/{path}")) {
                continue;
            }
            let name = h["name"].as_str().unwrap_or_default();
            let on = lib.on(&Target::Content(
                h["sampled"].as_str().unwrap_or_default().to_string(),
            ));
            let k = kinds.of_file(name, false);
            let words = query::kind_words(&kinds, k);
            if s.matches(&Subject {
                name,
                path,
                kinds: &words,
                on: &on,
            }) {
                found += 1;
                out(format!(
                    "{} {:<14} {:<14} {:>9}  {path}",
                    if h["online"].as_bool() == Some(true) {
                        "●"
                    } else {
                        "○"
                    },
                    h["device_name"].as_str().unwrap_or("?"),
                    h["location_name"].as_str().unwrap_or("?"),
                    human(h["size"].as_u64().unwrap_or(0)),
                ));
            }
        }
    }
    // Documents and projects carry tags too.
    let projects = drive_files::projects::projects(&t.a.space);
    for (target, on) in lib.targets() {
        let (label, name) = match &target {
            Target::Doc(id) => ("doc", id.clone()),
            Target::Project(id) => (
                "project",
                projects
                    .iter()
                    .find(|p| p.id == *id)
                    .map_or(id.clone(), |p| p.name.clone()),
            ),
            _ => continue,
        };
        if s.matches(&Subject {
            name: &name,
            path: &name,
            kinds: &[],
            on,
        }) {
            found += 1;
            out(format!("  {label:<6} {name}"));
        }
    }
    if hints.open {
        out(
            "(files on computers are searched by their tags, names and kinds; NOT and special: look at those and the Drive)",
        );
    }
    out(format!("{found} found"));
    Ok(())
}

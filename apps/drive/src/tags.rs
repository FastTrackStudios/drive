//! Tags and fields in the browser: a thing's tags and fields in the
//! inspector (add a tag from the keyboard), the Tags place in the sidebar
//! (every tag, nested, with counts), a tag's things, the tag manager, and
//! the search box's tag language. The model is [`drive_spine::tags`].

use std::collections::HashSet;

use dioxus::prelude::*;
use drive_dioxus::{use_doc, use_docs, use_drive};
use drive_files::{files_doc, space_doc, tree};
use drive_spine as spine;
use drive_spine::kinds::Kinds;
use drive_spine::tags::query::{self, Search, Subject};
use drive_spine::tags::{self, Color, FieldKind, Library, NewTag, Tag, Target};

use crate::browser::{
    Item, Items, Place, Src, app_label, enc, get, hit_item, icon, online, tree_item, use_nav,
};

/// Tags' sheet, loaded once by the app (a sheet a component loads goes with it).
pub const STYLE: Asset = asset!("/assets/tags.css");

fn now() -> i64 {
    crate::host::now_ms() as i64
}

/// Who an edit is by: the signed-in person, or this browser's name.
fn me() -> String {
    let ctx = use_drive();
    if ctx.who.is_empty() {
        ctx.me.peek().name.clone()
    } else {
        ctx.who.clone()
    }
}

/// What a browser item is tagged as (`None`: it cannot be: a folder on a
/// computer, or a file not yet read).
pub fn target_of(item: &Item) -> Option<Target> {
    match &item.src {
        Src::Tree { id, .. } => Some(Target::File(id.clone())),
        Src::Index { sampled, .. } if !item.dir && !sampled.is_empty() => {
            Some(Target::Content(sampled.clone()))
        }
        Src::Index { .. } | Src::Browse { .. } | Src::Logical { .. } => None,
        Src::Doc { id, .. } => Some(Target::Doc(id.clone())),
    }
}

fn chip_style(t: &Tag) -> String {
    format!("--tag: {}", t.color.hex())
}

// ── a thing's tags and fields ───────────────────────────────────────────

/// The inspector's Tags section for an item.
#[component]
pub fn ItemTags(space: String, item: Item) -> Element {
    let Some(target) = target_of(&item) else {
        return rsx! {
            section { class: "isec",
                h4 { "Tags" }
                p { class: "hint",
                    if item.dir { "Folders on computers are not tagged yet: tag the files in them." } else { "This file has not been read yet; it can be tagged once its computer indexes it." }
                }
            }
        };
    };
    let note = match &target {
        Target::Content(_) if item.copies > 1 => Some(format!(
            "Tagged by content: the same tags show on all {} copies.",
            item.copies
        )),
        Target::Content(_) => {
            Some("Tagged by content: copies, moves and renames keep the tags.".to_string())
        }
        _ => None,
    };
    rsx! {
        section { class: "isec",
            h4 { "Tags" }
            TargetTags { key: "{target.key()}", space, target, note }
        }
    }
}

/// A thing's tags (grouped by category), the keyboard adder, and its fields.
#[component]
pub fn TargetTags(space: String, target: Target, note: Option<String>) -> Element {
    let nav = use_nav();
    let doc = use_doc(tags::tags_doc(&space));
    let by = me();
    let lib = Library::load(&doc.loro());
    let on = lib.on(&target);
    let groups = lib.grouped(&on.tag_ids());
    let mut adding = use_signal(|| false);
    rsx! {
        div { class: "tagbox",
            for (cat , ids) in groups {
                div { key: "{cat.clone().unwrap_or_default()}", class: "taggroup",
                    if let Some(c) = cat.as_ref().and_then(|c| lib.get(c)) {
                        span { class: "tagcat", style: chip_style(c), "{c.name}" }
                    }
                    div { class: "tagchips",
                        for id in ids {
                            if let Some(t) = lib.get(&id) {
                                {
                                    let (doc, target, tid, tid2) = (doc.clone(), target.clone(), id.clone(), id.clone());
                                    let label = lib.label(&id);
                                    rsx! {
                                        span { key: "{id}", class: "tagchip", style: chip_style(t),
                                            button {
                                                class: "tagname",
                                                title: "Everything tagged {label}",
                                                onclick: move |_| nav.go(Place::Tag(tid2.clone())),
                                                "{label}"
                                            }
                                            button {
                                                class: "tagx",
                                                title: "Remove {label}",
                                                onclick: move |_| {
                                                    doc.edit(|d| tags::untag(d, &target, &tid).ok());
                                                },
                                                "×"
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
            if adding() {
                TagAdder {
                    space: space.clone(),
                    target: target.clone(),
                    on_close: move |_| adding.set(false),
                }
            } else {
                button { class: "tagadd", onclick: move |_| adding.set(true), "+ Tag" }
            }
            if let Some(n) = note {
                p { class: "hint", "{n}" }
            }
        }
        Fields { space, target, by }
    }
}

/// Type a tag: Enter adds the top match (or makes the tag), Enter on
/// nothing closes, arrows pick another match, Escape closes.
#[component]
fn TagAdder(space: String, target: Target, on_close: EventHandler<()>) -> Element {
    let doc = use_doc(tags::tags_doc(&space));
    let by = me();
    let mut text = use_signal(String::new);
    let mut pick = use_signal(|| 0usize);
    let lib = Library::load(&doc.loro());
    let on: HashSet<String> = lib.on(&target).tag_ids().into_iter().collect();
    let q = text();
    let matches: Vec<(String, String, Color)> = lib
        .suggest(&q)
        .into_iter()
        .filter(|t| !on.contains(&t.id))
        .take(6)
        .map(|t| (t.id.clone(), lib.label(&t.id), t.color))
        .collect();
    let exact = !lib.resolve(&q).is_empty();
    let create = !q.trim().is_empty() && !exact;
    let n = matches.len() + usize::from(create);
    let sel = if n == 0 { 0 } else { pick().min(n - 1) };
    let add = {
        let (doc, target, by) = (doc.clone(), target.clone(), by.clone());
        move |id: Option<String>, name: String| {
            doc.edit(|d| {
                let id = match id {
                    Some(id) => Some(id),
                    None => tags::create(
                        d,
                        NewTag {
                            name: &name,
                            ..NewTag::default()
                        },
                        &by,
                        now(),
                    )
                    .ok(),
                };
                if let Some(id) = id {
                    let _ = tags::tag(d, &target, &id, &by, now());
                }
            });
        }
    };
    let choices: Vec<(Option<String>, String)> = matches
        .iter()
        .map(|(id, label, _)| (Some(id.clone()), label.clone()))
        .chain(create.then(|| (None, q.trim().to_string())))
        .collect();
    let key_choices = choices.clone();
    let key_add = add.clone();
    rsx! {
        div { class: "tagadder",
            input {
                class: "taginput",
                placeholder: "Tag name, Enter to add",
                value: "{text}",
                autofocus: true,
                onmounted: move |e| async move {
                    let _ = e.set_focus(true).await;
                },
                oninput: move |e| {
                    text.set(e.value());
                    pick.set(0);
                },
                onkeydown: move |e: KeyboardEvent| {
                    match e.key().to_string().as_str() {
                        "Enter" => {
                            e.prevent_default();
                            if text().trim().is_empty() {
                                on_close.call(());
                            } else if let Some((id, name)) = key_choices.get(sel).cloned() {
                                key_add(id, name);
                                text.set(String::new());
                                pick.set(0);
                            }
                        }
                        "Escape" => on_close.call(()),
                        "ArrowDown" => {
                            e.prevent_default();
                            if n > 0 {
                                pick.set((sel + 1) % n);
                            }
                        }
                        "ArrowUp" => {
                            e.prevent_default();
                            if n > 0 {
                                pick.set((sel + n - 1) % n);
                            }
                        }
                        _ => {}
                    }
                },
            }
            if !choices.is_empty() {
                div { class: "tagsuggest",
                    for (i , (id , label)) in choices.into_iter().enumerate() {
                        {
                            let add = add.clone();
                            let colour = matches.get(i).map(|m| m.2).unwrap_or_default();
                            let new = id.is_none();
                            rsx! {
                                button {
                                    key: "{i}",
                                    class: if i == sel { "tagopt on" } else { "tagopt" },
                                    onmousedown: move |e| e.prevent_default(),
                                    onclick: move |_| {
                                        add(id.clone(), label.clone());
                                        text.set(String::new());
                                    },
                                    span { class: "tagdot", style: "--tag: {colour.hex()}" }
                                    if new {
                                        "Create “{label}”"
                                    } else {
                                        "{label}"
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

/// A thing's fields: edit in place, add from a template or by name.
#[component]
fn Fields(space: String, target: Target, by: String) -> Element {
    let doc = use_doc(tags::tags_doc(&space));
    let lib = Library::load(&doc.loro());
    let fields = lib.on(&target).fields;
    let mut custom = use_signal(String::new);
    let mut custom_kind = use_signal(|| FieldKind::Text);
    let mut naming = use_signal(|| false);
    let add = {
        let (doc, target, by) = (doc.clone(), target.clone(), by.clone());
        move |name: String, kind: FieldKind| {
            doc.edit(|d| tags::add_field(d, &target, &name, kind, "", &by, now()).ok());
        }
    };
    rsx! {
        if !fields.is_empty() {
            div { class: "fields",
                for f in fields {
                    {
                        let (doc, doc2, target, target2, by) = (doc.clone(), doc.clone(), target.clone(), target.clone(), by.clone());
                        let (fid, fid2) = (f.id.clone(), f.id.clone());
                        let save = move |v: String| {
                            doc.edit(|d| tags::set_field(d, &target, &fid, &v, &by, now()).ok());
                        };
                        rsx! {
                            div { key: "{f.id}", class: "field",
                                div { class: "fieldhead",
                                    span { class: "fieldname", "{f.name}" }
                                    button {
                                        class: "tagx",
                                        title: "Remove {f.name}",
                                        onclick: move |_| {
                                            doc2.edit(|d| tags::remove_field(d, &target2, &fid2).ok());
                                        },
                                        "×"
                                    }
                                }
                                match f.kind {
                                    FieldKind::Multiline => rsx! {
                                        textarea { class: "fieldval", rows: 3, value: "{f.value}", onchange: move |e| save(e.value()) }
                                    },
                                    FieldKind::Date => rsx! {
                                        input { class: "fieldval", r#type: "date", value: "{f.value}", onchange: move |e| save(e.value()) }
                                    },
                                    FieldKind::DateTime => rsx! {
                                        input { class: "fieldval", r#type: "datetime-local", value: "{f.value}", onchange: move |e| save(e.value()) }
                                    },
                                    FieldKind::Text => rsx! {
                                        input { class: "fieldval", value: "{f.value}", onchange: move |e| save(e.value()) }
                                    },
                                }
                            }
                        }
                    }
                }
            }
        }
        if naming() {
            div { class: "fieldnew",
                input {
                    class: "taginput",
                    placeholder: "Field name",
                    value: "{custom}",
                    autofocus: true,
                    oninput: move |e| custom.set(e.value()),
                    onkeydown: {
                        let add = add.clone();
                        move |e: KeyboardEvent| {
                            let k = e.key().to_string();
                            if k == "Enter" && !custom().trim().is_empty() {
                                add(custom().trim().to_string(), custom_kind());
                                custom.set(String::new());
                                naming.set(false);
                            } else if k == "Escape" {
                                naming.set(false);
                            }
                        }
                    },
                }
                select {
                    class: "fieldkind",
                    onchange: move |e| custom_kind.set(FieldKind::parse(&e.value())),
                    for k in FieldKind::ALL {
                        option { key: "{k.id()}", value: "{k.id()}", selected: k == custom_kind(), "{k.label()}" }
                    }
                }
            }
        } else {
            select {
                class: "tagadd fieldpick",
                onchange: {
                    let add = add.clone();
                    move |e: FormEvent| {
                        let v = e.value();
                        if v == "*" {
                            naming.set(true);
                        } else if let Some((n, k)) = tags::templates().into_iter().find(|(n, _)| *n == v) {
                            add(n.to_string(), k);
                        }
                    }
                },
                option { value: "", selected: true, "+ Field" }
                for (n , k) in tags::templates() {
                    option { key: "{n}", value: "{n}", "{n} · {k.label()}" }
                }
                option { value: "*", "Another name…" }
            }
        }
    }
}

// ── the sidebar ─────────────────────────────────────────────────────────

/// The Tags place: every tag, nested by parent, with how many things carry
/// it (or a tag under it).
#[component]
pub fn SidebarTags(space: String) -> Element {
    let doc = use_doc(tags::tags_doc(&space));
    let lib = Library::load(&doc.loro());
    let counts = lib.counts();
    let mut rows: Vec<(usize, Tag)> = Vec::new();
    fn walk(
        lib: &Library,
        t: &Tag,
        depth: usize,
        path: &mut Vec<String>,
        rows: &mut Vec<(usize, Tag)>,
    ) {
        rows.push((depth, t.clone()));
        if path.contains(&t.id) || depth > 6 {
            return;
        }
        path.push(t.id.clone());
        for c in lib.children(&t.id) {
            walk(lib, c, depth + 1, path, rows);
        }
        path.pop();
    }
    for r in lib.roots() {
        walk(&lib, r, 0, &mut Vec::new(), &mut rows);
    }
    let at = |place: Place| crate::nav::At {
        place,
        within: None,
    };
    rsx! {
        app_shell::ui::SidebarSection { title: "Tags",
            for (i , (depth , t)) in rows.into_iter().enumerate() {
                {
                    let count = counts.get(&t.id).copied().unwrap_or(0);
                    rsx! {
                        crate::shell::PlaceRow {
                            key: "{i}-{t.id}",
                            at: at(Place::Tag(t.id.clone())),
                            label: t.name.clone(),
                            color: t.color.hex(),
                            note: if count > 0 { count.to_string() } else { String::new() },
                            depth,
                        }
                    }
                }
            }
            crate::shell::PlaceRow { at: at(Place::Tags), label: "Manage tags", icon: "Collection" }
        }
    }
}

// ── results: a search or a tag ──────────────────────────────────────────

/// What the results are of.
#[derive(Clone, Debug, PartialEq)]
pub enum Wanted {
    /// The search box's text.
    Text(String),
    /// A tag and everything under it.
    Tag(String),
}

/// The search box: plain words search names as always; a search that
/// names a tag or uses the language (AND/OR/NOT, brackets, path:, kind:,
/// special:, a field) runs as a tag search.
#[component]
pub fn SearchView(space: String, q: String) -> Element {
    rsx! {
        Results { space, wanted: Wanted::Text(q) }
    }
}

/// Everything tagged with a tag, or a tag under it.
#[component]
pub fn TagView(space: String, id: String) -> Element {
    rsx! {
        Results { space, wanted: Wanted::Tag(id) }
    }
}

#[component]
fn Results(space: String, wanted: Wanted) -> Element {
    let nav = use_nav();
    let doc = use_doc(tags::tags_doc(&space));
    let spine_doc = use_doc(spine::SPINE);
    let files = use_doc(files_doc(&space));
    let reg = use_doc(space_doc(&space));
    let docs = use_docs("");
    let lib = Library::load(&doc.loro());
    let kinds = Kinds::load(&spine_doc.loro());
    let parsed: Result<Option<Search>, String> = match &wanted {
        Wanted::Text(q) => Search::new(q, &lib).map_err(|e| e.message),
        Wanted::Tag(id) => Ok(Some(Search::for_tag(id, &lib))),
    };
    let search = parsed.clone().ok().flatten();
    // What to ask the node for: one name search for plain words (as the
    // box always did); otherwise the tagged files, and what names, path
    // pieces and kinds find.
    let mut asks: Vec<String> = Vec::new();
    if let Some(s) = &search {
        if let (true, Wanted::Text(q)) = (s.is_plain(), &wanted) {
            asks.push(format!("q={}&limit=200", enc(q.trim())));
        } else {
            let h = s.hints();
            if h.tagged {
                let mut hashes: Vec<String> = lib
                    .targets()
                    .filter_map(|(t, _)| match t {
                        Target::Content(h) => Some(h),
                        _ => None,
                    })
                    .collect();
                hashes.sort();
                if !hashes.is_empty() {
                    asks.push(format!("sampled={}&limit=1000", hashes.join(",")));
                }
            }
            for w in h.words {
                asks.push(format!("q={}&limit=300", enc(&w)));
            }
            for k in h.kinds {
                let exts = query::kind_extensions(&kinds, &k).join(",");
                if !exts.is_empty() {
                    asks.push(format!("exts={}&limit=500", enc(&exts)));
                }
            }
        }
    }
    let key = asks.join("|");
    let sp = space.clone();
    let rows = use_resource(use_reactive!(|(sp, key)| async move {
        let mut out: Vec<serde_json::Value> = Vec::new();
        for ask in key.split('|').filter(|a| !a.is_empty()) {
            if let Some(v) = get(format!("/search?space={}&{ask}", enc(&sp))).await
                && let Some(r) = v["results"].as_array()
            {
                out.extend(r.iter().cloned());
            }
        }
        out
    }));
    let f = files.loro();
    let r = reg.loro();
    let up = online(&reg);
    let mut items: Vec<Item> = Vec::new();
    let mut projects: Vec<(String, String)> = Vec::new();
    if let Some(s) = &search {
        let hit = |item: &Item, path: &str| {
            let on = target_of(item).map(|t| lib.on(&t)).unwrap_or_default();
            let words = query::kind_words(&kinds, &item.kind);
            s.matches(&Subject {
                name: &item.name,
                path,
                kinds: &words,
                on: &on,
            })
        };
        for (p, e) in tree::files(&f) {
            let it = tree_item(&e, &p, &r, &up, &kinds);
            if hit(&it, &p) {
                items.push(it);
            }
        }
        let mut seen = HashSet::new();
        for h in rows().unwrap_or_default() {
            let it = hit_item(&h, &kinds);
            let Src::Index { location, path, .. } = &it.src else {
                continue;
            };
            if !seen.insert(format!("{location}/{path}")) {
                continue;
            }
            let path = path.clone();
            if hit(&it, &path) {
                items.push(it);
            }
        }
        let regp = drive_files::projects::projects(&r);
        let all_docs = docs();
        for (t, _) in lib.targets() {
            match t {
                Target::Doc(id) => {
                    let kind = kinds.of_doc(&id).clone();
                    let info = all_docs.iter().find(|d| d.id == id);
                    let it = Item {
                        name: if kind.doc_prefix.is_empty() {
                            id.clone()
                        } else {
                            id.trim_start_matches(&kind.doc_prefix).replace('-', " ")
                        },
                        dir: false,
                        size: info.map_or(0, |i| i.bytes),
                        mtime: 0,
                        at: app_label(&kind.app),
                        online: true,
                        copies: 1,
                        kept: None,
                        available: None,
                        src: Src::Doc {
                            id: id.clone(),
                            peers: info.map_or(0, |i| i.peers),
                        },
                        kind,
                    };
                    if hit(&it, &id) {
                        items.push(it);
                    }
                }
                Target::Project(id) => {
                    if let Some(p) = regp.iter().find(|p| p.id == id) {
                        let on = lib.on(&Target::Project(id.clone()));
                        if s.matches(&Subject {
                            name: &p.name,
                            path: &p.name,
                            kinds: &[],
                            on: &on,
                        }) {
                            projects.push((p.id.clone(), p.name.clone()));
                        }
                    }
                }
                _ => {}
            }
        }
    }
    let n = items.len() + projects.len();
    let tag_mode = search.as_ref().is_some_and(|s| !s.is_plain());
    let (title, ic) = match &wanted {
        Wanted::Text(q) => (format!("“{}”", q.trim()), "Search"),
        Wanted::Tag(id) => (lib.label(id), "Collection"),
    };
    let tag = match &wanted {
        Wanted::Tag(id) => lib.get(id).cloned(),
        _ => None,
    };
    let open = search.as_ref().is_some_and(|s| s.hints().open);
    rsx! {
        div { class: "viewhead",
            div { class: "kindtitle",
                if let Some(t) = &tag {
                    span { class: "tagdot big", style: chip_style(t) }
                } else {
                    img { src: icon(ic), alt: "" }
                }
                h1 { "{title}" }
                span { class: "count", "{n} found" }
                if tag_mode && matches!(wanted, Wanted::Text(_)) {
                    span { class: "chip tagmode", title: "Searching tags (see docs/tags.md)", "tag search" }
                }
            }
            if let Err(e) = &parsed {
                p { class: "searcherr", "{e}" }
            }
            if let Some(t) = &tag {
                if !t.parents.is_empty() || !lib.children(&t.id).is_empty() {
                    p { class: "hint taghier",
                        for p in t.parents.iter() {
                            {
                                let p = p.clone();
                                let label = lib.label(&p);
                                rsx! {
                                    button { key: "{p}", class: "taglink", onclick: move |_| nav.go(Place::Tag(p.clone())), "↑ {label}" }
                                }
                            }
                        }
                        for c in lib.children(&t.id) {
                            {
                                let c = c.id.clone();
                                let label = lib.label(&c);
                                rsx! {
                                    button { key: "{c}", class: "taglink", onclick: move |_| nav.go(Place::Tag(c.clone())), "↓ {label}" }
                                }
                            }
                        }
                    }
                }
            }
            if open {
                p { class: "hint", "NOT and special: look at the Drive and at tagged files; files on computers are otherwise found by their names, paths and kinds." }
            }
            if !projects.is_empty() {
                div { class: "tagprojects",
                    for (id , name) in projects {
                        button {
                            key: "{id}",
                            class: "chip",
                            onclick: move |_| nav.go(Place::Project(id.clone())),
                            "Project · {name}"
                        }
                    }
                }
            }
        }
        Items {
            items,
            show_at: true,
            empty: match &wanted {
                Wanted::Text(_) if parsed.is_err() => "Finish the search to see what it finds.".to_string(),
                Wanted::Text(_) => "Nothing found in the Drive or on any computer.".to_string(),
                Wanted::Tag(_) => "Nothing carries this tag yet. Pick a file and add it in the inspector.".to_string(),
            },
            loading: rows().is_none(),
        }
    }
}

// ── the tag manager ─────────────────────────────────────────────────────

/// Every tag: rename it, colour it, give it a shorthand, aliases, parents;
/// make it a category or hidden; delete it (after a confirmation).
#[component]
pub fn TagManager(space: String) -> Element {
    let doc = use_doc(tags::tags_doc(&space));
    let lib = Library::load(&doc.loro());
    let counts = lib.counts();
    let mut picked = use_signal(|| None::<String>);
    let mut draft = use_signal(String::new);
    let mut filter = use_signal(String::new);
    let by = me();
    let q = filter().to_lowercase();
    let list: Vec<Tag> = lib
        .all()
        .into_iter()
        .filter(|t| q.is_empty() || t.names().any(|n| n.to_lowercase().contains(&q)))
        .cloned()
        .collect();
    let cur = picked().and_then(|id| lib.get(&id).cloned());
    let editing = cur.is_some();
    rsx! {
        div { class: "pane wide tagmanager",
            div { class: "panehead",
                h1 { "Tags" }
                span { class: "count", "{lib.all().len()} tags" }
            }
            div { class: "capture",
                input {
                    placeholder: "New tag",
                    value: "{draft}",
                    oninput: move |e| draft.set(e.value()),
                    onkeydown: {
                        let (doc, by) = (doc.clone(), by.clone());
                        move |e: KeyboardEvent| {
                            let name = draft().trim().to_string();
                            if e.key().to_string() == "Enter" && !name.is_empty() {
                                let id = doc.edit(|d| tags::create(d, NewTag { name: &name, ..NewTag::default() }, &by, now()).ok());
                                draft.set(String::new());
                                picked.set(id);
                            }
                        }
                    },
                }
                input {
                    placeholder: "Filter",
                    value: "{filter}",
                    oninput: move |e| filter.set(e.value()),
                }
            }
            if !lib.broken().is_empty() {
                p { class: "searcherr",
                    "Two people put tags under each other at once; Drive ignores: "
                    {lib.broken().iter().map(|(c, p)| format!("{} under {}", lib.label(c), lib.label(p))).collect::<Vec<_>>().join(", ")}
                }
            }
            div { class: if editing { "tagmgr editing" } else { "tagmgr" },
                div { class: "taglist",
                    for t in list {
                        {
                            let id = t.id.clone();
                            let on = picked().as_deref() == Some(t.id.as_str());
                            let count = counts.get(&t.id).copied().unwrap_or(0);
                            let parents = t.parents.iter().map(|p| lib.label(p)).collect::<Vec<_>>().join(", ");
                            rsx! {
                                button {
                                    key: "{t.id}",
                                    class: if on { "tagrow on" } else { "tagrow" },
                                    onclick: move |_| picked.set(Some(id.clone())),
                                    span { class: "tagchip solo", style: chip_style(&t), span { class: "tagname", "{lib.label(&t.id)}" } }
                                    if !parents.is_empty() {
                                        span { class: "tagmeta", "under {parents}" }
                                    }
                                    if t.category {
                                        span { class: "chip", "category" }
                                    }
                                    if t.hidden {
                                        span { class: "chip", "hidden" }
                                    }
                                    span { class: "tagcount", "{count}" }
                                }
                            }
                        }
                    }
                }
                if let Some(t) = cur {
                    TagEditor { key: "{t.id}", space: space.clone(), id: t.id.clone(), on_done: move |_| picked.set(None) }
                }
            }
        }
    }
}

/// One edit of a tag, run when the person asks for it.
type TagEdit = Box<dyn FnOnce(&loro::LoroDoc) -> tags::Result<()>>;

#[component]
fn TagEditor(space: String, id: String, on_done: EventHandler<()>) -> Element {
    let nav = use_nav();
    let doc = use_doc(tags::tags_doc(&space));
    let lib = Library::load(&doc.loro());
    let mut alias = use_signal(String::new);
    let mut parent = use_signal(String::new);
    let mut problem = use_signal(String::new);
    let mut confirm = use_signal(|| false);
    let Some(t) = lib.get(&id).cloned() else {
        return rsx! {};
    };
    let count = lib.counts().get(&id).copied().unwrap_or(0);
    let edit = {
        let doc = doc.clone();
        move |f: TagEdit| match doc.edit(f) {
            Ok(()) => problem.set(String::new()),
            Err(e) => problem.set(e.to_string()),
        }
    };
    let parent_choices: Vec<(String, String)> = lib
        .suggest(&parent())
        .into_iter()
        .filter(|p| {
            p.id != id && !t.parents.contains(&p.id) && !lib.descendants(&id).contains(&p.id)
        })
        .take(5)
        .map(|p| (p.id.clone(), lib.label(&p.id)))
        .collect();
    let first_parent = parent_choices.first().map(|p| p.0.clone());
    let id2 = id.clone();
    rsx! {
        div { class: "tageditor",
            div { class: "tagedhead",
                span { class: "tagchip solo", style: chip_style(&t), span { class: "tagname", "{lib.label(&id)}" } }
                button { class: "tagx", title: "Close", onclick: move |_| on_done.call(()), "✕" }
            }
            label { class: "tagfield",
                span { "Name" }
                input {
                    value: "{t.name}",
                    onchange: {
                        let mut edit = edit.clone();
                        let id = id.clone();
                        move |e: FormEvent| {
                            let v = e.value();
                            let id = id.clone();
                            edit(Box::new(move |d| tags::rename(d, &id, &v)));
                        }
                    },
                }
            }
            label { class: "tagfield",
                span { "Shorthand" }
                input {
                    value: "{t.shorthand}",
                    placeholder: "dr",
                    onchange: {
                        let mut edit = edit.clone();
                        let id = id.clone();
                        move |e: FormEvent| {
                            let v = e.value();
                            let id = id.clone();
                            edit(Box::new(move |d| tags::set_shorthand(d, &id, &v)));
                        }
                    },
                }
            }
            div { class: "tagfield",
                span { "Colour" }
                div { class: "swatches",
                    for c in Color::ALL {
                        {
                            let mut edit = edit.clone();
                            let id = id.clone();
                            rsx! {
                                button {
                                    key: "{c.id()}",
                                    class: if c == t.color { "swatch on" } else { "swatch" },
                                    style: "--tag: {c.hex()}",
                                    title: "{c.label()}",
                                    onclick: move |_| {
                                        let id = id.clone();
                                        edit(Box::new(move |d| tags::set_color(d, &id, c)));
                                    },
                                }
                            }
                        }
                    }
                }
            }
            div { class: "tagfield",
                span { "Aliases" }
                div { class: "tagchips",
                    for a in t.aliases.iter() {
                        {
                            let mut edit = edit.clone();
                            let (id, a2) = (id.clone(), a.clone());
                            rsx! {
                                span { key: "{a}", class: "tagchip plain",
                                    span { class: "tagname", "{a}" }
                                    button {
                                        class: "tagx",
                                        onclick: move |_| {
                                            let (id, a2) = (id.clone(), a2.clone());
                                            edit(Box::new(move |d| tags::remove_alias(d, &id, &a2)));
                                        },
                                        "×"
                                    }
                                }
                            }
                        }
                    }
                    input {
                        class: "taginput small",
                        placeholder: "+ alias",
                        value: "{alias}",
                        oninput: move |e| alias.set(e.value()),
                        onkeydown: {
                            let mut edit = edit.clone();
                            let id = id.clone();
                            move |e: KeyboardEvent| {
                                if e.key().to_string() == "Enter" && !alias().trim().is_empty() {
                                    let (id, a) = (id.clone(), alias());
                                    edit(Box::new(move |d| tags::add_alias(d, &id, &a)));
                                    alias.set(String::new());
                                }
                            }
                        },
                    }
                }
            }
            div { class: "tagfield",
                span { "Parents" }
                div { class: "tagchips",
                    for p in t.parents.iter() {
                        if let Some(pt) = lib.get(p) {
                            {
                                let mut edit = edit.clone();
                                let (id, p2) = (id.clone(), p.clone());
                                rsx! {
                                    span { key: "{p}", class: "tagchip", style: chip_style(pt),
                                        span { class: "tagname", "{lib.label(p)}" }
                                        button {
                                            class: "tagx",
                                            onclick: move |_| {
                                                let (id, p2) = (id.clone(), p2.clone());
                                                edit(Box::new(move |d| tags::remove_parent(d, &id, &p2)));
                                            },
                                            "×"
                                        }
                                    }
                                }
                            }
                        }
                    }
                    input {
                        class: "taginput small",
                        placeholder: "+ parent",
                        value: "{parent}",
                        oninput: move |e| parent.set(e.value()),
                        onkeydown: {
                            let mut edit = edit.clone();
                            let id = id.clone();
                            let first = first_parent.clone();
                            let any = lib.resolve(&parent());
                            move |e: KeyboardEvent| {
                                if e.key().to_string() != "Enter" {
                                    return;
                                }
                                // A name typed exactly wins (a tag that would make a
                                // cycle is then refused with the reason); otherwise
                                // the top suggestion, which never makes one.
                                let target = any.first().cloned().or_else(|| first.clone());
                                if let Some(p) = target {
                                    let id = id.clone();
                                    edit(Box::new(move |d| tags::add_parent(d, &id, &p)));
                                    parent.set(String::new());
                                }
                            }
                        },
                    }
                }
                if !parent().trim().is_empty() && !parent_choices.is_empty() {
                    div { class: "tagsuggest inline",
                        for (pid , label) in parent_choices {
                            {
                                let mut edit = edit.clone();
                                let id = id.clone();
                                rsx! {
                                    button {
                                        key: "{pid}",
                                        class: "tagopt",
                                        onclick: move |_| {
                                            let (id, pid) = (id.clone(), pid.clone());
                                            edit(Box::new(move |d| tags::add_parent(d, &id, &pid)));
                                            parent.set(String::new());
                                        },
                                        "{label}"
                                    }
                                }
                            }
                        }
                    }
                }
            }
            label { class: "tagcheck",
                input {
                    r#type: "checkbox",
                    checked: t.category,
                    onchange: {
                        let mut edit = edit.clone();
                        let id = id.clone();
                        move |e: FormEvent| {
                            let (id, on) = (id.clone(), e.checked());
                            edit(Box::new(move |d| tags::set_category(d, &id, on)));
                        }
                    },
                }
                span { "Category: group the tags under it in the inspector" }
            }
            label { class: "tagcheck",
                input {
                    r#type: "checkbox",
                    checked: t.hidden,
                    onchange: {
                        let mut edit = edit.clone();
                        let id = id.clone();
                        move |e: FormEvent| {
                            let (id, on) = (id.clone(), e.checked());
                            edit(Box::new(move |d| tags::set_hidden(d, &id, on)));
                        }
                    },
                }
                span { "Hidden: things with it stay out of searches that do not name it" }
            }
            if !problem().is_empty() {
                p { class: "searcherr", "{problem}" }
            }
            div { class: "tagedacts",
                button { class: "primary ghost", onclick: move |_| nav.go(Place::Tag(id2.clone())), "Show {count} tagged" }
                if !t.builtin {
                    if confirm() {
                        button {
                            class: "primary tagdanger",
                            onclick: {
                                let mut edit = edit.clone();
                                let id = id.clone();
                                move |_| {
                                    let id = id.clone();
                                    edit(Box::new(move |d| tags::delete(d, &id)));
                                    confirm.set(false);
                                    on_done.call(());
                                }
                            },
                            if count > 0 { "Delete, and take it off {count}" } else { "Delete it" }
                        }
                        button { class: "primary ghost", onclick: move |_| confirm.set(false), "Keep it" }
                    } else {
                        button { class: "primary ghost tagdanger", onclick: move |_| confirm.set(true), "Delete…" }
                    }
                }
            }
        }
    }
}

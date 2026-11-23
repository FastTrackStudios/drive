//! Comments: timestamped, authored, threaded notes on anything Drive keeps
//! (a document, a file, a project), anchored where they belong: a stretch
//! of text, a moment or span of audio or video, a region of an image or a
//! PDF page, or the whole thing. Every app uses this one model, so a mix's
//! review notes, a chart's margin notes and a recipe PDF's markup look and
//! sync the same way.
//!
//! The comments on one thing live in their own document,
//! `comments:<target>` (in the thing's org): live, offline-capable, with
//! its own history. A file's comments remember which version (content hash)
//! they were made on, so a new version shows the old notes as earlier ones;
//! each version can also be approved or sent back.
//!
//! Layout: `comments` map: id → { id, parent, author, by, created, edited,
//! body, resolved, resolved_by, resolved_at, deleted, version, anchor: {
//! kind, … } }; `reviews` map: version → { state, by, at }.

use std::collections::HashMap;

use loro::cursor::{Cursor, Side};
use loro::{LoroDoc, LoroMap, LoroValue, ValueOrContainer};

/// What a comment thread is about.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Target {
    /// An app document, by its id within the org (`studio:song:x`).
    Doc(String),
    /// A file of the space's tree, by its tree id (stable through moves).
    File(String),
    /// A file of a location's index: location id and path.
    Indexed { location: String, path: String },
    /// A project of the space's registry.
    Project(String),
}

impl Target {
    /// The comments document for this target (within the org; `use_doc`
    /// adds the org).
    pub fn doc_id(&self) -> String {
        match self {
            Target::Doc(id) => format!("comments:doc:{id}"),
            Target::File(id) => format!("comments:file:{id}"),
            // Paths escaped: a document id keeps '/' for its org.
            Target::Indexed { location, path } => {
                format!("comments:loc:{location}:{}", escape(path))
            }
            Target::Project(id) => format!("comments:project:{id}"),
        }
    }
}

fn escape(path: &str) -> String {
    path.replace('%', "%25").replace('/', "%2F")
}

/// Where in the thing a comment points.
#[derive(Clone, Debug, PartialEq)]
pub enum Anchor {
    /// The whole thing.
    Whole,
    /// A stretch of text in the target document's text container: two
    /// encoded Loro cursors, which follow the text as it is edited.
    Text {
        container: String,
        start: String,
        end: String,
        quote: String,
    },
    /// A moment (`end_ms` == `start_ms`) or span of audio or video.
    Time { start_ms: i64, end_ms: i64 },
    /// A region of an image or a PDF page, in fractions of its size (0..1).
    Region {
        page: i64,
        x: f64,
        y: f64,
        w: f64,
        h: f64,
    },
}

/// One comment (a thread's first comment has no parent).
#[derive(Clone, Debug, PartialEq)]
pub struct Comment {
    pub id: String,
    /// The comment it answers ("": it starts a thread).
    pub parent: String,
    /// Who: their email (signed in) or a name (a guest from a share link).
    pub author: String,
    /// How to show them.
    pub by: String,
    /// Milliseconds since the epoch.
    pub created: i64,
    /// When the text was last edited (0: never).
    pub edited: i64,
    pub body: String,
    pub resolved: bool,
    pub resolved_by: String,
    pub resolved_at: i64,
    pub deleted: bool,
    /// The file version (content hash) it was made on, if a file.
    pub version: String,
    pub anchor: Anchor,
}

/// A thread: a comment and its replies, oldest first.
#[derive(Clone, Debug, PartialEq)]
pub struct Thread {
    pub root: Comment,
    pub replies: Vec<Comment>,
}

/// Where a version stands in review.
#[derive(Clone, Debug, PartialEq)]
pub struct Review {
    pub version: String,
    /// `approved` or `changes`.
    pub state: String,
    pub by: String,
    pub at: i64,
}

fn s(m: &LoroMap, k: &str) -> String {
    match m.get(k) {
        Some(ValueOrContainer::Value(LoroValue::String(v))) => v.to_string(),
        _ => String::new(),
    }
}

fn n(m: &LoroMap, k: &str) -> i64 {
    match m.get(k) {
        Some(ValueOrContainer::Value(LoroValue::I64(v))) => v,
        Some(ValueOrContainer::Value(LoroValue::Double(v))) => v as i64,
        _ => 0,
    }
}

fn f(m: &LoroMap, k: &str) -> f64 {
    match m.get(k) {
        Some(ValueOrContainer::Value(LoroValue::Double(v))) => v,
        Some(ValueOrContainer::Value(LoroValue::I64(v))) => v as f64,
        _ => 0.0,
    }
}

fn b(m: &LoroMap, k: &str) -> bool {
    matches!(
        m.get(k),
        Some(ValueOrContainer::Value(LoroValue::Bool(true)))
    )
}

fn child(m: &LoroMap, k: &str) -> Option<LoroMap> {
    match m.get(k) {
        Some(ValueOrContainer::Container(c)) => c.into_map().ok(),
        _ => None,
    }
}

/// Time-ordered and unique across devices: the millisecond, this client's
/// Loro peer and its next op counter.
fn new_id(doc: &LoroDoc, now: i64) -> String {
    let peer = doc.peer_id();
    let counter =
        doc.oplog_vv().get(&peer).copied().unwrap_or(0) + doc.get_pending_txn_len() as i32;
    format!("c{now:x}-{peer:x}-{counter:x}")
}

fn put_anchor(m: &LoroMap, a: &Anchor) -> loro::LoroResult<()> {
    let am = m.ensure_mergeable_map("anchor")?;
    match a {
        Anchor::Whole => {
            am.insert("kind", "whole")?;
        }
        Anchor::Text {
            container,
            start,
            end,
            quote,
        } => {
            am.insert("kind", "text")?;
            am.insert("container", container.as_str())?;
            am.insert("start", start.as_str())?;
            am.insert("end", end.as_str())?;
            am.insert("quote", quote.as_str())?;
        }
        Anchor::Time { start_ms, end_ms } => {
            am.insert("kind", "time")?;
            am.insert("start_ms", *start_ms)?;
            am.insert("end_ms", *end_ms)?;
        }
        Anchor::Region { page, x, y, w, h } => {
            am.insert("kind", "region")?;
            am.insert("page", *page)?;
            am.insert("x", *x)?;
            am.insert("y", *y)?;
            am.insert("w", *w)?;
            am.insert("h", *h)?;
        }
    }
    Ok(())
}

fn read_anchor(m: &LoroMap) -> Anchor {
    let Some(a) = child(m, "anchor") else {
        return Anchor::Whole;
    };
    match s(&a, "kind").as_str() {
        "text" => Anchor::Text {
            container: s(&a, "container"),
            start: s(&a, "start"),
            end: s(&a, "end"),
            quote: s(&a, "quote"),
        },
        "time" => Anchor::Time {
            start_ms: n(&a, "start_ms"),
            end_ms: n(&a, "end_ms"),
        },
        "region" => Anchor::Region {
            page: n(&a, "page"),
            x: f(&a, "x"),
            y: f(&a, "y"),
            w: f(&a, "w"),
            h: f(&a, "h"),
        },
        _ => Anchor::Whole,
    }
}

/// What a new comment says and where.
#[derive(Clone, Debug)]
pub struct New<'a> {
    pub author: &'a str,
    pub by: &'a str,
    pub body: &'a str,
    pub anchor: Anchor,
    pub version: &'a str,
    /// The comment it answers, if a reply (a reply takes its thread's anchor).
    pub parent: Option<&'a str>,
}

/// Add a comment (or a reply); returns its id. `now`: milliseconds.
pub fn add(doc: &LoroDoc, c: New, now: i64) -> loro::LoroResult<String> {
    let id = new_id(doc, now);
    let m = doc.get_map("comments").ensure_mergeable_map(&id)?;
    m.insert("id", id.as_str())?;
    m.insert("parent", c.parent.unwrap_or(""))?;
    m.insert("author", c.author)?;
    m.insert("by", c.by)?;
    m.insert("created", now)?;
    m.insert("body", c.body)?;
    m.insert("version", c.version)?;
    put_anchor(&m, &c.anchor)?;
    Ok(id)
}

fn comment_map(doc: &LoroDoc, id: &str) -> Option<LoroMap> {
    child(&doc.get_map("comments"), id)
}

/// Change a comment's text (its author, in the app).
pub fn edit(doc: &LoroDoc, id: &str, body: &str, now: i64) -> loro::LoroResult<()> {
    if let Some(m) = comment_map(doc, id) {
        m.insert("body", body)?;
        m.insert("edited", now)?;
    }
    Ok(())
}

/// Resolve a thread (or reopen it).
pub fn resolve(doc: &LoroDoc, id: &str, on: bool, by: &str, now: i64) -> loro::LoroResult<()> {
    if let Some(m) = comment_map(doc, id) {
        m.insert("resolved", on)?;
        m.insert("resolved_by", if on { by } else { "" })?;
        m.insert("resolved_at", if on { now } else { 0 })?;
    }
    Ok(())
}

/// Delete a comment (its replies stay readable under "deleted").
pub fn delete(doc: &LoroDoc, id: &str) -> loro::LoroResult<()> {
    if let Some(m) = comment_map(doc, id) {
        m.insert("deleted", true)?;
        m.insert("body", "")?;
    }
    Ok(())
}

fn read(m: &LoroMap) -> Comment {
    Comment {
        id: s(m, "id"),
        parent: s(m, "parent"),
        author: s(m, "author"),
        by: s(m, "by"),
        created: n(m, "created"),
        edited: n(m, "edited"),
        body: s(m, "body"),
        resolved: b(m, "resolved"),
        resolved_by: s(m, "resolved_by"),
        resolved_at: n(m, "resolved_at"),
        deleted: b(m, "deleted"),
        version: s(m, "version"),
        anchor: read_anchor(m),
    }
}

/// Every comment, oldest first.
pub fn all(doc: &LoroDoc) -> Vec<Comment> {
    let map = doc.get_map("comments");
    let mut out: Vec<Comment> = map
        .keys()
        .filter_map(|k| child(&map, &k))
        .map(|m| read(&m))
        .filter(|c| !c.id.is_empty())
        .collect();
    out.sort_by(|a, b| (a.created, &a.id).cmp(&(b.created, &b.id)));
    out
}

/// Threads and their replies, oldest first. A reply with a missing parent
/// starts its own thread. A cycle uses the first parent encountered twice.
pub fn threads(doc: &LoroDoc) -> Vec<Thread> {
    let all = all(doc);
    let roots = thread_roots(&all);
    let mut replies: HashMap<usize, Vec<Comment>> = HashMap::new();
    let mut threads = Vec::new();
    for (index, (comment, root)) in all.into_iter().zip(roots).enumerate() {
        if index == root {
            threads.push((index, comment));
        } else {
            replies.entry(root).or_default().push(comment);
        }
    }
    threads
        .into_iter()
        .map(|(index, root)| Thread {
            root,
            replies: replies.remove(&index).unwrap_or_default(),
        })
        .collect()
}

fn thread_roots(comments: &[Comment]) -> Vec<usize> {
    let by_id: HashMap<_, _> = comments
        .iter()
        .enumerate()
        .map(|(index, comment)| (comment.id.as_str(), index))
        .collect();
    let mut roots = vec![None; comments.len()];
    let mut seen_from = vec![None; comments.len()];
    let mut path = Vec::new();
    for start in 0..comments.len() {
        let mut index = start;
        let root = loop {
            if let Some(root) = roots[index] {
                break root;
            }
            if seen_from[index] == Some(start) {
                break index;
            }
            seen_from[index] = Some(start);
            path.push(index);
            let Some(&parent) = by_id.get(comments[index].parent.as_str()) else {
                break index;
            };
            index = parent;
        };
        for index in path.drain(..) {
            roots[index] = Some(root);
        }
    }
    roots
        .into_iter()
        .map(|root| root.expect("thread root"))
        .collect()
}

/// Mark a version approved or needing changes (`state`: `approved`,
/// `changes`, or "" to clear).
pub fn set_review(
    doc: &LoroDoc,
    version: &str,
    state: &str,
    by: &str,
    now: i64,
) -> loro::LoroResult<()> {
    let reviews = doc.get_map("reviews");
    if state.is_empty() {
        if reviews.get(version).is_some() {
            reviews.delete(version)?;
        }
        return Ok(());
    }
    let m = reviews.ensure_mergeable_map(version)?;
    m.insert("state", state)?;
    m.insert("by", by)?;
    m.insert("at", now)?;
    Ok(())
}

pub fn reviews(doc: &LoroDoc) -> Vec<Review> {
    let map = doc.get_map("reviews");
    let mut out: Vec<Review> = map
        .keys()
        .filter_map(|k| {
            child(&map, &k).map(|m| Review {
                version: k.to_string(),
                state: s(&m, "state"),
                by: s(&m, "by"),
                at: n(&m, "at"),
            })
        })
        .collect();
    out.sort_by_key(|r| r.at);
    out
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn unhex(s: &str) -> Option<Vec<u8>> {
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok())
        .collect()
}

/// An anchor on characters `start..end` of the target document's text
/// container `container` (by root name), which keeps pointing at the same
/// words as the text around them changes.
pub fn text_anchor(target: &LoroDoc, container: &str, start: usize, end: usize) -> Option<Anchor> {
    let text = target.get_text(container);
    let len = text.len_unicode();
    let (start, end) = (start.min(len), end.min(len).max(start.min(len)));
    let a = text.get_cursor(start, Side::Right)?;
    let z = text.get_cursor(end, Side::Left)?;
    let quote: String = text
        .to_string()
        .chars()
        .skip(start)
        .take(end - start)
        .collect();
    Some(Anchor::Text {
        container: container.into(),
        start: hex(&a.encode()),
        end: hex(&z.encode()),
        quote,
    })
}

/// Where a text anchor is now in the target document (`None`: its text is
/// gone).
pub fn text_range(target: &LoroDoc, anchor: &Anchor) -> Option<(usize, usize)> {
    let Anchor::Text { start, end, .. } = anchor else {
        return None;
    };
    let pos = |enc: &str| -> Option<usize> {
        let c = Cursor::decode(&unhex(enc)?).ok()?;
        Some(target.get_cursor_pos(&c).ok()?.current.pos)
    };
    let (a, z) = (pos(start)?, pos(end)?);
    Some((a.min(z), a.max(z)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn new<'a>(body: &'a str, anchor: Anchor, parent: Option<&'a str>) -> New<'a> {
        New {
            author: "cody@example.com",
            by: "Cody",
            body,
            anchor,
            version: "v1",
            parent,
        }
    }

    #[test]
    fn a_reply_cycle_remains_visible() {
        let doc = LoroDoc::new();
        let first = add(&doc, new("First", Anchor::Whole, None), 1).unwrap();
        let second = add(&doc, new("Second", Anchor::Whole, Some(&first)), 2).unwrap();
        comment_map(&doc, &first)
            .unwrap()
            .insert("parent", second.as_str())
            .unwrap();

        let threads = threads(&doc);
        assert_eq!(threads.len(), 1);
        assert_eq!(threads[0].root.id, first);
        assert_eq!(threads[0].replies.len(), 1);
        assert_eq!(threads[0].replies[0].id, second);
    }

    #[test]
    fn threads_replies_resolve_and_reviews() {
        let d = LoroDoc::new();
        let a = add(
            &d,
            new(
                "Vocal too loud at the chorus",
                Anchor::Time {
                    start_ms: 74_000,
                    end_ms: 81_500,
                },
                None,
            ),
            1_000,
        )
        .unwrap();
        let r = add(
            &d,
            new("Pulled it down 1.5 dB", Anchor::Whole, Some(&a)),
            2_000,
        )
        .unwrap();
        let nested = add(&d, new("Sounds good", Anchor::Whole, Some(&r)), 2_500).unwrap();
        let b = add(
            &d,
            new(
                "Love the intro",
                Anchor::Time {
                    start_ms: 0,
                    end_ms: 0,
                },
                None,
            ),
            3_000,
        )
        .unwrap();
        resolve(&d, &a, true, "client@example.com", 4_000).unwrap();
        edit(&d, &b, "Love the intro!", 5_000).unwrap();
        set_review(&d, "v1", "changes", "client@example.com", 6_000).unwrap();
        set_review(&d, "v2", "approved", "client@example.com", 7_000).unwrap();
        d.commit();

        let t = threads(&d);
        assert_eq!(t.len(), 2);
        assert_eq!(
            t[0].root.anchor,
            Anchor::Time {
                start_ms: 74_000,
                end_ms: 81_500
            }
        );
        assert!(t[0].root.resolved);
        assert_eq!(
            t[0].replies
                .iter()
                .map(|reply| &reply.id)
                .collect::<Vec<_>>(),
            [&r, &nested]
        );
        assert_eq!(t[1].root.body, "Love the intro!");
        assert_eq!(t[1].root.edited, 5_000);
        assert_eq!(
            reviews(&d)
                .iter()
                .map(|r| r.state.as_str())
                .collect::<Vec<_>>(),
            ["changes", "approved"]
        );
    }

    #[test]
    fn a_text_anchor_follows_its_words() {
        let page = LoroDoc::new();
        page.get_text("body")
            .insert(0, "Grace that taught my heart to fear")
            .unwrap();
        let anchor = text_anchor(&page, "body", 11, 17).unwrap(); // "taught"
        // Someone edits before and inside the line.
        page.get_text("body").insert(0, "Amazing ").unwrap();
        page.get_text("body").insert(8, "'Twas ").unwrap();
        let (s, e) = text_range(&page, &anchor).unwrap();
        let text = page.get_text("body").to_string();
        assert_eq!(
            text.chars().skip(s).take(e - s).collect::<String>(),
            "taught"
        );
        if let Anchor::Text { quote, .. } = &anchor {
            assert_eq!(quote, "taught");
        }
    }

    #[test]
    fn two_devices_comment_at_once() {
        let a = LoroDoc::new();
        let b = LoroDoc::new();
        add(&a, new("from the laptop", Anchor::Whole, None), 10).unwrap();
        add(
            &b,
            new(
                "from the phone",
                Anchor::Region {
                    page: 2,
                    x: 0.1,
                    y: 0.2,
                    w: 0.3,
                    h: 0.1,
                },
                None,
            ),
            11,
        )
        .unwrap();
        a.commit();
        b.commit();
        a.import(&b.export(loro::ExportMode::Snapshot).unwrap())
            .unwrap();
        b.import(&a.export(loro::ExportMode::Snapshot).unwrap())
            .unwrap();
        assert_eq!(threads(&a).len(), 2);
        assert_eq!(threads(&a), threads(&b));
    }
}

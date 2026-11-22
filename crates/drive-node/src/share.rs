//! Share links: `/s/{space}/{token}` shows a project's files (or only some
//! kinds of them, "all the audio") to anyone holding the link, and lets
//! them download one file or everything as one zip.
//!
//! Where a file comes from, fastest first:
//! 1. kept available and in a public storage (the R2 bucket): a redirect
//!    there;
//! 2. kept available on the node: from the node;
//! 3. a storage server that has a copy and is on (Starcommand);
//! 4. the computer that holds the part, if it is on;
//! 5. any other computer with a copy that is on.
//!
//! Nothing is uploaded ahead of time: what is not kept available travels
//! from the computer that has it, through the node, when it is asked for.

use std::collections::HashMap;
use std::io::Write;

use axum::body::{Body, Bytes};
use axum::extract::{Path as UrlPath, Query, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{Html, IntoResponse, Redirect, Response};
use drive_files::placement::Role;
use drive_files::projects::{self, Available, Part, Project, Share};
use drive_files::space;
use drive_spine::comments::{self, Anchor, Target};
use drive_spine::kinds::Kinds;

use crate::catalog::Catalog;
use crate::store::Found;

/// A share, resolved: its project, and every file it offers.
struct Resolved {
    share: Share,
    project: Project,
    /// The project's org.
    space: String,
    files: Vec<(Part, Found)>,
    /// Each org whose locations hold parts (the project's own, and those
    /// that lend it folders): what the share needs to know of it.
    spaces: HashMap<String, SpaceView>,
}

/// One org's side of a share.
#[derive(Default)]
struct SpaceView {
    /// location id → (its device, its name, its device's name).
    locations: HashMap<String, (String, String, String)>,
    /// location id → path → its record, for files kept available.
    available: HashMap<String, HashMap<String, Available>>,
    /// storage id → public URL.
    public: HashMap<String, String>,
    /// Devices whose agent is connected.
    online: Vec<String>,
    /// Storage-server devices.
    servers: Vec<String>,
    /// blob hash → its holders, for files kept available.
    holders: HashMap<String, Vec<String>>,
}

async fn view(c: &Catalog, space_id: &str) -> SpaceView {
    let (locations, devices, public, available, holders) = c
        .rooms
        .read(&drive_files::space_doc(space_id), |d| {
            let locations = space::locations(d);
            let available: HashMap<String, HashMap<String, Available>> = locations
                .iter()
                .map(|l| (l.id.clone(), projects::available(d, &l.id)))
                .collect();
            let holders: HashMap<String, Vec<String>> = available
                .values()
                .flat_map(|m| m.values())
                .map(|a| {
                    (
                        a.hash.clone(),
                        space::holders(d, &a.hash)
                            .into_iter()
                            .map(|(w, _)| w)
                            .collect(),
                    )
                })
                .collect();
            (
                locations,
                space::devices(d),
                space::public(d),
                available,
                holders,
            )
        })
        .await;
    let names: HashMap<&str, &str> = devices
        .iter()
        .map(|d| (d.id.as_str(), d.name.as_str()))
        .collect();
    SpaceView {
        locations: locations
            .iter()
            .map(|l| {
                (
                    l.id.clone(),
                    (
                        l.device.clone(),
                        l.name.clone(),
                        names.get(l.device.as_str()).unwrap_or(&"").to_string(),
                    ),
                )
            })
            .collect(),
        available,
        public,
        online: c.rooms.agents_online(space_id).await,
        servers: devices
            .iter()
            .filter(|d| Role::of(&d.roles) == Role::StorageServer)
            .map(|d| d.id.clone())
            .collect(),
        holders,
    }
}

async fn resolve(c: &Catalog, space_id: &str, token: &str) -> Result<Resolved, StatusCode> {
    let doc = drive_files::space_doc(space_id);
    let (share, all) = c
        .rooms
        .read(&doc, |d| (projects::share(d, token), projects::projects(d)))
        .await;
    let Some(share) = share else {
        tracing::warn!(share.space = %space_id, "a share link that does not exist was asked for");
        return Err(StatusCode::NOT_FOUND);
    };
    let mut project = all
        .into_iter()
        .find(|p| p.id == share.project)
        .ok_or(StatusCode::NOT_FOUND)?;
    // A part in another org's location only when that org lends it to
    // this project (drive#52): otherwise it is not this link's to give.
    let mut lent = HashMap::new();
    let mut kept_parts = Vec::new();
    for part in std::mem::take(&mut project.parts) {
        let theirs = part.space_in(space_id).to_string();
        if part.in_tree() {
            continue; // Drive tree folders are not offered by share links yet.
        }
        if theirs != space_id {
            if !lent.contains_key(&theirs) {
                let l = c
                    .rooms
                    .read(&drive_files::space_doc(&theirs), projects::lends)
                    .await;
                lent.insert(theirs.clone(), l);
            }
            let ok = lent[&theirs]
                .iter()
                .any(|l| l.org == space_id && l.matches(&theirs, &project, &part));
            if !ok {
                continue;
            }
        }
        kept_parts.push(part);
    }
    project.parts = kept_parts;
    let kinds = c
        .rooms
        .read(&drive_spine::spine_doc(space_id), Kinds::load)
        .await;
    let allowed: Vec<String> = share
        .kinds
        .iter()
        .flat_map(|k| kinds.family(k).into_iter().map(|x| x.id.clone()))
        .collect();
    // A file inside two parts (Mixes inside the song's folder) is listed
    // once, under the most specific part.
    let mut parts: Vec<&Part> = project.parts.iter().collect();
    parts.sort_by_key(|p| std::cmp::Reverse(p.path.len()));
    let mut seen = std::collections::HashSet::new();
    let mut files = Vec::new();
    let mut spaces = HashMap::new();
    for part in parts {
        let theirs = part.space_in(space_id);
        if !spaces.contains_key(theirs) {
            spaces.insert(theirs.to_string(), view(c, theirs).await);
        }
        let found = c
            .rooms
            .store
            .under(theirs, &part.location, &part.path)
            .await
            .map_err(|e| {
                tracing::warn!(error = %e, "a share's files could not be listed");
                StatusCode::INTERNAL_SERVER_ERROR
            })?;
        for f in found {
            if (allowed.is_empty() || allowed.contains(&kinds.of_file(&f.name, false).id))
                && seen.insert((theirs.to_string(), f.location.clone(), f.path.clone()))
            {
                files.push((part.clone(), f));
            }
        }
    }
    Ok(Resolved {
        share,
        project,
        space: space_id.to_string(),
        files,
        spaces,
    })
}

impl Resolved {
    /// The org whose location a part is in.
    fn space_of<'a>(&'a self, part: &'a Part) -> &'a str {
        part.space_in(&self.space)
    }

    fn view(&self, part: &Part) -> Option<&SpaceView> {
        self.spaces.get(self.space_of(part))
    }

    fn kept(&self, part: &Part, f: &Found) -> Option<&Available> {
        self.view(part)?
            .available
            .get(&part.location)?
            .get(&f.path)
            .filter(|a| a.size == f.size && a.mtime == f.mtime)
    }

    /// Where a kept-available file is in a public storage, if it is.
    fn public_url(&self, part: &Part, a: &Available) -> Option<String> {
        let v = self.view(part)?;
        let held = v.holders.get(&a.hash)?;
        v.public
            .iter()
            .find(|(sid, _)| held.contains(sid))
            .map(|(_, url)| format!("{url}/{}", drive_files::blob::key(&a.hash)))
    }

    fn on(&self, part: &Part, location: &str) -> bool {
        self.view(part).is_some_and(|v| {
            v.locations
                .get(location)
                .is_some_and(|(dev, _, _)| v.online.contains(dev))
        })
    }

    /// Whether a file can be had now: kept available, or some computer
    /// with a copy (its own, a storage server's mirror, …) is on.
    async fn reachable(&self, c: &Catalog, part: &Part, f: &Found) -> bool {
        if self.kept(part, f).is_some() || self.on(part, &part.location) {
            return true;
        }
        if f.sampled.is_empty() {
            return false;
        }
        c.rooms
            .store
            .copies(self.space_of(part), &f.sampled, f.size)
            .await
            .unwrap_or_default()
            .iter()
            .any(|x| self.on(part, &x.location))
    }

    fn find(&self, part_id: &str, path: &str) -> Option<&(Part, Found)> {
        self.files
            .iter()
            .find(|(p, f)| p.id == part_id && f.path == path)
    }
}

/// Bring a file's bytes to the node from the best online source; its hash.
async fn bring(c: &Catalog, r: &Resolved, part: &Part, f: &Found) -> Result<Got, &'static str> {
    if let Some(a) = r.kept(part, f) {
        if c.blobs.has(&a.hash).await.unwrap_or(false) {
            return Ok(Got::Blob(a.hash.clone()));
        }
    }
    let space_id = r.space_of(part);
    let Some(v) = r.view(part) else {
        return Err("every computer that has this file is off");
    };
    // Every location holding the same content, storage servers first, then
    // the part's own, then the rest; only those whose device is on.
    let mut sources: Vec<(u8, String, String)> = Vec::new();
    if !f.sampled.is_empty() {
        for copy in c
            .rooms
            .store
            .copies(space_id, &f.sampled, f.size)
            .await
            .unwrap_or_default()
        {
            let Some((dev, _, _)) = v.locations.get(&copy.location) else {
                continue;
            };
            if !v.online.contains(dev) {
                continue;
            }
            let rank = if v.servers.contains(dev) {
                0
            } else if copy.location == part.location {
                1
            } else {
                2
            };
            sources.push((rank, copy.location, copy.path));
        }
    }
    if sources.is_empty()
        && v.locations
            .get(&part.location)
            .is_some_and(|(d, _, _)| v.online.contains(d))
    {
        sources.push((1, part.location.clone(), f.path.clone()));
    }
    sources.sort();
    for (_, loc, path) in sources {
        // Over iroh when that computer serves files that way: streamed,
        // not uploaded whole first (docs/transfer.md).
        if let Some((dev, _, _)) = v.locations.get(&loc) {
            let ask = drive_proto::Browse::ProvideLocation {
                location: loc.clone(),
                path: path.clone(),
            };
            match crate::transfer::provide(&c.rooms, space_id, dev, ask).await {
                Ok(Some((got, src))) => {
                    if c.blobs.has(&got.hash).await.unwrap_or(false) {
                        return Ok(Got::Blob(got.hash));
                    }
                    return Ok(Got::Device(got, src));
                }
                Ok(None) => {}
                Err(_) => continue,
            }
        }
        if let Ok(h) = c.rooms.fetch(space_id, &loc, &path).await {
            return Ok(Got::Blob(h));
        }
    }
    Err("every computer that has this file is off")
}

/// Where a shared file's bytes are now.
enum Got {
    /// On the node, as a blob.
    Blob(String),
    /// On a computer that serves it over iroh.
    Device(drive_proto::Provided, drive_transfer::Source),
}

/// `/s/{space}/{token}`: the share's page.
pub async fn page(
    UrlPath((space_id, token)): UrlPath<(String, String)>,
    State(c): State<Catalog>,
) -> Response {
    let r = match resolve(&c, &space_id, &token).await {
        Ok(r) => r,
        Err(s) => {
            return (
                s,
                Html(shell(
                    "Not found",
                    "<p class=empty>This link does not exist, or was taken back.</p>",
                )),
            )
                .into_response();
        }
    };
    let base = format!("/s/{}/{}", enc(&space_id), enc(&token));
    let total: i64 = r.files.iter().map(|(_, f)| f.size).sum();
    let mut ok_files = std::collections::HashSet::new();
    for (p, f) in &r.files {
        if r.reachable(&c, p, f).await {
            ok_files.insert((p.id.clone(), f.path.clone()));
        }
    }
    let reach = ok_files.len();
    let what = if r.share.kinds.is_empty() {
        "files".to_string()
    } else {
        format!("{} files", r.share.kinds.join(", "))
    };
    let mut body = format!(
        "<header><h1>{}</h1><p>{} {} · {}</p><a class=all href=\"{base}/zip\">Download all</a></header>",
        esc(&r.project.name),
        r.files.len(),
        esc(&what),
        human(total.max(0) as u64)
    );
    if reach < r.files.len() {
        body.push_str(&format!(
            "<p class=note>{} of them are on a computer that is off right now; the zip leaves them out (and says which).</p>",
            r.files.len() - reach
        ));
    }
    for part in &r.project.parts {
        let mine: Vec<&(Part, Found)> = r.files.iter().filter(|(p, _)| p.id == part.id).collect();
        if mine.is_empty() {
            continue;
        }
        let from = r
            .view(part)
            .and_then(|v| v.locations.get(&part.location))
            .map_or(String::new(), |(_, _, dev)| dev.clone());
        body.push_str(&format!(
            "<h2>{} <span>{}</span></h2><ul>",
            esc(&part.label),
            esc(&from)
        ));
        for (p, f) in mine {
            let ok = ok_files.contains(&(p.id.clone(), f.path.clone()));
            let href = format!("{base}/file?part={}&path={}", enc(&p.id), enc(&f.path));
            let fast = r.kept(p, f).is_some();
            // Review: a player for audio and video, and the file's comments
            // with a form (a guest's name, a note, at the current time).
            let media = media_of(&f.name);
            let review = if ok {
                let player = match media {
                    Some("audio") => {
                        format!("<audio controls preload=none src=\"{href}\"></audio>")
                    }
                    Some(_) => format!("<video controls preload=none src=\"{href}\"></video>"),
                    None => String::new(),
                };
                let at = if media.is_some() {
                    "<label class=atl><input type=checkbox name=at checked> at <span class=now>0:00</span></label>"
                } else {
                    ""
                };
                format!(
                    "<div class=review data-part=\"{}\" data-path=\"{}\">{player}<div class=cm></div>\
                     <form><input name=name placeholder=\"Your name\" required maxlength=80>\
                     <textarea name=body required maxlength=4000 placeholder=\"Leave a note\"></textarea>\
                     <div class=row>{at}<button>Comment</button></div></form></div>",
                    esc(&p.id),
                    esc(&f.path)
                )
            } else {
                String::new()
            };
            body.push_str(&format!(
                "<li class=\"{}\"><div class=line>{}<span class=size>{}</span>{}</div>{review}</li>",
                if ok { "" } else { "off" },
                if ok {
                    format!("<a href=\"{href}\">{}</a>", esc(p.relative(&f.path)))
                } else {
                    esc(p.relative(&f.path))
                },
                human(f.size.max(0) as u64),
                if fast {
                    "<span class=tag>ready</span>"
                } else if ok {
                    ""
                } else {
                    "<span class=tag>computer off</span>"
                }
            ));
        }
        body.push_str("</ul>");
    }
    body.push_str(&format!("<script>{REVIEW_JS}</script>"));
    Html(shell(&r.project.name, &body)).into_response()
}

/// Audio or video, by extension: the share page plays these.
fn media_of(name: &str) -> Option<&'static str> {
    let ext = name
        .rsplit_once('.')
        .map(|(_, e)| e.to_ascii_lowercase())
        .unwrap_or_default();
    match ext.as_str() {
        "wav" | "mp3" | "flac" | "aif" | "aiff" | "m4a" | "ogg" | "opus" => Some("audio"),
        "mp4" | "mov" | "m4v" | "webm" => Some("video"),
        _ => None,
    }
}

fn file_target(part: &Part, f: &Found) -> Target {
    Target::Indexed {
        location: part.location.clone(),
        path: f.path.clone(),
    }
}

/// `/s/{space}/{token}/comments?part=&path=`: a shared file's comments.
pub async fn comments(
    UrlPath((space_id, token)): UrlPath<(String, String)>,
    State(c): State<Catalog>,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    let r = match resolve(&c, &space_id, &token).await {
        Ok(r) => r,
        Err(s) => return s.into_response(),
    };
    let Some((part, f)) = r.find(
        q.get("part").map_or("", String::as_str),
        q.get("path").map_or("", String::as_str),
    ) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let id = drive_proto::qualify(&space_id, &file_target(part, f).doc_id());
    let threads = c.rooms.read(&id, comments::threads).await;
    let json: Vec<serde_json::Value> = threads
        .into_iter()
        .filter(|t| !t.root.deleted)
        .map(|t| {
            serde_json::json!({
                "id": t.root.id,
                "by": t.root.by,
                "created": t.root.created,
                "body": t.root.body,
                "resolved": t.root.resolved,
                "version": t.root.version,
                "at": match t.root.anchor { Anchor::Time { start_ms, .. } => Some(start_ms), _ => None },
                "replies": t.replies.iter().filter(|x| !x.deleted).map(|x| serde_json::json!({ "by": x.by, "body": x.body, "created": x.created })).collect::<Vec<_>>(),
            })
        })
        .collect();
    axum::Json(serde_json::json!({ "threads": json, "version": f.sampled })).into_response()
}

#[derive(serde::Deserialize)]
pub struct GuestComment {
    part: String,
    path: String,
    name: String,
    body: String,
    at_ms: Option<i64>,
}

/// `POST /s/{space}/{token}/comments`: a guest's note on a shared file (the
/// link is the permission; the guest names themself).
pub async fn comment(
    UrlPath((space_id, token)): UrlPath<(String, String)>,
    State(c): State<Catalog>,
    axum::Json(g): axum::Json<GuestComment>,
) -> Response {
    let (name, text) = (g.name.trim(), g.body.trim());
    if name.is_empty()
        || text.is_empty()
        || name.chars().count() > 80
        || text.chars().count() > 4000
    {
        return (
            StatusCode::BAD_REQUEST,
            "a name and a note, please (up to 80 and 4000 characters)",
        )
            .into_response();
    }
    let r = match resolve(&c, &space_id, &token).await {
        Ok(r) => r,
        Err(s) => return s.into_response(),
    };
    let Some((part, f)) = r.find(&g.part, &g.path) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let id = drive_proto::qualify(&space_id, &file_target(part, f).doc_id());
    let by = format!("{name} (guest)");
    let author = format!("guest:{name}");
    let anchor = match g.at_ms {
        Some(ms) if ms >= 0 => Anchor::Time {
            start_ms: ms,
            end_ms: ms,
        },
        _ => Anchor::Whole,
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64);
    let added = c
        .rooms
        .edit(&id, &by, |d| {
            comments::add(
                d,
                comments::New {
                    author: &author,
                    by: &by,
                    body: text,
                    anchor,
                    version: &f.sampled,
                    parent: None,
                },
                now,
            )
        })
        .await;
    match added {
        Ok(cid) => {
            tracing::info!(share.space = %space_id, comment.id = %cid, "a guest commented on a shared file");
            axum::Json(serde_json::json!({ "id": cid })).into_response()
        }
        Err(e) => {
            tracing::warn!(error = %e, "a guest comment did not save");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

/// The share page's review script: plays a file, lists its comments
/// (a time jumps the player there), and posts a guest's note at the
/// current time.
const REVIEW_JS: &str = r#"
const B=location.pathname.replace(/\/$/,'');
const fmt=ms=>{const s=Math.floor(ms/1000);return Math.floor(s/60)+':'+String(s%60).padStart(2,'0')};
const esc=t=>String(t).replace(/[&<>"]/g,c=>({'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;'}[c]));
document.querySelectorAll('.review').forEach(r=>{
  const part=r.dataset.part,path=r.dataset.path,au=r.querySelector('audio,video'),list=r.querySelector('.cm'),f=r.querySelector('form');
  const q='part='+encodeURIComponent(part)+'&path='+encodeURIComponent(path);
  const load=async()=>{
    const j=await(await fetch(B+'/comments?'+q)).json();
    list.innerHTML=j.threads.map(t=>'<div class=c>'+(t.at!=null?'<button type=button class=at data-ms='+t.at+'>'+fmt(t.at)+'</button>':'')
      +'<b>'+esc(t.by)+'</b> <span class=when>'+new Date(t.created).toLocaleString()+'</span>'
      +(t.resolved?' <span class=tag>resolved</span>':'')+(t.version!==j.version?' <span class=tag>earlier version</span>':'')
      +'<p>'+esc(t.body)+'</p>'+t.replies.map(x=>'<div class=reply><b>'+esc(x.by)+'</b> '+esc(x.body)+'</div>').join('')+'</div>').join('');
    list.querySelectorAll('.at').forEach(b=>b.onclick=()=>{if(au){au.currentTime=b.dataset.ms/1000;au.play()}});
  };
  f.name.value=localStorage.getItem('drive:guest')||'';
  if(au)au.addEventListener('timeupdate',()=>{const l=f.querySelector('.now');if(l)l.textContent=fmt(au.currentTime*1000)});
  f.onsubmit=async e=>{
    e.preventDefault();localStorage.setItem('drive:guest',f.name.value);
    const body={part,path,name:f.name.value,body:f.body.value};
    if(au&&f.at&&f.at.checked)body.at_ms=Math.round(au.currentTime*1000);
    const res=await fetch(B+'/comments',{method:'POST',headers:{'content-type':'application/json'},body:JSON.stringify(body)});
    if(res.ok){f.body.value='';load()}else alert(await res.text());
  };
  load();
});
"#;

/// `/s/{space}/{token}/file?part=&path=`: one file of the share.
pub async fn file(
    UrlPath((space_id, token)): UrlPath<(String, String)>,
    State(c): State<Catalog>,
    Query(q): Query<HashMap<String, String>>,
    headers: axum::http::HeaderMap,
) -> Response {
    let r = match resolve(&c, &space_id, &token).await {
        Ok(r) => r,
        Err(s) => return s.into_response(),
    };
    let (part_id, path) = (
        q.get("part").map_or("", String::as_str),
        q.get("path").map_or("", String::as_str),
    );
    let Some((part, f)) = r.find(part_id, path) else {
        tracing::warn!(share.space = %space_id, "a file outside a share was asked for");
        return StatusCode::NOT_FOUND.into_response();
    };
    if let Some(url) = r.kept(part, f).and_then(|a| r.public_url(part, a)) {
        return Redirect::temporary(&url).into_response();
    }
    match bring(&c, &r, part, f).await {
        Ok(Got::Blob(hash)) => {
            let mut query = HashMap::new();
            query.insert("name".to_string(), f.name.clone());
            match crate::get_blob_ranged(
                UrlPath(hash),
                Query(query),
                headers,
                State(c.blobs.clone()),
            )
            .await
            {
                Ok(resp) => resp,
                Err(s) => s.into_response(),
            }
        }
        Ok(Got::Device(got, src)) => match c.rooms.transfer.get() {
            Some(t) => {
                let reply = crate::transfer::Reply {
                    name: f.name.clone(),
                    cache: Some(c.blobs.clone()),
                };
                crate::transfer::respond(t, &got, src, &headers, reply)
            }
            None => StatusCode::SERVICE_UNAVAILABLE.into_response(),
        },
        Err(why) => (StatusCode::SERVICE_UNAVAILABLE, why).into_response(),
    }
}

/// One file of the zip: its bytes, or a stream of them from a computer.
enum Entry {
    Bytes(Vec<u8>),
    Stream(tokio::sync::mpsc::Receiver<std::io::Result<Bytes>>),
}

/// A `Write` that hands its bytes to the response body.
struct Pipe(tokio::sync::mpsc::Sender<Result<Vec<u8>, std::io::Error>>);

impl Write for Pipe {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.blocking_send(Ok(buf.to_vec())).map_err(|_| {
            std::io::Error::new(std::io::ErrorKind::BrokenPipe, "the download was cancelled")
        })?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// `/s/{space}/{token}/zip`: everything reachable, as one zip, streamed as
/// each file arrives (stored, not compressed: audio does not shrink).
pub async fn zip(
    UrlPath((space_id, token)): UrlPath<(String, String)>,
    State(c): State<Catalog>,
) -> Response {
    let r = match resolve(&c, &space_id, &token).await {
        Ok(r) => r,
        Err(s) => return s.into_response(),
    };
    let name = format!(
        "{}.zip",
        r.project.name.replace(['/', '\\', '"', '\r', '\n'], "_")
    );
    let (body_tx, body_rx) = tokio::sync::mpsc::channel::<Result<Vec<u8>, std::io::Error>>(8);
    let (file_tx, file_rx) = std::sync::mpsc::sync_channel::<(String, Entry)>(1);

    // Writer: a blocking task, one entry per file that arrives.
    tokio::task::spawn_blocking(move || {
        let mut z = zip::ZipWriter::new_stream(Pipe(body_tx));
        let opts = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored)
            .large_file(true);
        for (entry, bytes) in file_rx {
            if z.start_file(entry, opts).is_err() {
                return;
            }
            match bytes {
                Entry::Bytes(b) => {
                    if z.write_all(&b).is_err() {
                        return;
                    }
                }
                // As it comes from the computer; a file that stops coming
                // stops the zip (the download fails rather than lies).
                Entry::Stream(mut rx) => {
                    while let Some(chunk) = rx.blocking_recv() {
                        match chunk {
                            Ok(b) => {
                                if z.write_all(&b).is_err() {
                                    return;
                                }
                            }
                            Err(_) => return,
                        }
                    }
                }
            }
        }
        let _ = z.finish();
    });

    // Reader: bring each file from wherever it is, in order.
    tokio::spawn(async move {
        let mut missing = Vec::new();
        for (part, f) in &r.files {
            let entry = format!("{}/{}", part.label, part.relative(&f.path));
            let bytes = match bring(&c, &r, part, f).await {
                Ok(Got::Blob(h)) => c.blobs.get(&h).await.ok().map(Entry::Bytes),
                Ok(Got::Device(got, src)) => c.rooms.transfer.get().map(|t| {
                    let (tx, rx) = tokio::sync::mpsc::channel(16);
                    let stream = crate::transfer::tee(
                        t.clone(),
                        src.clone(),
                        c.blobs.clone(),
                        t.fetcher.stream(src, 0, got.size.saturating_sub(1)),
                    );
                    tokio::spawn(async move {
                        let mut stream = std::pin::pin!(stream);
                        while let Some(chunk) = futures::StreamExt::next(&mut stream).await {
                            if tx.send(chunk).await.is_err() {
                                return;
                            }
                        }
                    });
                    Entry::Stream(rx)
                }),
                Err(_) => None,
            };
            match bytes {
                Some(b) => {
                    if file_tx.send((entry, b)).is_err() {
                        return;
                    }
                }
                None => missing.push(entry),
            }
        }
        if !missing.is_empty() {
            let note = format!(
                "These files are on a computer that was off when this zip was made. Download again later for them.\n\n{}\n",
                missing.join("\n")
            );
            let _ = file_tx.send(("MISSING.txt".into(), Entry::Bytes(note.into_bytes())));
        }
    });

    let mut resp = Response::new(Body::from_stream(tokio_stream_from(body_rx)));
    let h = resp.headers_mut();
    h.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/zip"),
    );
    if let Ok(v) = HeaderValue::from_str(&format!("attachment; filename=\"{name}\"")) {
        h.insert(header::CONTENT_DISPOSITION, v);
    }
    resp
}

fn tokio_stream_from(
    mut rx: tokio::sync::mpsc::Receiver<Result<Vec<u8>, std::io::Error>>,
) -> impl futures::Stream<Item = Result<Vec<u8>, std::io::Error>> {
    futures::stream::poll_fn(move |cx| rx.poll_recv(cx))
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn enc(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

fn human(n: u64) -> String {
    match n {
        0..1024 => format!("{n} B"),
        1024..1_048_576 => format!("{:.1} KB", n as f64 / 1024.0),
        1_048_576..1_073_741_824 => format!("{:.1} MB", n as f64 / 1_048_576.0),
        _ => format!("{:.2} GB", n as f64 / 1_073_741_824.0),
    }
}

fn shell(title: &str, body: &str) -> String {
    format!(
        "<!DOCTYPE html><html><head><meta charset=utf-8><meta name=viewport content=\"width=device-width, initial-scale=1\">\
         <title>{}</title><style>{CSS}</style></head><body><main>{body}<footer>Shared with Drive</footer></main></body></html>",
        esc(title)
    )
}

const CSS: &str = "\
:root{color-scheme:dark}*{box-sizing:border-box}\
body{margin:0;background:#000;color:#f4f4f5;font:15px/1.45 -apple-system,system-ui,sans-serif}\
main{max-width:760px;margin:0 auto;padding:28px 18px 60px}\
header{display:flex;flex-wrap:wrap;align-items:center;gap:6px 16px;margin-bottom:18px}\
h1{margin:0;font-size:28px;letter-spacing:-.02em;flex-basis:100%}\
header p{margin:0;color:#a1a1aa;flex:1}\
a.all{background:#f4f4f5;color:#000;text-decoration:none;font-weight:650;padding:10px 16px;border-radius:12px}\
.note{color:#f5d68a;font-size:14px}\
h2{font-size:13px;text-transform:uppercase;letter-spacing:.06em;color:#8b8b94;margin:24px 2px 8px}\
h2 span{text-transform:none;letter-spacing:0;font-weight:500;margin-left:6px}\
ul{list-style:none;margin:0;padding:0;background:#141417;border:1px solid #1d1d22;border-radius:14px}\
li{display:flex;align-items:center;gap:10px;padding:10px 14px;border-top:1px solid #1d1d22}\
li:first-child{border-top:0}\
li a{color:#2dd4bf;text-decoration:none;font-weight:600;flex:1;min-width:0;overflow-wrap:anywhere}\
li.off{color:#8b8b94}li.off{flex:1}\
.size{color:#8b8b94;font-size:13px;margin-left:auto;white-space:nowrap}\
.tag{font-size:11.5px;font-weight:650;padding:2px 8px;border-radius:9px;background:#1f1f23;color:#a1a1aa;white-space:nowrap}\
footer{margin-top:40px;color:#52525b;font-size:12px;text-align:center}\
li{flex-direction:column;align-items:stretch}.line{display:flex;align-items:center;gap:10px}\
.review{display:flex;flex-direction:column;gap:8px;margin-top:8px}\
.review audio,.review video{width:100%;max-height:320px;border-radius:10px}\
.cm .c{background:#1f1f23;border-radius:10px;padding:8px 10px;margin-bottom:6px}\
.cm .c p{margin:4px 0 0}.cm .when{color:#8b8b94;font-size:12px}\
.cm .reply{margin:6px 0 0 14px;font-size:14px;color:#d4d4d8}\
.at{background:#2dd4bf;color:#042f2a;border:0;border-radius:8px;padding:1px 7px;font-weight:700;margin-right:6px;cursor:pointer}\
form{display:flex;flex-direction:column;gap:6px}\
form input,form textarea{background:#0e0e11;color:#f4f4f5;border:1px solid #2b2b31;border-radius:10px;padding:8px 10px;font:inherit}\
form textarea{min-height:60px;resize:vertical}.row{display:flex;align-items:center;gap:10px}\
.row button{margin-left:auto;background:#f4f4f5;color:#000;border:0;border-radius:10px;padding:8px 14px;font-weight:650;cursor:pointer}\
.atl{color:#a1a1aa;font-size:13px}";

//! The agent's always-running part: answers the node, keeps its volumes
//! and locations' indexes current (a watcher, and a full pass now and
//! then; unchanged files are not read again), and keeps the Computers
//! view fresh.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use drive_files::space;
use drive_native::Event;
use notify::Watcher;
use tokio::sync::mpsc;

use crate::agent::Agent;
use crate::view::View;

/// Devices whose agent is connected now (from the registry's presence),
/// this one included.
pub fn online_devices(a: &Agent) -> HashSet<String> {
    let mut on: HashSet<String> = a
        .drive
        .others(&a.space_id)
        .into_iter()
        .filter_map(|(_, _, json)| {
            serde_json::from_str::<serde_json::Value>(&json).ok()?["data"]["device"]
                .as_str()
                .map(str::to_string)
        })
        .collect();
    on.insert(a.device_id());
    on
}

pub async fn refresh_view(a: &Agent, view: &RwLock<View>) {
    let indexes = a.refresh_indexes().await;
    let devices = space::devices(&a.space)
        .into_iter()
        .map(|d| (d.id, d.name))
        .collect();
    let built = View::build(
        &a.device_id(),
        devices,
        space::locations(&a.space),
        indexes,
        online_devices(a),
    );
    *view.write().expect("view") = built;
}

/// Run until the process stops.
pub async fn serve(a: Arc<Agent>, view: Arc<RwLock<View>>) {
    let (space_name, me) = {
        let c = a.cfg.lock().expect("cfg");
        (c.space.clone(), c.device_id.clone())
    };
    a.drive.register_agent(&space_name, &me);
    let mut events = a.drive.events();
    a.report_volumes();

    // Changes under a local location: reindex it a few seconds after they stop.
    let (changed_tx, mut changed) = mpsc::unbounded_channel::<String>();
    let mut watchers = Vec::new();
    for loc in a
        .my_locations()
        .into_iter()
        .filter(|l| l.storage.is_empty())
    {
        let tx = changed_tx.clone();
        let id = loc.id.clone();
        match notify::recommended_watcher(move |r: notify::Result<notify::Event>| {
            if r.is_ok_and(|e| !e.kind.is_access()) {
                let _ = tx.send(id.clone());
            }
        }) {
            Ok(mut w) => match w.watch(
                std::path::Path::new(&loc.path),
                notify::RecursiveMode::Recursive,
            ) {
                Ok(()) => watchers.push(w),
                Err(e) => {
                    tracing::warn!(location.name = %loc.name, error = %e, "a location cannot be watched; it is rescanned every ten minutes")
                }
            },
            Err(e) => tracing::warn!(error = %e, "no file watcher"),
        }
    }

    for loc in a.my_locations() {
        if let Err(e) = a.index_location(&loc).await {
            tracing::warn!(location.name = %loc.name, error = %e, "a location did not index");
        }
    }
    refresh_view(&a, &view).await;

    // File rules (crate::rules): after a location is indexed, on their
    // schedules and when asked; the first pass catches what changed while
    // the agent was off.
    let rules = Arc::new(crate::rules::Engine::open(&a).await);
    let ruling = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let mut rules_due: Vec<String> = a.my_locations().into_iter().map(|l| l.id).collect();
    let mut last_rules = tokio::time::Instant::now();

    // Sites: checked a few seconds after their pages, theme or settings
    // change, and every minute (new sites, builds asked for elsewhere).
    let building = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let mut sites_due: Option<tokio::time::Instant> =
        Some(tokio::time::Instant::now() + Duration::from_secs(3));
    let mut last_sites = tokio::time::Instant::now();
    let mut listed: Vec<String> = drive_files::sites::listed(&a.space).into_keys().collect();

    let mut pending: HashMap<String, tokio::time::Instant> = HashMap::new();
    let mut tick = tokio::time::interval(Duration::from_secs(2));
    let mut last_full = tokio::time::Instant::now();
    let mut last_view = tokio::time::Instant::now();
    let mut last_place = tokio::time::Instant::now() - Duration::from_secs(3600);
    // Sidecars: missing ones made in the background, a few files at a
    // time, starting a minute after the first indexing.
    let backfilling = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let mut last_backfill = tokio::time::Instant::now() - Duration::from_secs(540);
    loop {
        tokio::select! {
            ev = events.recv() => match ev {
                Ok(Event::Fetch { req, location, path }) => {
                    let a = a.clone();
                    tokio::spawn(async move {
                        let r = a.answer_fetch(&location, &path).await;
                        if let Err(e) = &r {
                            tracing::warn!(fetch.location = %location, error = %e, "a fetch was refused");
                        }
                        a.drive.fetched(req, r);
                    });
                }
                Ok(Event::Sidecars { req, location, path, kinds, force: _ }) => {
                    let a = a.clone();
                    tokio::spawn(async move {
                        let r = a.answer_sidecars(&location, &path, &kinds).await;
                        if let Err(e) = &r
                            && !e.starts_with("no generator")
                        {
                            tracing::warn!(sidecar.location = %location, error = %e, "sidecars could not be made");
                        }
                        a.drive.fetched(req, r);
                    });
                }
                Ok(Event::Browse { req, ask }) => {
                    let a = a.clone();
                    tokio::spawn(async move {
                        let index = matches!(ask, drive_proto::Browse::Index { .. });
                        let r = a.answer_browse(ask).await;
                        let added = match (&r, index) {
                            (Ok(drive_proto::Browsed::Key(id)), true) => Some(id.clone()),
                            _ => None,
                        };
                        a.drive.browsed(req, r);
                        // A folder made a location from a browser: index it now
                        // (the full pass every ten minutes keeps it after).
                        if let Some(loc) = added.and_then(|id| a.my_locations().into_iter().find(|l| l.id == id))
                            && let Err(e) = a.index_location(&loc).await
                        {
                            tracing::warn!(location.name = %loc.name, error = %e, "a new location did not index");
                        }
                    });
                }
                Ok(Event::Online(true)) => {
                    a.announce();
                }
                Ok(Event::Changed(doc)) if doc == a.space_id => {
                    // A site added or removed (the Drive app, another device).
                    let now: Vec<String> = drive_files::sites::listed(&a.space).into_keys().collect();
                    if now != listed {
                        listed = now;
                        let at = tokio::time::Instant::now() + Duration::from_secs(2);
                        sites_due = Some(sites_due.map_or(at, |d| d.min(at)));
                    }
                }
                Ok(Event::Changed(doc)) if doc == a.files_id || doc.contains("/drive:site:") => {
                    let at = tokio::time::Instant::now() + Duration::from_secs(4);
                    sites_due = Some(sites_due.map_or(at, |d| d.min(at)));
                }
                Ok(Event::Presence { .. }) => {
                    view.write().expect("view").online = online_devices(&a);
                }
                Ok(_) => {}
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                Err(_) => break,
            },
            Some(id) = changed.recv() => {
                pending.insert(id, tokio::time::Instant::now() + Duration::from_secs(5));
            }
            _ = tick.tick() => {
                let now = tokio::time::Instant::now();
                let due: Vec<String> = pending.iter().filter(|(_, t)| **t <= now).map(|(k, _)| k.clone()).collect();
                for id in due {
                    pending.remove(&id);
                    if let Some(loc) = a.my_locations().into_iter().find(|l| l.id == id) {
                        match a.index_location(&loc).await {
                            Ok(_) => rules_due.push(loc.id.clone()),
                            Err(e) => tracing::warn!(location.name = %loc.name, error = %e, "a location did not reindex"),
                        }
                    }
                }
                if (!rules_due.is_empty() || now.duration_since(last_rules) > Duration::from_secs(10))
                    && !ruling.swap(true, std::sync::atomic::Ordering::SeqCst)
                {
                    last_rules = now;
                    let locs = std::mem::take(&mut rules_due);
                    let (a, rules, ruling) = (a.clone(), rules.clone(), ruling.clone());
                    tokio::spawn(async move {
                        for l in locs {
                            rules.after_index(&a, &l).await;
                        }
                        rules.tick(&a).await;
                        ruling.store(false, std::sync::atomic::Ordering::SeqCst);
                    });
                }
                if now.duration_since(last_full) > Duration::from_secs(600) {
                    last_full = now;
                    a.report_volumes();
                    for loc in a.my_locations() {
                        let _ = a.index_location(&loc).await;
                    }
                }
                if now.duration_since(last_place) > Duration::from_secs(60) {
                    last_place = now;
                    a.place().await;
                    a.make_available().await;
                    a.publish_available().await;
                }
                if now.duration_since(last_sites) > Duration::from_secs(60) && sites_due.is_none() {
                    sites_due = Some(now);
                }
                if sites_due.is_some_and(|d| d <= now)
                    && !building.swap(true, std::sync::atomic::Ordering::SeqCst)
                {
                    sites_due = None;
                    last_sites = now;
                    let (a, building) = (a.clone(), building.clone());
                    tokio::spawn(async move {
                        a.build_due_sites().await;
                        building.store(false, std::sync::atomic::Ordering::SeqCst);
                    });
                }
                if now.duration_since(last_backfill) > Duration::from_secs(600)
                    && pending.is_empty()
                    && !backfilling.swap(true, std::sync::atomic::Ordering::SeqCst)
                {
                    last_backfill = now;
                    let (a, backfilling) = (a.clone(), backfilling.clone());
                    tokio::spawn(async move {
                        let p = a.backfill_sidecars(&[], false, 200).await;
                        if p.files > 0 {
                            tracing::info!(
                                sidecars.files = p.files,
                                sidecars.made = p.made,
                                sidecars.no_generator = p.no_generator,
                                sidecars.failed = p.failed,
                                "sidecars made in the background"
                            );
                        }
                        backfilling.store(false, std::sync::atomic::Ordering::SeqCst);
                    });
                }
                if now.duration_since(last_view) > Duration::from_secs(30) {
                    last_view = now;
                    a.announce();
                    refresh_view(&a, &view).await;
                }
            }
        }
    }
    drop(watchers);
}

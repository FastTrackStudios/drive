//! Where this process reads and writes location indexes (drive#157).
//!
//! The agent that serves the app socket (`drive serve`, `drive mount`)
//! owns the index database (`indexes.db`). A `drive` command run beside
//! it asks it over `app.sock` ([`drive_local::proto::IndexOp`]) instead of
//! opening the database too, and opens it itself only when no agent
//! answers there (or the agent goes away while it runs). Several
//! processes on one database is then only the no-daemon case: two
//! `drive` commands at once.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use drive_files::index::{IndexEntry, WalkStats};
use drive_local::proto::{IndexOp, Op};
use serde::de::DeserializeOwned;
use tokio::sync::OnceCell;

use crate::agent::Agent;
use crate::index_db::{IndexDb, Meta};

/// Why an index could not be read or written.
#[derive(Debug)]
pub enum IndexError {
    /// This process's own database.
    Db(turso::Error),
    /// The running agent's answer.
    Daemon(String),
}

impl std::fmt::Display for IndexError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            IndexError::Db(e) => write!(f, "{e}"),
            IndexError::Daemon(e) => write!(f, "the running agent: {e}"),
        }
    }
}

impl std::error::Error for IndexError {}

impl From<turso::Error> for IndexError {
    fn from(e: turso::Error) -> Self {
        IndexError::Db(e)
    }
}

pub type Result<T> = std::result::Result<T, IndexError>;

/// This process's way to the location indexes. Cheap to clone.
#[derive(Clone)]
pub struct Indexes(Arc<Inner>);

struct Inner {
    /// The agent's folder (its `app.sock`, its `indexes.db`).
    dir: PathBuf,
    /// This process serves the app socket: it never asks anyone else.
    owner: AtomicBool,
    /// The running agent, once looked for (`None`: none answered).
    daemon: OnceCell<Option<Arc<drive_local::Client>>>,
    /// It went away: everything is this process's own from then on.
    gone: AtomicBool,
    db: OnceCell<Arc<IndexDb>>,
}

/// The agent's index database in `dir`.
fn db_path(dir: &Path) -> PathBuf {
    dir.join("indexes.db")
}

impl Indexes {
    /// The indexes of the agent in `dir`: nothing is opened or asked until
    /// they are first used.
    pub fn new(dir: &Path) -> Indexes {
        Indexes(Arc::new(Inner {
            dir: dir.to_path_buf(),
            owner: AtomicBool::new(false),
            daemon: OnceCell::new(),
            gone: AtomicBool::new(false),
            db: OnceCell::new(),
        }))
    }

    /// This process serves the app socket (it is the running agent):
    /// its indexes are its own database, which is opened now.
    ///
    /// # Errors
    /// The database would not open.
    pub async fn claim(&self) -> Result<()> {
        self.0.owner.store(true, Ordering::SeqCst);
        self.db().await.map(|_| ())
    }

    /// The database, opened here (once). Only the owner, or a process that
    /// found no running agent, should hold it.
    ///
    /// # Errors
    /// It would not open.
    pub async fn db(&self) -> Result<&Arc<IndexDb>> {
        self.0
            .db
            .get_or_try_init(|| async { IndexDb::open(&db_path(&self.0.dir)).await.map(Arc::new) })
            .await
            .map_err(IndexError::Db)
    }

    /// The running agent, if one answers on the app socket and this
    /// process is not it.
    async fn daemon(&self) -> Option<Arc<drive_local::Client>> {
        if self.0.owner.load(Ordering::SeqCst) || self.0.gone.load(Ordering::SeqCst) {
            return None;
        }
        let dir = self.0.dir.clone();
        self.0
            .daemon
            .get_or_init(|| async move {
                tokio::task::spawn_blocking(move || drive_local::Client::connect_in(&dir, "drive"))
                    .await
                    .ok()
                    .and_then(std::result::Result::ok)
                    .map(Arc::new)
            })
            .await
            .clone()
    }

    /// Whether a running agent (not this process) holds the indexes.
    pub async fn asks_daemon(&self) -> bool {
        self.daemon().await.is_some()
    }

    /// Ask the running agent; `None` when there is none (or it went away
    /// meanwhile): the caller then uses its own database.
    async fn ask<T: DeserializeOwned + Send + 'static>(&self, op: IndexOp) -> Option<Result<T>> {
        let client = self.daemon().await?;
        let answer = tokio::task::spawn_blocking(move || client.call_op(Op::Index(op))).await;
        let value = match answer {
            Ok(Ok(v)) => v,
            Ok(Err(drive_local::Error::Failed(why))) => return Some(Err(IndexError::Daemon(why))),
            // Closed, not running, the socket gone: no agent any more.
            Ok(Err(_)) | Err(_) => {
                self.0.gone.store(true, Ordering::SeqCst);
                return None;
            }
        };
        Some(
            serde_json::from_value(value)
                .map_err(|e| IndexError::Daemon(format!("an answer of the wrong shape: {e}"))),
        )
    }

    pub async fn meta(&self, location: &str) -> Result<Option<Meta>> {
        let op = IndexOp::Meta {
            location: location.into(),
        };
        if let Some(r) = self.ask(op).await {
            return r;
        }
        Ok(self.db().await?.meta(location).await?)
    }

    pub async fn get(&self, location: &str, path: &str) -> Result<Option<IndexEntry>> {
        let op = IndexOp::Get {
            location: location.into(),
            path: path.into(),
        };
        if let Some(r) = self.ask(op).await {
            return r;
        }
        Ok(self.db().await?.get(location, path).await?)
    }

    pub async fn list(&self, location: &str, dir: &str) -> Result<Vec<IndexEntry>> {
        let op = IndexOp::List {
            location: location.into(),
            dir: dir.into(),
        };
        if let Some(r) = self.ask(op).await {
            return r;
        }
        Ok(self.db().await?.list(location, dir).await?)
    }

    pub async fn page_under(
        &self,
        location: &str,
        dir: &str,
        after: &str,
        limit: usize,
    ) -> Result<Vec<IndexEntry>> {
        let op = IndexOp::Page {
            location: location.into(),
            dir: dir.into(),
            after: after.into(),
            limit,
        };
        if let Some(r) = self.ask(op).await {
            return r;
        }
        Ok(self
            .db()
            .await?
            .page_under(location, dir, after, limit)
            .await?)
    }

    pub async fn locations_of(&self, sampled: &[String]) -> Result<HashMap<String, Vec<String>>> {
        let op = IndexOp::LocationsOf {
            sampled: sampled.to_vec(),
        };
        if let Some(r) = self.ask(op).await {
            return r;
        }
        Ok(self.db().await?.locations_of(sampled).await?)
    }

    pub async fn duplicates(&self, min_size: u64) -> Result<Vec<(String, u64, u64)>> {
        if let Some(r) = self.ask(IndexOp::Duplicates { min_size }).await {
            return r;
        }
        Ok(self.db().await?.duplicates(min_size).await?)
    }

    /// [`IndexDb::duplicates_under`]. From the running agent the groups
    /// come in one answer.
    pub async fn duplicates_under(
        &self,
        location: &str,
        dir: &str,
        mut each: impl FnMut(String, u64, u64),
    ) -> Result<()> {
        let op = IndexOp::DuplicatesUnder {
            location: location.into(),
            dir: dir.into(),
        };
        if let Some(r) = self.ask::<Vec<(String, u64, u64)>>(op).await {
            r?.into_iter().for_each(|(s, size, n)| each(s, size, n));
            return Ok(());
        }
        Ok(self
            .db()
            .await?
            .duplicates_under(location, dir, each)
            .await?)
    }

    pub async fn copies(
        &self,
        sampled: &str,
        size: Option<u64>,
    ) -> Result<Vec<(String, IndexEntry)>> {
        let op = IndexOp::Copies {
            sampled: sampled.into(),
            size,
        };
        if let Some(r) = self.ask(op).await {
            return r;
        }
        Ok(self.db().await?.copies(sampled, size).await?)
    }

    /// Forget a location's index.
    pub async fn remove(&self, location: &str) -> Result<()> {
        if let Some(r) = self
            .ask(IndexOp::Remove {
                location: location.into(),
            })
            .await
        {
            return r;
        }
        Ok(self.db().await?.remove(location).await?)
    }

    /// Ask the running agent to walk and publish one of its locations
    /// (`None`: there is none; walk it here).
    pub async fn reindex_by_daemon(&self, location: &str) -> Option<Result<WalkStats>> {
        self.ask(IndexOp::Reindex {
            location: location.into(),
        })
        .await
    }

    /// Ask the running agent to download the indexes newer on the node
    /// (`None`: there is none; download them here).
    pub async fn sync_by_daemon(&self) -> Option<Result<()>> {
        self.ask(IndexOp::Sync).await
    }

    /// For tests and the stress test: the database's path.
    #[cfg(test)]
    pub fn path(&self) -> PathBuf {
        db_path(&self.0.dir)
    }
}

/// Entries whose name holds `words`, in every location (`None`: this
/// computer has no location indexes), for `drive where`, which must work
/// with Drive down and change nothing: asked of the running agent, or
/// read from the database if it is there (none is made).
///
/// # Errors
/// The database or the agent could not be read.
pub async fn find_existing(
    dir: &Path,
    words: &str,
    limit: usize,
) -> Result<Option<Vec<(String, IndexEntry)>>> {
    let indexes = Indexes::new(dir);
    let op = IndexOp::Find {
        words: words.into(),
        limit,
    };
    if let Some(r) = indexes.ask(op).await {
        return r.map(Some);
    }
    match IndexDb::open_existing(&db_path(dir)).await? {
        Some(db) => Ok(Some(db.find(words, limit).await?)),
        None => Ok(None),
    }
}

/// How long the running agent waits for a location a `drive` command
/// just added to reach it (through the node, as every edit does).
const NEW_LOCATION_WAIT: std::time::Duration = std::time::Duration::from_secs(15);

/// The running agent's answer to a `drive` command's [`IndexOp`], from its
/// own database (never asking anyone: it is the one asked).
pub async fn answer(a: &Arc<Agent>, op: IndexOp) -> std::result::Result<serde_json::Value, String> {
    let db = a.index_db.db().await.map_err(|e| e.to_string())?;
    match op {
        IndexOp::Meta { location } => json(db.meta(&location).await.map_err(err)?),
        IndexOp::Get { location, path } => json(db.get(&location, &path).await.map_err(err)?),
        IndexOp::Find { words, limit } => json(db.find(&words, limit).await.map_err(err)?),
        IndexOp::List { location, dir } => json(db.list(&location, &dir).await.map_err(err)?),
        IndexOp::Page {
            location,
            dir,
            after,
            limit,
        } => json(
            db.page_under(&location, &dir, &after, limit.min(10_000))
                .await
                .map_err(err)?,
        ),
        IndexOp::LocationsOf { sampled } => json(db.locations_of(&sampled).await.map_err(err)?),
        IndexOp::Duplicates { min_size } => json(db.duplicates(min_size).await.map_err(err)?),
        IndexOp::DuplicatesUnder { location, dir } => {
            let mut groups = Vec::new();
            db.duplicates_under(&location, &dir, |s, size, n| groups.push((s, size, n)))
                .await
                .map_err(err)?;
            json(groups)
        }
        IndexOp::Copies { sampled, size } => json(db.copies(&sampled, size).await.map_err(err)?),
        IndexOp::Remove { location } => json(db.remove(&location).await.map_err(err)?),
        IndexOp::Reindex { location } => json(reindex(a.clone(), location).await?),
        IndexOp::Sync => {
            a.sync_indexes().await;
            Ok(serde_json::Value::Null)
        }
    }
}

fn err(e: turso::Error) -> String {
    e.to_string()
}

fn json(v: impl serde::Serialize) -> std::result::Result<serde_json::Value, String> {
    serde_json::to_value(v).map_err(|e| e.to_string())
}

/// Walk and publish one of this device's locations for a `drive` command.
/// Its own task: the command going away (its connection closed) does not
/// cut a walk short.
async fn reindex(a: Arc<Agent>, location: String) -> std::result::Result<WalkStats, String> {
    tokio::spawn(async move {
        let loc = wait_for_location(&a, &location).await?;
        a.index_location(&loc).await
    })
    .await
    .map_err(|e| e.to_string())?
}

/// One of this device's locations, waited for a moment (a `drive` command
/// that just added it reaches this agent through the node).
async fn wait_for_location(
    a: &Agent,
    id: &str,
) -> std::result::Result<drive_files::space::Location, String> {
    let until = tokio::time::Instant::now() + NEW_LOCATION_WAIT;
    loop {
        if let Some(loc) = a.my_locations().into_iter().find(|l| l.id == id) {
            return Ok(loc);
        }
        if tokio::time::Instant::now() >= until {
            return Err(format!(
                "the running agent does not know location {id} yet (it indexes it once it does)"
            ));
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
}

#[cfg(test)]
mod tests;

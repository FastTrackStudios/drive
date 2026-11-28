//! A watch on the agent's own FUSE mount (drive#210): a request the mount
//! has not answered for a while is a warning, and a thread of this agent
//! waiting on a FUSE answer itself is an error (the way the mount
//! deadlocked), so a hang shows in the journal instead of only as programs
//! stuck in the kernel.
//!
//! It reads the kernel's count of the connection's waiting requests
//! (`/sys/fs/fuse/connections/<n>/waiting`) and its own threads' wait
//! channels (`/proc/self/task/*/wchan`): nothing on the mount is touched.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// How often the mount is looked at.
const EVERY: Duration = Duration::from_secs(5);
/// How long requests may wait before it is said.
pub const STUCK: Duration = Duration::from_secs(30);
/// How often a hang that goes on is said again.
const AGAIN: Duration = Duration::from_secs(300);

/// Watch the FUSE mount at `mount` for as long as it is mounted.
pub fn watch(mount: PathBuf) {
    let spawned = std::thread::Builder::new()
        .name("fuse-watch".into())
        .spawn(move || run(&mount));
    if let Err(e) = spawned {
        tracing::warn!(error = %e, "the mount's watch could not start");
    }
}

fn run(mount: &Path) {
    // The connection appears a moment after the mount call returns.
    let mut conn = None;
    for _ in 0..20 {
        conn = connection(mount);
        if conn.is_some() {
            break;
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    let Some(conn) = conn else {
        tracing::info!(mount = %mount.display(), "the mount's FUSE connection is not visible (no fusectl); it is not watched");
        return;
    };
    let mut state = Watch::default();
    loop {
        std::thread::sleep(EVERY);
        let Some(waiting) = read_waiting(&conn) else {
            return; // unmounted
        };
        let now = Instant::now();
        if let Some(said) = state.see(waiting, now) {
            let own = own_threads_waiting();
            let stuck_s = said.as_secs();
            if own.is_empty() {
                tracing::warn!(
                    mount = %mount.display(),
                    fuse.waiting = waiting,
                    fuse.stuck_s = stuck_s,
                    "FUSE requests on the mount have waited a long time; programs reading it may hang"
                );
            } else {
                tracing::error!(
                    mount = %mount.display(),
                    fuse.waiting = waiting,
                    fuse.stuck_s = stuck_s,
                    fuse.own_threads = %own.join(","),
                    "the agent is waiting on a FUSE answer itself: the mount may be deadlocked (drive#210)"
                );
            }
        }
    }
}

/// When to speak, from the waiting counts seen over time.
#[derive(Debug, Default)]
struct Watch {
    /// Since when requests have been waiting without a break.
    since: Option<Instant>,
    said: Option<Instant>,
}

impl Watch {
    /// Note `waiting` requests at `now`; how long they have been waiting,
    /// when it is time to say so.
    fn see(&mut self, waiting: u64, now: Instant) -> Option<Duration> {
        if waiting == 0 {
            *self = Watch::default();
            return None;
        }
        let since = *self.since.get_or_insert(now);
        let stuck = now.duration_since(since);
        if stuck < STUCK || self.said.is_some_and(|s| now.duration_since(s) < AGAIN) {
            return None;
        }
        self.said = Some(now);
        Some(stuck)
    }
}

/// The fusectl folder of the FUSE connection mounted at `mount`: named by
/// the minor of its anonymous device (`0:226` → `226`).
fn connection(mount: &Path) -> Option<PathBuf> {
    let info = std::fs::read_to_string("/proc/self/mountinfo").ok()?;
    let minor = minor_of(&info, mount)?;
    let dir = PathBuf::from(format!("/sys/fs/fuse/connections/{minor}"));
    dir.join("waiting").exists().then_some(dir)
}

fn minor_of(info: &str, mount: &Path) -> Option<u32> {
    let want = mount.to_str()?;
    info.lines().find_map(|line| {
        let mut f = line.split(' ');
        let dev = f.nth(2)?;
        let at = f.nth(1)?;
        let fs = line.split_once(" - ")?.1.split(' ').next()?;
        if at != want.replace(' ', "\\040") || !fs.starts_with("fuse") {
            return None;
        }
        dev.split_once(':')?.1.parse().ok()
    })
}

fn read_waiting(conn: &Path) -> Option<u64> {
    std::fs::read_to_string(conn.join("waiting"))
        .ok()?
        .trim()
        .parse()
        .ok()
}

/// This process's threads waiting on a FUSE answer (any mount's), by name.
fn own_threads_waiting() -> Vec<String> {
    let Ok(tasks) = std::fs::read_dir("/proc/self/task") else {
        return Vec::new();
    };
    tasks
        .flatten()
        .filter(|t| {
            std::fs::read_to_string(t.path().join("wchan"))
                .is_ok_and(|w| w.trim() == "request_wait_answer")
        })
        .map(|t| {
            let name = std::fs::read_to_string(t.path().join("comm")).unwrap_or_default();
            format!("{}({})", name.trim(), t.file_name().to_string_lossy())
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_wait_is_said_after_a_while_and_not_every_time() {
        let t0 = Instant::now();
        let at = |s: u64| t0 + Duration::from_secs(s);
        let mut w = Watch::default();
        assert_eq!(w.see(0, at(0)), None);
        assert_eq!(w.see(3, at(5)), None);
        assert_eq!(w.see(3, at(20)), None, "not yet");
        assert_eq!(w.see(3, at(35)), Some(Duration::from_secs(30)));
        assert_eq!(w.see(4, at(40)), None, "said already");
        assert_eq!(w.see(4, at(336)), Some(Duration::from_secs(331)), "again");
        assert_eq!(w.see(0, at(340)), None, "answered: starts over");
        assert_eq!(w.see(1, at(345)), None);
    }

    #[test]
    fn a_mount_s_connection_is_its_device_minor() {
        let info = "\
22 1 259:2 / / rw - btrfs /dev/nvme0n1p2 rw
40 22 0:226 / /home/cody/Drive rw,nosuid shared:9 - fuse.drive drive:codywright rw
41 22 0:77 / /tmp/a\\040b rw - fuse.drive drive:dev rw
";
        assert_eq!(minor_of(info, Path::new("/home/cody/Drive")), Some(226));
        assert_eq!(minor_of(info, Path::new("/tmp/a b")), Some(77));
        assert_eq!(minor_of(info, Path::new("/")), None, "not FUSE");
    }

    #[test]
    fn no_thread_of_a_healthy_process_waits_on_fuse() {
        assert!(own_threads_waiting().is_empty());
    }
}

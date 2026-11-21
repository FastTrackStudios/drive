//! Running dodeca's `ddc`.

use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use regex::Regex;

/// Where `ddc` is: `$DRIVE_DDC`, `~/.local/bin/ddc`, `~/.cargo/bin/ddc`,
/// or the first on `PATH`.
pub fn locate() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("DRIVE_DDC").map(PathBuf::from)
        && p.is_file()
    {
        return Some(p);
    }
    let home = std::env::var_os("HOME").map(PathBuf::from);
    for rel in [".local/bin/ddc", ".cargo/bin/ddc"] {
        if let Some(p) = home.as_ref().map(|h| h.join(rel))
            && p.is_file()
        {
            return Some(p);
        }
    }
    std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths)
            .map(|d| d.join("ddc"))
            .find(|p| p.is_file())
    })
}

/// `ddc --version` (`ddc 0.17.0`).
pub async fn version(bin: &Path) -> String {
    match tokio::process::Command::new(bin)
        .arg("--version")
        .output()
        .await
    {
        Ok(o) => String::from_utf8_lossy(&o.stdout)
            .lines()
            .next()
            .unwrap_or_default()
            .trim()
            .to_string(),
        Err(_) => String::new(),
    }
}

/// What a build said.
#[derive(Debug, Default)]
pub struct Report {
    pub ok: bool,
    /// HTML pages written.
    pub pages: i64,
    /// `/target/ (from /page/)`.
    pub broken: Vec<String>,
    /// Its output, colours removed.
    pub log: String,
}

static ANSI: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\x1b\[[0-9;?]*[A-Za-z]").expect("a regex"));
static BROKEN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^\s*\[(int|ext)\]\s+(\S+)\s+->\s+(.*?)(?:\(from\s+(\S+)\))?\s*$").expect("a regex")
});
static WROTE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"Wrote\s+(\d+)\s+HTML").expect("a regex"));

/// Read what `ddc build` printed.
pub fn parse(log: &str, ok: bool) -> Report {
    let log = ANSI.replace_all(log, "").into_owned();
    let mut r = Report {
        ok,
        ..Report::default()
    };
    for line in log.lines() {
        if let Some(c) = BROKEN.captures(line) {
            let from = c
                .get(4)
                .map_or(String::new(), |m| format!(" (from {})", m.as_str()));
            r.broken.push(format!("{}{from}", &c[2]));
        } else if let Some(c) = WROTE.captures(line) {
            r.pages = c[1].parse().unwrap_or(0);
        }
    }
    r.broken.sort();
    r.broken.dedup();
    r.log = log;
    r
}

/// `ddc build` in a project (its output in `public/`, made afresh;
/// internal links checked, the network left alone).
pub async fn build(bin: &Path, project: &Path) -> Report {
    let _ = tokio::fs::remove_dir_all(project.join("public")).await;
    let out = tokio::process::Command::new(bin)
        .arg("build")
        .arg("--no-tui")
        .arg("--link-check")
        .arg("internal")
        .arg(project)
        .current_dir(project)
        .env("NO_COLOR", "1")
        .stdin(std::process::Stdio::null())
        .output()
        .await;
    match out {
        Ok(o) => {
            let mut log = String::from_utf8_lossy(&o.stdout).into_owned();
            log.push_str(&String::from_utf8_lossy(&o.stderr));
            parse(&log, o.status.success())
        }
        Err(e) => Report {
            ok: false,
            log: format!("ddc could not be run ({}): {e}", bin.display()),
            ..Report::default()
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_build_log_is_read() {
        let log = "\x1b[36mWrote\x1b[39m 3 HTML, 17 static, CSS\n\x1b[2m[int]\x1b[0m \x1b[31m/theory/nope/\x1b[39m -> target '/theory/nope' not found (from \x1b[33m/\x1b[39m)\n";
        let r = parse(log, true);
        assert_eq!(r.pages, 3);
        assert_eq!(r.broken, ["/theory/nope/ (from /)"]);
    }
}

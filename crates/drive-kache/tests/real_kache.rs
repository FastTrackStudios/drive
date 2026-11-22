//! Real kache against two agents of one build group, in one process: a tiny
//! crate built on A (kache uploads to A's endpoint), then the same crate in
//! another folder built on B with an empty store: B's kache restores it
//! from B's endpoint, which fetched it from A.
//!
//! Needs `kache` on PATH (2.x) and cargo: `cargo test -p drive-kache --test
//! real_kache -- --ignored --nocapture`. Every kache setting is the test's
//! own (store, runtime dir, config, no host layer): the owner's kache store
//! and daemon are never touched.

mod common;

use std::path::{Path, PathBuf};
use std::process::Command;

use drive_kache::Timeouts;

fn which(bin: &str) -> Option<PathBuf> {
    std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths)
            .map(|p| p.join(bin))
            .find(|p| p.is_file())
    })
}

fn write(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

/// A workspace of two crates: a library and a binary using it.
fn tiny(dir: &Path) {
    write(
        &dir.join("Cargo.toml"),
        "[workspace]\nmembers = [\"dep\", \"app\"]\nresolver = \"3\"\n",
    );
    write(
        &dir.join("dep/Cargo.toml"),
        "[package]\nname = \"tinydep\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
    );
    write(
        &dir.join("dep/src/lib.rs"),
        "pub fn answer() -> u64 { (1..=20u64).product::<u64>() % 1000 }\n",
    );
    write(
        &dir.join("app/Cargo.toml"),
        "[package]\nname = \"tinyapp\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[dependencies]\ntinydep = { path = \"../dep\" }\n",
    );
    write(
        &dir.join("app/src/main.rs"),
        "fn main() { std::process::exit((tinydep::answer() % 2) as i32) }\n",
    );
}

/// kache, set up as the device `name` would be: its own store, runtime
/// dir and config pointing at its agent's endpoint.
fn kache_env(cmd: &mut Command, t: &Path, name: &str, port: u16, kache: &Path) {
    let config = t.join(format!("{name}-kache.toml"));
    write(
        &config,
        &format!(
            "[cache.remote]\ntype = \"s3\"\nbucket = \"kache\"\nendpoint = \"http://127.0.0.1:{port}\"\nregion = \"us-east-1\"\n"
        ),
    );
    for (k, _) in std::env::vars() {
        if k.starts_with("KACHE_") || k.starts_with("AWS_") {
            cmd.env_remove(k);
        }
    }
    cmd.env("RUSTC_WRAPPER", kache)
        .env("CARGO_BUILD_RUSTC_WRAPPER", kache)
        .env("KACHE_HOST_CONFIG", "")
        .env("KACHE_CONFIG", &config)
        .env("KACHE_CACHE_DIR", t.join(format!("{name}-store")))
        .env("KACHE_RUNTIME_DIR", t.join(format!("{name}-run")))
        .env("KACHE_S3_ACCESS_KEY", "drive")
        .env("KACHE_S3_SECRET_KEY", "drive");
}

fn run(mut cmd: Command) -> String {
    let out = cmd.output().unwrap();
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(out.status.success(), "{cmd:?} failed:\n{text}");
    text
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "runs the real kache and cargo; see the module docs"]
async fn a_crate_built_on_one_device_is_a_hit_on_the_other() {
    let Some(kache) = which("kache") else {
        panic!("kache is not on PATH");
    };
    let t = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let t = t.path();
    let a = common::agent(t, "a", 1 << 30, Timeouts::default()).await;
    let b = common::agent(t, "b", 1 << 30, Timeouts::default()).await;
    common::group(&a, &b);
    let (proj_a, proj_b) = (t.join("on-a/tiny"), t.join("on-b/tiny"));
    tiny(&proj_a);
    tiny(&proj_b);

    // A builds; its daemon uploads to A's endpoint, drained by a stop.
    let build = |proj: &Path, name: &str, port: u16| {
        let mut c = Command::new("cargo");
        c.arg("build").current_dir(proj);
        kache_env(&mut c, t, name, port, &kache);
        c
    };
    let stop = |name: &str, port: u16| {
        let mut c = Command::new(&kache);
        c.args(["daemon", "stop"]);
        kache_env(&mut c, t, name, port, &kache);
        c
    };
    let stats = |proj: &Path, name: &str, port: u16| {
        let mut c = Command::new(&kache);
        c.args(["stats", "--last-build"]).current_dir(proj);
        kache_env(&mut c, t, name, port, &kache);
        c
    };
    let (pa, pb) = (a.http.port(), b.http.port());
    let c = build(&proj_a, "a", pa);
    let a_build = tokio::task::spawn_blocking(move || run(c)).await.unwrap();
    let c = stop("a", pa);
    let stopped = tokio::task::spawn_blocking(move || run(c)).await.unwrap();
    let (kept, bytes) = a.kache.store.stats();
    eprintln!(
        "A built:\n{a_build}\nA's daemon: {stopped}\nA's endpoint keeps {kept} objects, {bytes} bytes"
    );
    for (k, o) in a.kache.store.list("") {
        eprintln!("  {k} ({} bytes)", o.size);
    }
    assert!(
        a.kache
            .store
            .list("")
            .iter()
            .any(|(k, _)| k.contains("/v3/packs/tinydep/")),
        "kache on A uploaded no pack for tinydep"
    );

    // B builds the same crate in another folder, from an empty store.
    let started = std::time::Instant::now();
    let c = build(&proj_b, "b", pb);
    let b_build = tokio::task::spawn_blocking(move || run(c)).await.unwrap();
    let b_ms = started.elapsed().as_millis();
    let c = stats(&proj_b, "b", pb);
    let b_stats = tokio::task::spawn_blocking(move || run(c)).await.unwrap();
    let c = stop("b", pb);
    let _ = tokio::task::spawn_blocking(move || run(c)).await;
    let fetched = b.kache.store.list("");
    eprintln!(
        "B built in {b_ms} ms:\n{b_build}\nB's stats:\n{b_stats}\nB's endpoint fetched from A:"
    );
    for (k, o) in &fetched {
        eprintln!("  {k} ({} bytes)", o.size);
    }
    assert!(
        fetched
            .iter()
            .any(|(k, _)| k.contains("/v3/packs/tinydep/")),
        "B's endpoint never fetched tinydep's pack from A"
    );
    assert!(
        b_stats.contains("2 of 2 crates from cache, 0 compiled"),
        "B compiled what A had built"
    );
}

//! The MCP server against a node in this process and a device over a
//! scratch folder: the protocol, the tools' schemas, paging, and the
//! safety lines (nothing deleted or overwritten, no rule switched on or
//! live, a plan's destructive operations refused, the write cap).

use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

use drive_files::space;
use drive_files::tree;
use drive_rules::{Mode, doc};
use serde_json::{Value, json};

use super::ctx::Ctx;
use super::{Options, Server, run_io, tools};
use crate::agent::{Agent, Config};

async fn node() -> (String, tempfile::TempDir) {
    let data = tempfile::tempdir().unwrap();
    let opts = drive_node::Options {
        no_backfill: true,
        no_iroh: true,
        ..Default::default()
    };
    let app = drive_node::router(data.path(), Vec::new(), opts).await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .await
        .unwrap()
    });
    (format!("http://{addr}"), data)
}

/// A scratch "disk": `Songs/` (120 small files, two of them the same),
/// `Mixes/mix.wav`, `Mixes/mix copy.wav` (the same bytes).
fn scratch(root: &Path) {
    std::fs::create_dir_all(root.join("Songs")).unwrap();
    std::fs::create_dir_all(root.join("Mixes")).unwrap();
    for i in 0..120 {
        std::fs::write(root.join(format!("Songs/s{i:03}.txt")), format!("song {i}")).unwrap();
    }
    std::fs::write(root.join("Songs/dup-a.txt"), "the same words, twice").unwrap();
    std::fs::write(root.join("Songs/dup-b.txt"), "the same words, twice").unwrap();
    let wav: Vec<u8> = (0..300_000u32).map(|i| (i % 251) as u8).collect();
    std::fs::write(root.join("Mixes/mix.wav"), &wav).unwrap();
    std::fs::write(root.join("Mixes/mix copy.wav"), &wav).unwrap();
    // Written an hour ago: rules leave files modified in the last minute.
    let then = std::time::SystemTime::now() - Duration::from_secs(3600);
    for f in ["Mixes/mix.wav", "Mixes/mix copy.wav"] {
        std::fs::File::options()
            .append(true)
            .open(root.join(f))
            .unwrap()
            .set_modified(then)
            .unwrap();
    }
}

/// Every file under a folder: path → (size, BLAKE3).
fn snapshot(root: &Path) -> BTreeMap<String, (u64, String)> {
    let mut out = BTreeMap::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d).unwrap().flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else {
                let b = std::fs::read(&p).unwrap();
                out.insert(
                    p.strip_prefix(root).unwrap().display().to_string(),
                    (b.len() as u64, blake3::hash(&b).to_hex().to_string()),
                );
            }
        }
    }
    out
}

struct World {
    _node: tempfile::TempDir,
    _agent: tempfile::TempDir,
    disk: tempfile::TempDir,
    url: String,
    server: Server,
}

impl World {
    fn ctx(&self) -> &Ctx {
        self.server.ctx.get().expect("a context")
    }

    async fn call(&self, tool: &str, args: Value) -> Result<Value, String> {
        let r = self
            .server
            .handle(json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": { "name": tool, "arguments": args } }))
            .await
            .expect("an answer");
        let res = &r["result"];
        let text = res["content"][0]["text"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        if res["isError"].as_bool() == Some(true) {
            Err(text)
        } else {
            Ok(serde_json::from_str(&text).expect("JSON"))
        }
    }
}

async fn world(max_writes: usize) -> World {
    let (url, node_dir) = node().await;
    let agent_dir = tempfile::tempdir().unwrap();
    let disk = tempfile::tempdir().unwrap();
    scratch(disk.path());
    let cfg = Config {
        node: format!("{}/sync", url.replace("http", "ws")),
        space: "home".into(),
        device_id: "battleship".into(),
        device_name: "Battleship".into(),
        storages: Vec::new(),
        pins: Vec::new(),
        token: None,
        token_expires: None,
        signed_in_as: None,
        allow_evict: false,
        relay: None,
        allow_rule_moves: false,
        kache: Default::default(),
        apps: Default::default(),
    };
    cfg.save(agent_dir.path()).unwrap();
    let author = "agent: test-client on Battleship".to_string();
    let a = Agent::start_with(
        agent_dir.path(),
        cfg.clone(),
        &author,
        agent_dir.path().join("mcp/state"),
    )
    .await;
    a.drive.online().await;
    let mut d = crate::agent::this_device("battleship", "Battleship");
    d.approved_by = "created the space".into();
    space::add_device(&a.space, &d).unwrap();
    a.commit_space();
    let loc = a.add_location("Scratch", disk.path()).unwrap();
    a.index_location(&loc).await.unwrap();
    // A file in the Drive tree, its blob on the node.
    let bytes = b"a lyric sheet".to_vec();
    let hash = blake3::hash(&bytes).to_hex().to_string();
    let put = reqwest::Client::new()
        .put(format!("{url}/blobs/{hash}"))
        .body(bytes.clone())
        .send()
        .await
        .unwrap();
    assert!(put.status().is_success());
    let songs = tree::mkdir(&a.files, None, "Songs").unwrap();
    let f = tree::create_file(&a.files, Some(songs), "lyrics.txt").unwrap();
    tree::set_content(&a.files, f, bytes.len() as u64, &hash).unwrap();
    a.commit_files();
    let opts = Options {
        client: Some("test-client".into()),
        max_writes,
        page_max: 100,
        timeout: Duration::from_secs(60),
    };
    let ctx = Ctx::over(a, opts, author).await;
    World {
        _node: node_dir,
        _agent: agent_dir,
        disk,
        url,
        server: Server::with_ctx(ctx),
    }
}

// ── the protocol and the schemas ────────────────────────────────────────

#[tokio::test]
async fn speaks_mcp_over_lines() {
    let server = Server::new(Path::new("/nonexistent-drive-agent"), Options::default());
    let input = [
        json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": { "protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": { "name": "claude-code", "version": "1" } } }),
        json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }),
        json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list" }),
        json!({ "jsonrpc": "2.0", "id": 3, "method": "prompts/get", "params": { "name": "organize", "arguments": { "focus": "the Samples location" } } }),
        json!({ "jsonrpc": "2.0", "id": 4, "method": "resources/read", "params": { "uri": "drive://safety" } }),
        json!({ "jsonrpc": "2.0", "id": 5, "method": "tools/call", "params": { "name": "devices", "arguments": {} } }),
        json!({ "jsonrpc": "2.0", "id": 6, "method": "no/such" }),
    ]
    .iter()
    .map(|m| m.to_string())
    .collect::<Vec<_>>()
    .join("\n")
        + "\nnot json\n";
    let mut out = Vec::new();
    run_io(
        &server,
        tokio::io::BufReader::new(input.as_bytes()),
        &mut out,
    )
    .await
    .unwrap();
    let answers: Vec<Value> = String::from_utf8(out)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    // Seven requests (the notification has no answer), and the bad line.
    assert_eq!(answers.len(), 7);
    assert_eq!(answers[0]["result"]["protocolVersion"], "2025-06-18");
    assert_eq!(answers[0]["result"]["serverInfo"]["name"], "drive");
    assert_eq!(*server.client.lock().unwrap(), "claude-code");
    assert!(answers[1]["result"]["tools"].as_array().unwrap().len() >= 20);
    let prompt = answers[2]["result"]["messages"][0]["content"]["text"]
        .as_str()
        .unwrap();
    assert!(prompt.contains("the Samples location") && prompt.contains("on and live themselves"));
    assert!(
        answers[3]["result"]["contents"][0]["text"]
            .as_str()
            .unwrap()
            .contains("Nothing deletes")
    );
    // No config in that folder: the tool says so, as a tool error.
    assert_eq!(answers[4]["result"]["isError"], true);
    assert_eq!(answers[5]["error"]["code"], -32601);
    assert_eq!(answers[6]["error"]["code"], -32700);
}

#[test]
fn tools_are_described_and_none_destroys() {
    let defs = tools::defs();
    let names: Vec<&str> = defs.iter().map(|d| d.name).collect();
    for want in [
        "devices",
        "locations",
        "browse",
        "search",
        "file_info",
        "folder_summary",
        "duplicates",
        "projects",
        "tags",
        "rules",
        "rule_preview",
        "journal",
        "graph_backlinks",
        "graph_neighbors",
        "graph_path",
        "graph_tag",
        "link",
        "unlink",
        "tag",
        "untag",
        "set_field",
        "create_tag",
        "add_to_project",
        "create_project",
        "index_folder",
        "propose_rule",
        "plan",
        "apply_plan",
    ] {
        assert!(names.contains(&want), "{want} is missing");
    }
    for d in &defs {
        // No tool deletes, trashes, or switches rules on or live.
        for bad in [
            "delete",
            "remove",
            "trash",
            "live",
            "enable",
            "rules_on",
            "overwrite",
        ] {
            assert!(!d.name.contains(bad), "{} sounds destructive", d.name);
        }
        assert!(d.description.len() > 40, "{} says too little", d.name);
        assert_eq!(d.schema["type"], "object");
        assert_eq!(d.schema["additionalProperties"], false);
        let props = d.schema["properties"].as_object().unwrap();
        for r in d.schema["required"].as_array().unwrap() {
            assert!(
                props.contains_key(r.as_str().unwrap()),
                "{} requires an unknown {r}",
                d.name
            );
        }
        // Every write tool has a dry run; no read tool needs one.
        assert_eq!(props.contains_key("dry_run"), d.write, "{}", d.name);
    }
    for t in tools::list() {
        assert_eq!(t["annotations"]["destructiveHint"], false);
    }
}

/// The MCP module calls nothing that deletes, trashes, overwrites, or
/// switches a rule on or live.
#[test]
fn nothing_here_destroys() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/mcp");
    for f in [
        "mod.rs", "ctx.rs", "read.rs", "write.rs", "plan.rs", "tools.rs",
    ] {
        let src = std::fs::read_to_string(dir.join(f)).unwrap();
        for bad in [
            "tree::trash",
            "tags::delete",
            "remove_project",
            "remove_part",
            "remove_location",
            "remove_device",
            "remove_storage",
            "doc::remove",
            "remove_file",
            "remove_dir",
            "fs::rename",
            "Mode::Live",
            "set_enabled(&ctx.e.rules, &id, true",
            "request_run(&ctx.e.rules, &r.id, true",
            "request_undo",
            "OpenOptions",
            "sidecars/purge",
        ] {
            assert!(!src.contains(bad), "{f} calls {bad}");
        }
    }
}

// ── reading ─────────────────────────────────────────────────────────────

#[tokio::test]
async fn reads_page_and_summarise() {
    let w = world(100).await;
    let d = w.call("devices", json!({})).await.unwrap();
    assert_eq!(d["devices"][0]["name"], "Battleship");
    let l = w.call("locations", json!({})).await.unwrap();
    assert_eq!(l["locations"][0]["name"], "Scratch");

    // 122 files in Songs: pages of 50, then the rest.
    let p1 = w
        .call(
            "browse",
            json!({ "location": "Scratch", "path": "Songs", "limit": 50 }),
        )
        .await
        .unwrap();
    assert_eq!(p1["total"], 122);
    assert_eq!(p1["items"].as_array().unwrap().len(), 50);
    assert_eq!(p1["next_offset"], 50);
    let p3 = w
        .call(
            "browse",
            json!({ "location": "Scratch", "path": "Songs", "offset": 100, "limit": 50 }),
        )
        .await
        .unwrap();
    assert_eq!(p3["items"].as_array().unwrap().len(), 22);
    assert_eq!(p3["next_offset"], Value::Null);
    // A limit past the session's page size is cut to it.
    let big = w
        .call(
            "browse",
            json!({ "location": "Scratch", "path": "Songs", "limit": 5000 }),
        )
        .await
        .unwrap();
    assert_eq!(big["items"].as_array().unwrap().len(), 100);
    // Unknown arguments are refused, with what it takes.
    assert!(
        w.call("browse", json!({ "folder": "x" }))
            .await
            .unwrap_err()
            .contains("takes no folder")
    );
    assert!(
        w.call("browse", json!({ "location": "Scratch", "path": "../.." }))
            .await
            .unwrap_err()
            .starts_with("refused")
    );

    let s = w
        .call("folder_summary", json!({ "location": "Scratch" }))
        .await
        .unwrap();
    assert_eq!(s["files"], 124);
    assert_eq!(s["duplicates_inside"]["groups"], 2);
    assert_eq!(s["largest"][0]["size"], 300_000);
    let dups = w
        .call("duplicates", json!({ "location": "Scratch", "limit": 1 }))
        .await
        .unwrap();
    assert_eq!(dups["total"], 2);
    assert_eq!(dups["items"][0]["wasted"], 300_000);
    assert_eq!(dups["next_offset"], 1);

    let found = w.call("search", json!({ "query": "mix" })).await.unwrap();
    assert!(found["total"].as_u64().unwrap() >= 2, "{found}");
    let info = w
        .call("file_info", json!({ "thing": "Scratch:Mixes/mix.wav" }))
        .await
        .unwrap();
    assert_eq!(info["size"], 300_000);
    assert_eq!(info["copies"].as_array().unwrap().len(), 2, "{info}");
    let info = w
        .call("file_info", json!({ "thing": "drive:Songs/lyrics.txt" }))
        .await
        .unwrap();
    assert_eq!(info["in"], "drive");
    let _ = &w.url;
}

// ── writing, safely ─────────────────────────────────────────────────────

#[tokio::test]
async fn writes_record_the_agent_and_dry_runs_change_nothing() {
    let w = world(100).await;
    let before = w
        .call("tags", json!({ "thing": "Scratch:Mixes/mix.wav" }))
        .await
        .unwrap();
    assert!(before["tags"].as_array().unwrap().is_empty());
    let dry = w
        .call("tag", json!({ "things": ["Scratch:Mixes/mix.wav"], "tags": ["Mixes"], "create_missing": true, "dry_run": true }))
        .await
        .unwrap();
    assert_eq!(dry["dry_run"], true);
    assert_eq!(w.ctx().writes_used(), 0);
    let still = w
        .call("tags", json!({ "thing": "Scratch:Mixes/mix.wav" }))
        .await
        .unwrap();
    assert!(still["tags"].as_array().unwrap().is_empty());
    assert!(
        w.call("tags", json!({})).await.unwrap()["items"]
            .as_array()
            .unwrap()
            .iter()
            .all(|t| t["name"] != "Mixes")
    );
    // A tag that does not exist is refused unless asked to make it.
    assert!(
        w.call(
            "tag",
            json!({ "things": ["Scratch:Mixes/mix.wav"], "tags": ["Mixes"] })
        )
        .await
        .is_err()
    );

    w.call(
        "tag",
        json!({ "things": ["Scratch:Mixes/mix.wav"], "tags": ["Mixes"], "create_missing": true }),
    )
    .await
    .unwrap();
    // Tagged by content: the copy carries it too, and the agent is its author.
    let copy = w
        .call("tags", json!({ "thing": "Scratch:Mixes/mix copy.wav" }))
        .await
        .unwrap();
    assert_eq!(copy["tags"][0]["tag"], "Mixes");
    assert_eq!(copy["tags"][0]["by"], "agent: test-client on Battleship");
    w.call(
        "set_field",
        json!({ "thing": "Scratch:Mixes/mix.wav", "name": "Artist", "value": "Jane" }),
    )
    .await
    .unwrap();
    assert!(w.call("set_field", json!({ "thing": "Scratch:Mixes/mix.wav", "name": "Date created", "value": "soon", "kind": "date" })).await.is_err());
    w.call("create_project", json!({ "name": "Album" }))
        .await
        .unwrap();
    assert!(
        w.call("create_project", json!({ "name": "album" }))
            .await
            .unwrap_err()
            .starts_with("refused")
    );
    w.call(
        "add_to_project",
        json!({ "project": "Album", "location": "Scratch", "path": "Mixes", "label": "Mixes" }),
    )
    .await
    .unwrap();
    let info = w
        .call("file_info", json!({ "thing": "Scratch:Mixes/mix.wav" }))
        .await
        .unwrap();
    assert_eq!(info["projects"][0], "Album");
    assert_eq!(info["fields"][0]["value"], "Jane");
    // The history's author is the agent too.
    let ctx = w.ctx();
    let changes = drive_spine::history::changes(&ctx.e.tags);
    assert!(
        changes
            .iter()
            .any(|c| c.who == "agent: test-client on Battleship"),
        "{changes:?}"
    );
}

#[tokio::test]
async fn the_write_cap_holds() {
    let w = world(3).await;
    w.call("create_tag", json!({ "name": "Keep" }))
        .await
        .unwrap();
    // Four writes do not fit in the two left, and nothing of them is done.
    let e = w
        .call("tag", json!({ "things": ["Scratch:Songs/s000.txt", "Scratch:Songs/s001.txt"], "tags": ["Keep", "Favorite"] }))
        .await
        .unwrap_err();
    assert!(
        e.starts_with("refused: this session may make 3 writes"),
        "{e}"
    );
    let on = w
        .call("tags", json!({ "thing": "Scratch:Songs/s000.txt" }))
        .await
        .unwrap();
    assert!(on["tags"].as_array().unwrap().is_empty());
    // Dry runs are free.
    w.call("tag", json!({ "things": ["Scratch:Songs/s000.txt", "Scratch:Songs/s001.txt"], "tags": ["Keep", "Favorite"], "dry_run": true })).await.unwrap();
    w.call(
        "tag",
        json!({ "things": ["Scratch:Songs/s000.txt"], "tags": ["Keep", "Favorite"] }),
    )
    .await
    .unwrap();
    assert!(
        w.call("create_tag", json!({ "name": "One more" }))
            .await
            .is_err()
    );
}

#[tokio::test]
async fn a_proposed_rule_is_off_and_a_dry_run_whatever_it_says() {
    let w = world(100).await;
    let disk_before = snapshot(w.disk.path());
    let rule = json!({
        "name": "File mixes",
        "enabled": true,
        "mode": "live",
        "scope": { "devices": { "by": "all" }, "places": [ { "kind": "location", "location": "Scratch", "path": "Mixes" } ] },
        "when": { "all": true, "groups": [ { "all": true, "conditions": [ { "test": { "test": "extension", "any": ["wav"] } } ] } ] },
        "actions": [ { "do": "move", "to": { "kind": "location", "location": "Scratch", "path": "Archive" }, "name": "" } ]
    });
    let v = w
        .call("propose_rule", json!({ "rule": rule, "wait_seconds": 5 }))
        .await
        .unwrap();
    assert_eq!(v["created"]["on"], false);
    assert_eq!(v["created"]["mode"], "dry-run");
    let ctx = w.ctx();
    let saved = doc::find(&ctx.e.rules, "File mixes").unwrap();
    assert!(!saved.enabled);
    assert_eq!(saved.mode, Mode::DryRun);
    assert_eq!(saved.by, "agent: test-client on Battleship");
    // Its dry run ran here: two files, the moves held back (moves are off
    // on this device), and the run asked of everyone was not live.
    let here = v["preview"]["previews"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["device"] == "Battleship")
        .cloned()
        .expect("a preview here");
    assert_eq!(here["matched"], 2, "{here}");
    assert!(
        here["items"][0]["steps"][0]["held_back"].is_string(),
        "{here}"
    );
    assert!(!doc::request(&ctx.e.rules, &saved.id).unwrap().live);
    // Nothing on disk changed.
    assert_eq!(snapshot(w.disk.path()), disk_before);

    // A rule that trashes is refused; so is one of a taken name.
    let trash = json!({ "name": "Tidy", "actions": [ { "do": "trash" } ] });
    assert!(
        w.call("propose_rule", json!({ "rule": trash }))
            .await
            .unwrap_err()
            .starts_with("refused")
    );
    assert!(w.call("propose_rule", json!({ "rule": { "name": "File mixes", "actions": [ { "do": "add-tags", "tags": ["Favorite"] } ] } })).await.unwrap_err().starts_with("refused"));
    // No tool turns it on or live.
    for t in ["rules_on", "rule_live", "set_rule_mode", "enable_rule"] {
        assert!(
            w.call(t, json!({ "rule": "File mixes" }))
                .await
                .unwrap_err()
                .starts_with("no tool")
        );
    }
    // rule_preview asks again, still not live.
    w.call(
        "rule_preview",
        json!({ "rule": "File mixes", "wait_seconds": 1 }),
    )
    .await
    .unwrap();
    let saved = doc::find(&ctx.e.rules, "File mixes").unwrap();
    assert!(!saved.enabled && saved.mode == Mode::DryRun);
    assert!(!doc::request(&ctx.e.rules, &saved.id).unwrap().live);
}

#[tokio::test]
async fn a_plan_refuses_what_destroys_and_turns_device_moves_into_rules() {
    let w = world(100).await;
    let disk_before = snapshot(w.disk.path());
    let ctx = w.ctx();
    let drive_before = tree::files(&ctx.a.files).len();
    // A file of that name already where the copy goes.
    let songs = tree::resolve(&ctx.a.files, "Songs").unwrap().unwrap();
    let lyric = tree::resolve(&ctx.a.files, "Songs/lyrics.txt")
        .unwrap()
        .unwrap();
    let p = w
        .call("plan", json!({ "title": "Tidy", "operations": [
            { "op": "delete", "thing": "Scratch:Mixes/mix copy.wav" },
            { "op": "trash", "thing": "drive:Songs/lyrics.txt" },
            { "op": "copy", "from": "drive:Songs/lyrics.txt", "to": "drive:Songs", "overwrite": true },
            { "op": "empty_trash" },
            { "op": "shred", "thing": "x" },
            { "op": "move", "from": "drive:Songs/lyrics.txt", "to": "drive:Trash" },
            { "op": "copy", "from": "drive:Songs/lyrics.txt", "to": "drive:Songs" },
            { "op": "move", "from": "drive:Songs/lyrics.txt", "to": "drive:Archive/2026" },
            { "op": "tag", "thing": "Scratch:Mixes/mix.wav", "tags": ["Favorite"] },
            { "op": "move", "from": "Scratch:Mixes/mix copy.wav", "to": "Scratch:Archive" },
            { "op": "rename", "from": "Scratch:Mixes/mix.wav", "name": "2026 mix.wav" },
            { "op": "move", "from": "Scratch:Mixes/mix.wav", "to": "drive:Mixes" }
        ] }))
        .await
        .unwrap();
    let outcomes: Vec<&str> = p["operations"]
        .as_array()
        .unwrap()
        .iter()
        .map(|o| o["outcome"].as_str().unwrap())
        .collect();
    assert_eq!(
        outcomes,
        [
            "refused", "refused", "refused", "refused", "refused", "refused", "apply", "apply",
            "apply", "rule", "rule", "refused"
        ],
        "{p}"
    );
    assert_eq!(p["proposed_rules"].as_array().unwrap().len(), 2);
    // Planning changed nothing.
    assert_eq!(w.ctx().writes_used(), 0);
    assert_eq!(tree::files(&ctx.a.files).len(), drive_before);

    let id = p["plan_id"].as_str().unwrap();
    let dry = w
        .call("apply_plan", json!({ "plan_id": id, "dry_run": true }))
        .await
        .unwrap();
    assert_eq!(dry["dry_run"], true);
    assert_eq!(tree::files(&ctx.a.files).len(), drive_before);

    let done = w
        .call("apply_plan", json!({ "plan_id": id }))
        .await
        .unwrap();
    assert!(
        done["results"]
            .as_array()
            .unwrap()
            .iter()
            .all(|r| r.get("error").is_none()),
        "{done}"
    );
    // The copy took a free name beside the original, which then moved:
    // one more file, none gone.
    let files: Vec<String> = tree::files(&ctx.a.files)
        .into_iter()
        .map(|(p, _)| p)
        .collect();
    assert_eq!(files.len(), drive_before + 1, "{files:?}");
    assert!(
        files.contains(&"Songs/lyrics (2).txt".to_string()),
        "{files:?}"
    );
    assert!(
        files.contains(&"Archive/2026/lyrics.txt".to_string()),
        "{files:?}"
    );
    assert_eq!(
        tree::path_of(&ctx.a.files, lyric.id),
        "Archive/2026/lyrics.txt"
    );
    assert!(tree::entry(&ctx.a.files, songs.id).is_some());
    // The device's operations are rules, off and dry runs.
    for r in done["proposed_rules"].as_array().unwrap() {
        let rule = doc::rule(&ctx.e.rules, r["id"].as_str().unwrap()).unwrap();
        assert!(!rule.enabled && rule.mode == Mode::DryRun, "{rule:?}");
        assert!(!rule.trigger.on_change);
    }
    // Nothing on the computer's disk changed.
    assert_eq!(snapshot(w.disk.path()), disk_before);
    // A plan is applied once.
    assert!(
        w.call("apply_plan", json!({ "plan_id": id }))
            .await
            .unwrap_err()
            .starts_with("refused")
    );
}

#[tokio::test]
async fn a_local_file_is_never_the_agents_own_or_the_systems() {
    let w = world(10).await;
    let agent = w._agent.path();
    for name in [
        agent.join("config.json"),
        agent.join(".").join("config.json"),
        w.disk
            .path()
            .join("..")
            .join(agent.file_name().unwrap())
            .join("config.json"),
        std::path::PathBuf::from("/proc/self/status"),
    ] {
        let name = name.to_string_lossy().into_owned();
        assert!(w.ctx().thing(&name).await.is_err(), "{name} was probed");
    }
    // A symlink to it from elsewhere.
    let link = w.disk.path().join("cfg");
    std::os::unix::fs::symlink(agent.join("config.json"), &link).unwrap();
    assert!(w.ctx().thing(&link.to_string_lossy()).await.is_err());
    // An ordinary file still is one.
    let mix = w.disk.path().join("Mixes/mix.wav");
    assert!(w.ctx().thing(&mix.to_string_lossy()).await.is_ok());
}

// ── the graph ───────────────────────────────────────────────────────────

/// A note in the Drive tree, its blob on the node.
async fn note(w: &World, dir: loro::TreeID, name: &str, text: &str) {
    let bytes = text.as_bytes().to_vec();
    let hash = blake3::hash(&bytes).to_hex().to_string();
    let put = reqwest::Client::new()
        .put(format!("{}/blobs/{hash}", w.url))
        .body(bytes.clone())
        .send()
        .await
        .unwrap();
    assert!(put.status().is_success());
    let a = &w.ctx().a;
    let f = tree::create_file(&a.files, Some(dir), name).unwrap();
    tree::set_content(&a.files, f, bytes.len() as u64, &hash).unwrap();
    a.commit_files();
}

#[tokio::test]
async fn the_graph_tools_read_this_peers_graph() {
    let w = world(10).await;
    let notes = tree::mkdir(&w.ctx().a.files, None, "Notes").unwrap();
    note(&w, notes, "Scales.md", "---\naliases: [Modes]\ntags: [theory]\n---\nSee [[Chords]] and [[Nowhere]].\n").await;
    note(&w, notes, "Chords.md", "Built from [[Modes|scales]].\n").await;

    let back = w
        .call("graph_backlinks", json!({ "thing": "drive:Notes/Scales.md" }))
        .await
        .unwrap();
    assert_eq!(back["total"], 1, "{back}");
    assert_eq!(back["items"][0]["thing"], "drive:Notes/Chords.md");
    assert_eq!(back["items"][0]["edge"]["written"], "[[Modes|scales]]");
    assert_eq!(back["coverage"]["out_of_reach"], 0);

    // A name the graph knows, and a page not written yet.
    let missing = w
        .call("graph_backlinks", json!({ "thing": "missing:nowhere" }))
        .await
        .unwrap();
    assert_eq!(missing["items"][0]["thing"], "drive:Notes/Scales.md");
    let near = w
        .call("graph_neighbors", json!({ "thing": "Modes", "depth": 1 }))
        .await
        .unwrap();
    assert_eq!(near["total"], 4, "Chords, the missing page, the tag and its folder: {near}");

    let path = w
        .call("graph_path", json!({ "from": "drive:Notes/Chords.md", "to": "missing:nowhere" }))
        .await
        .unwrap();
    assert_eq!(path["found"], true);
    assert_eq!(path["steps"].as_array().unwrap().len(), 3);

    let tagged = w.call("graph_tag", json!({ "tag": "theory" })).await.unwrap();
    assert_eq!(tagged["items"][0]["thing"], "drive:Notes/Scales.md");
    assert_eq!(tagged["tag"], "label:theory", "a frontmatter tag no Drive tag has");

    // Read only: nothing was written to the space's documents.
    assert_eq!(w.ctx().writes_used(), 0);

    // An app's link: dry run first, then made, then seen by the graph.
    let args = json!({ "from": "doc:season:episode:1", "to": "drive:Notes/Scales.md", "kind": "uses" });
    let mut dry_args = args.clone();
    dry_args["dry_run"] = json!(true);
    assert_eq!(w.call("link", dry_args).await.unwrap()["dry_run"], true);
    assert_eq!(w.ctx().writes_used(), 0);
    w.call("link", args.clone()).await.unwrap();
    assert_eq!(w.ctx().writes_used(), 1);
    let back = w
        .call("graph_backlinks", json!({ "thing": "drive:Notes/Scales.md" }))
        .await
        .unwrap();
    let linked: Vec<&Value> = back["items"].as_array().unwrap().iter().filter(|h| h["edge"]["kind"] == "linked").collect();
    assert_eq!(linked.len(), 1, "{back}");
    assert_eq!(linked[0]["id"], "doc:season:episode:1");
    assert_eq!(linked[0]["edge"]["written"], "uses");
    w.call("link", args.clone()).await.unwrap();
    assert_eq!(w.ctx().writes_used(), 1, "linking again writes nothing");
    w.call("unlink", args).await.unwrap();
    let back = w
        .call("graph_backlinks", json!({ "thing": "drive:Notes/Scales.md" }))
        .await
        .unwrap();
    assert!(!back["items"].as_array().unwrap().iter().any(|h| h["edge"]["kind"] == "linked"));
}

#[tokio::test]
async fn a_computers_folder_is_tagged_and_its_tags_follow_a_move() {
    let w = world(10).await;
    w.call(
        "tag",
        json!({ "things": ["Scratch:Mixes"], "tags": ["Bounces"], "create_missing": true }),
    )
    .await
    .unwrap();
    w.call("link", json!({ "from": "doc:season:episode:1", "to": "Scratch:Mixes", "kind": "uses" }))
        .await
        .unwrap();
    let on = w.call("tags", json!({ "thing": "Scratch:Mixes" })).await.unwrap();
    assert_eq!(on["tags"][0]["tag"], "Bounces", "{on}");

    // Renamed on disk, then walked again by the device that has it.
    std::fs::rename(w.disk.path().join("Mixes"), w.disk.path().join("Mixes 2026")).unwrap();
    let a = &w.ctx().a;
    let loc = space::locations(&a.space)
        .into_iter()
        .find(|l| l.name == "Scratch")
        .unwrap();
    a.index_location(&loc).await.unwrap();

    let moved = w.call("tags", json!({ "thing": "Scratch:Mixes 2026" })).await.unwrap();
    assert_eq!(moved["tags"][0]["tag"], "Bounces", "the tag went with it: {moved}");
    let back = w
        .call("graph_backlinks", json!({ "thing": "Scratch:Mixes 2026" }))
        .await
        .unwrap();
    let kinds: Vec<&str> = back["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|h| h["edge"]["kind"].as_str().unwrap())
        .collect();
    assert!(kinds.contains(&"linked"), "the link went with it too: {back}");
    let tagged = w.call("graph_tag", json!({ "tag": "Bounces" })).await.unwrap();
    assert_eq!(tagged["total"], 1);
}

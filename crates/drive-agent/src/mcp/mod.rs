//! `drive mcp`: Drive as an MCP server (Model Context Protocol, over
//! stdio), so an AI agent (Claude Code, any MCP client) can see the
//! owner's files on every computer and organise them, safely
//! (docs/agents.md, #43).
//!
//! It runs as this device: it opens the space's documents (registry, tags,
//! rules, journal) through the node as the CLI does, and asks the node's
//! HTTP API (search, copies, browse, index) with this device's session.
//! Every edit records `agent: <client> on <device>` as its author.
//!
//! The safety model, held by the tools and their tests (`tests.rs`):
//!
//! - nothing deletes, trashes or overwrites, ever: no tool does, and a
//!   plan's destructive operations are refused;
//! - a rule an agent proposes is always off and a dry run; no tool switches
//!   a rule on or live (a person does, in the app or the CLI), and moves of
//!   a computer's files stay gated by `allow_rule_moves` on that device;
//! - every write tool takes `dry_run`; a session has a cap on writes; reads
//!   are paged and every call has a timeout.
//!
//! The protocol is the small part of MCP this needs (initialize, tools,
//! prompts, resources, ping), JSON-RPC 2.0 one message per line. Not
//! `rmcp`: its 3.x API derives schemas with schemars and routes through
//! macros, while these tools' schemas are written by hand and every call
//! goes through one place that applies the limits; the subset is ~200
//! lines and tested end to end here.

mod ctx;
mod plan;
mod read;
mod tools;
mod write;

#[cfg(test)]
mod tests;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt};

pub use ctx::Ctx;

/// The protocol versions this server speaks, newest first.
const VERSIONS: &[&str] = &["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];

/// The session's limits.
#[derive(Clone, Debug)]
pub struct Options {
    /// The client's name as it appears in history (`agent: NAME on
    /// DEVICE`); none: the name the client gives in `initialize`.
    pub client: Option<String>,
    /// Write operations this session may make (each tag put on each thing,
    /// each field, project, location, rule and plan operation is one).
    pub max_writes: usize,
    /// The most items one page of any listing holds.
    pub page_max: usize,
    /// How long one tool call may take.
    pub timeout: Duration,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            client: None,
            max_writes: 500,
            page_max: 500,
            timeout: Duration::from_secs(90),
        }
    }
}

impl Options {
    /// From the command line: `--as NAME --max-writes N --page-max N
    /// --timeout SECONDS`.
    pub fn from_args(args: &[String]) -> Result<Options, String> {
        let mut o = Options::default();
        let num = |name: &str| -> Result<Option<u64>, String> {
            crate::flag(args, name)
                .map(|v| {
                    v.parse::<u64>()
                        .map_err(|_| format!("{name} takes a number"))
                })
                .transpose()
        };
        o.client = crate::flag(args, "--as");
        if let Some(n) = num("--max-writes")? {
            o.max_writes = n as usize;
        }
        if let Some(n) = num("--page-max")? {
            o.page_max = (n as usize).clamp(1, 2000);
        }
        if let Some(n) = num("--timeout")? {
            o.timeout = Duration::from_secs(n.clamp(5, 600));
        }
        Ok(o)
    }
}

/// One MCP session: the client's name, and the context opened on the
/// first tool call (so `initialize` answers at once, offline or not).
pub struct Server {
    dir: Option<PathBuf>,
    opts: Options,
    client: std::sync::Mutex<String>,
    ctx: tokio::sync::OnceCell<Arc<Ctx>>,
}

impl Server {
    pub fn new(dir: &Path, opts: Options) -> Server {
        Server {
            dir: Some(dir.to_path_buf()),
            client: std::sync::Mutex::new(opts.client.clone().unwrap_or_else(|| "mcp".into())),
            opts,
            ctx: tokio::sync::OnceCell::new(),
        }
    }

    /// A server over a context already open (tests).
    #[cfg(test)]
    pub fn with_ctx(ctx: Ctx) -> Server {
        let opts = ctx.opts.clone();
        let cell = tokio::sync::OnceCell::new();
        let _ = cell.set(Arc::new(ctx));
        Server {
            dir: None,
            client: std::sync::Mutex::new("test".into()),
            opts,
            ctx: cell,
        }
    }

    async fn ctx(&self) -> Result<Arc<Ctx>, String> {
        self.ctx
            .get_or_try_init(|| async {
                let dir = self.dir.as_ref().ok_or("no agent folder")?;
                let client = self.client.lock().expect("client").clone();
                Ctx::open(dir, self.opts.clone(), &client)
                    .await
                    .map(Arc::new)
            })
            .await
            .cloned()
    }

    /// Answer one message; `None` for notifications.
    pub async fn handle(&self, msg: Value) -> Option<Value> {
        let id = msg.get("id").cloned();
        let method = msg["method"].as_str().unwrap_or_default().to_string();
        let params = msg.get("params").cloned().unwrap_or(Value::Null);
        // A notification (or a response to us): nothing to answer.
        let id = id?;
        let result = match method.as_str() {
            "initialize" => Ok(self.initialize(&params)),
            "ping" => Ok(json!({})),
            "tools/list" => Ok(json!({ "tools": tools::list() })),
            "tools/call" => Ok(self.call(&params).await),
            "prompts/list" => Ok(json!({ "prompts": [tools::organize_prompt_info()] })),
            "prompts/get" => tools::organize_prompt(&params),
            "resources/list" => Ok(json!({ "resources": tools::resources() })),
            "resources/read" => tools::read_resource(&params),
            "" => Err((-32600, "not a request".to_string())),
            other => Err((-32601, format!("no method {other}"))),
        };
        Some(match result {
            Ok(r) => json!({ "jsonrpc": "2.0", "id": id, "result": r }),
            Err((code, message)) => {
                json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
            }
        })
    }

    fn initialize(&self, params: &Value) -> Value {
        if self.opts.client.is_none()
            && let Some(name) = params["clientInfo"]["name"]
                .as_str()
                .filter(|n| !n.trim().is_empty())
        {
            *self.client.lock().expect("client") = name.trim().chars().take(60).collect();
        }
        let asked = params["protocolVersion"].as_str().unwrap_or_default();
        let version = VERSIONS
            .iter()
            .find(|v| **v == asked)
            .copied()
            .unwrap_or(VERSIONS[1]);
        json!({
            "protocolVersion": version,
            "capabilities": {
                "tools": { "listChanged": false },
                "prompts": { "listChanged": false },
                "resources": { "listChanged": false, "subscribe": false }
            },
            "serverInfo": { "name": "drive", "title": "Drive", "version": env!("CARGO_PKG_VERSION") },
            "instructions": tools::INSTRUCTIONS
        })
    }

    /// `tools/call`: the tool's answer as text (compact JSON), or its error
    /// with `isError` (the model reads it and can correct itself).
    async fn call(&self, params: &Value) -> Value {
        let name = params["name"].as_str().unwrap_or_default().to_string();
        let args = match params.get("arguments") {
            None | Some(Value::Null) => json!({}),
            Some(v @ Value::Object(_)) => v.clone(),
            Some(_) => return error_result("arguments must be an object"),
        };
        let started = Instant::now();
        let span = tracing::info_span!(
            "mcp.tool",
            mcp.tool = %name,
            mcp.client = tracing::field::Empty,
            mcp.dry_run = args["dry_run"].as_bool().unwrap_or(false),
            mcp.outcome = tracing::field::Empty,
            mcp.writes = tracing::field::Empty,
            mcp.ms = tracing::field::Empty,
        );
        let _enter = span.enter();
        span.record("mcp.client", self.client.lock().expect("client").as_str());
        let Some(tool) = tools::find(&name) else {
            span.record("mcp.outcome", "unknown");
            return error_result(&format!("no tool {name}; tools/list names them"));
        };
        let answer = match self.ctx().await {
            Err(e) => Err(format!("Drive is not set up on this computer: {e}")),
            Ok(ctx) => {
                match tokio::time::timeout(self.opts.timeout, tools::run(&ctx, tool, &args)).await {
                    Ok(r) => {
                        span.record("mcp.writes", ctx.writes_used());
                        r
                    }
                    Err(_) => Err(format!(
                        "{name} took longer than {} s and was stopped",
                        self.opts.timeout.as_secs()
                    )),
                }
            }
        };
        span.record("mcp.ms", started.elapsed().as_millis() as u64);
        match answer {
            Ok(v) => {
                span.record("mcp.outcome", "ok");
                tracing::info!("tool call");
                json!({
                    "content": [{ "type": "text", "text": serde_json::to_string(&v).unwrap_or_default() }],
                    "isError": false
                })
            }
            Err(e) => {
                let refused = e.starts_with("refused");
                span.record("mcp.outcome", if refused { "refused" } else { "error" });
                if refused {
                    tracing::warn!(mcp.why = %e, "a tool call was refused");
                } else {
                    tracing::info!(mcp.error = %e, "tool call");
                }
                error_result(&e)
            }
        }
    }
}

fn error_result(text: &str) -> Value {
    json!({ "content": [{ "type": "text", "text": text }], "isError": true })
}

/// Serve one client: a JSON-RPC message per line in, one per line out.
pub async fn run_io<R, W>(server: &Server, reader: R, mut writer: W) -> Result<(), String>
where
    R: AsyncBufRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut lines = reader.lines();
    while let Some(line) = lines.next_line().await.map_err(|e| e.to_string())? {
        if line.trim().is_empty() {
            continue;
        }
        let answers: Vec<Value> = match serde_json::from_str::<Value>(&line) {
            Err(e) => vec![json!({
                "jsonrpc": "2.0", "id": null,
                "error": { "code": -32700, "message": format!("not JSON: {e}") }
            })],
            Ok(Value::Array(batch)) => {
                let mut out = Vec::new();
                for m in batch {
                    out.extend(server.handle(m).await);
                }
                out
            }
            Ok(m) => server.handle(m).await.into_iter().collect(),
        };
        for a in answers {
            let mut text = serde_json::to_string(&a).map_err(|e| e.to_string())?;
            text.push('\n');
            writer
                .write_all(text.as_bytes())
                .await
                .map_err(|e| e.to_string())?;
            writer.flush().await.map_err(|e| e.to_string())?;
        }
    }
    Ok(())
}

/// `drive mcp`: serve on stdin and stdout until the client leaves.
pub async fn serve(dir: &Path, args: &[String]) -> Result<(), String> {
    let opts = Options::from_args(args)?;
    crate::agent::Config::load(dir)?;
    tracing::info!(mcp.max_writes = opts.max_writes, "MCP server on stdio");
    let server = Server::new(dir, opts);
    run_io(
        &server,
        tokio::io::BufReader::new(tokio::io::stdin()),
        tokio::io::stdout(),
    )
    .await
}

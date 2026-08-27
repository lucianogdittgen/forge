//! Forge's MCP server: how the agent reaches the process manager.
//!
//! The agent is given no built-in tools at all (see ADR-0002). Everything it
//! can do arrives through this endpoint, and everything this endpoint offers is
//! a projection of the same [`ProcessManager`] the terminal pane renders. That
//! is the mechanism behind the product's one hard rule: there is no way for the
//! agent to run a command the developer cannot watch, because there is only one
//! place commands are run.
//!
//! Transport is MCP streamable HTTP on loopback. Two things guard it:
//!
//! - it binds `127.0.0.1` on an ephemeral port, so it is not reachable off-box;
//! - the URL carries an unguessable token, so other local processes — every
//!   user account on a shared build machine included — cannot drive Forge's
//!   process manager by guessing a port number.
//!
//! The token is *not* the permission gate. It stops unrelated processes; the
//! gate that decides whether a call may proceed lives in `forge-agent`, and
//! runs on every call that reaches this server.

pub mod http;
pub mod tools;

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use forge_process::ProcessManager;
use serde_json::{json, Value};
use tokio::io::BufReader;
use tokio::net::TcpListener;

/// The MCP protocol revision Forge implements.
const PROTOCOL_VERSION: &str = "2025-06-18";

/// A running MCP endpoint.
pub struct McpServer {
    /// The full URL to hand the agent, token included.
    pub url: String,
    pub port: u16,
    task: tokio::task::JoinHandle<()>,
}

impl McpServer {
    /// Bind and start serving. Returns once the socket is listening, so the
    /// agent can be spawned immediately afterwards without a race.
    pub async fn start(pm: ProcessManager, cwd: PathBuf) -> Result<Self> {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await?;
        let port = listener.local_addr()?.port();
        let token = token();
        let path = Arc::new(format!("/{token}/mcp"));
        let url = format!("http://127.0.0.1:{port}{path}");

        let cwd = Arc::new(cwd);
        let task = tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    break;
                };
                let (pm, path, cwd) = (pm.clone(), path.clone(), cwd.clone());
                tokio::spawn(async move {
                    if let Err(e) = serve_connection(stream, pm, path, cwd).await {
                        tracing::debug!(error = %e, "mcp connection ended");
                    }
                });
            }
        });

        Ok(Self { url, port, task })
    }
}

impl Drop for McpServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn serve_connection(
    stream: tokio::net::TcpStream,
    pm: ProcessManager,
    path: Arc<String>,
    cwd: Arc<PathBuf>,
) -> Result<()> {
    let (r, mut w) = stream.into_split();
    let mut r = BufReader::new(r);

    // Keep-alive: the agent reuses one connection for a whole session.
    while let Some(req) = http::read_request(&mut r).await? {
        let req_path = req.path.split('?').next().unwrap_or("");
        let response = if req_path != path.as_str() {
            // Deliberately indistinguishable from a wrong port: a probe learns
            // nothing about whether the token was close.
            http::Response::status(404, "not found")
        } else {
            match req.method.as_str() {
                "POST" => handle_post(&req.body, &pm, &cwd).await,
                // Forge never initiates messages, so there is no server-to-client
                // stream to open. The spec allows saying so.
                "GET" => {
                    http::Response::status(405, "this endpoint does not offer an event stream")
                }
                "DELETE" => http::Response::accepted(),
                _ => http::Response::status(405, "method not allowed"),
            }
        };
        response.write(&mut w).await?;
    }
    Ok(())
}

async fn handle_post(body: &[u8], pm: &ProcessManager, cwd: &std::path::Path) -> http::Response {
    let Ok(v) = serde_json::from_slice::<Value>(body) else {
        return http::Response::json(
            serde_json::to_vec(&error_response(Value::Null, -32700, "parse error")).unwrap(),
        );
    };

    match v {
        // A batch: reply with the responses to the requests it contained, and
        // with nothing at all if it was all notifications.
        Value::Array(items) => {
            let mut out = Vec::new();
            for item in items {
                if let Some(r) = dispatch(item, pm, cwd).await {
                    out.push(r);
                }
            }
            if out.is_empty() {
                http::Response::accepted()
            } else {
                http::Response::json(serde_json::to_vec(&out).unwrap_or_default())
            }
        }
        single => match dispatch(single, pm, cwd).await {
            Some(r) => http::Response::json(serde_json::to_vec(&r).unwrap_or_default()),
            None => http::Response::accepted(),
        },
    }
}

/// Handle one JSON-RPC message. `None` means it was a notification.
async fn dispatch(msg: Value, pm: &ProcessManager, cwd: &std::path::Path) -> Option<Value> {
    let method = msg.get("method").and_then(|m| m.as_str()).unwrap_or("");
    let id = msg.get("id").cloned();
    let params = msg.get("params").cloned().unwrap_or(Value::Null);

    // No id: a notification, or a response to something we never sent.
    let id = match id {
        Some(Value::Null) | None => {
            return None;
        }
        Some(id) => id,
    };

    let result = match method {
        "initialize" => Ok(json!({
            // Echo the client's revision when it names one it and we both
            // speak; the shapes this server uses have not changed across them.
            "protocolVersion": params
                .get("protocolVersion")
                .and_then(|p| p.as_str())
                .unwrap_or(PROTOCOL_VERSION),
            "capabilities": { "tools": { "listChanged": false } },
            "serverInfo": { "name": "forge", "version": env!("CARGO_PKG_VERSION") }
        })),

        "ping" => Ok(json!({})),

        "tools/list" => Ok(json!({ "tools": tools::descriptors() })),

        "tools/call" => {
            let name = params.get("name").and_then(|n| n.as_str()).unwrap_or("");
            let args = params.get("arguments").cloned().unwrap_or(json!({}));
            let outcome = tools::call(pm, cwd, name, &args).await;
            Ok(json!({
                "content": [{ "type": "text", "text": outcome.text }],
                "isError": outcome.is_error
            }))
        }

        other => Err(format!("method not found: {other}")),
    };

    Some(match result {
        Ok(r) => json!({ "jsonrpc": "2.0", "id": id, "result": r }),
        Err(msg) => error_response(id, -32601, &msg),
    })
}

fn error_response(id: Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

/// An unguessable path segment for this session's endpoint.
///
/// Not a secret that needs to survive an attacker with the process's memory —
/// it only has to be unpredictable to another process on the same machine that
/// is scanning loopback ports.
fn token() -> String {
    let mut seed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0x9E37_79B9_7F4A_7C15)
        ^ (std::process::id() as u64).rotate_left(32)
        ^ (&SEED_ANCHOR as *const _ as u64);

    let mut out = String::with_capacity(32);
    for _ in 0..4 {
        // splitmix64
        seed = seed.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = seed;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^= z >> 31;
        out.push_str(&format!("{z:016x}"));
    }
    out.truncate(48);
    out
}

/// A stable address to mix into the token seed; ASLR makes it unpredictable.
static SEED_ANCHOR: u8 = 0;

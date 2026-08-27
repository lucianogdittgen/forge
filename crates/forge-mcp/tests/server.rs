//! End-to-end tests against a real socket.
//!
//! These drive the server the way the agent process does — raw HTTP on
//! loopback — rather than calling the dispatch function directly, because the
//! transport is where a wrong `Content-Length` or a missing keep-alive would
//! hang the agent, and neither is visible from the JSON layer.

use forge_mcp::McpServer;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;

/// A deliberately dumb HTTP client: one request, one connection, no reuse.
async fn post_raw(url: &str, body: &str, chunked: bool) -> (u16, String) {
    let rest = url.strip_prefix("http://").unwrap();
    let (host, path) = rest.split_once('/').unwrap();
    let path = format!("/{path}");

    let mut s = TcpStream::connect(host).await.unwrap();
    let head = if chunked {
        format!(
            "POST {path} HTTP/1.1\r\nHost: {host}\r\nContent-Type: application/json\r\nTransfer-Encoding: chunked\r\n\r\n"
        )
    } else {
        format!(
            "POST {path} HTTP/1.1\r\nHost: {host}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n",
            body.len()
        )
    };
    s.write_all(head.as_bytes()).await.unwrap();
    if chunked {
        // Two chunks, to prove the decoder reassembles rather than reading one.
        let (a, b) = body.split_at(body.len() / 2);
        for part in [a, b] {
            s.write_all(format!("{:x}\r\n{part}\r\n", part.len()).as_bytes())
                .await
                .unwrap();
        }
        s.write_all(b"0\r\n\r\n").await.unwrap();
    } else {
        s.write_all(body.as_bytes()).await.unwrap();
    }
    s.flush().await.unwrap();

    let mut r = BufReader::new(s);
    let mut status_line = String::new();
    r.read_line(&mut status_line).await.unwrap();
    let status: u16 = status_line
        .split_whitespace()
        .nth(1)
        .unwrap()
        .parse()
        .unwrap();

    let mut len = 0usize;
    loop {
        let mut h = String::new();
        r.read_line(&mut h).await.unwrap();
        if h.trim().is_empty() {
            break;
        }
        if let Some((k, v)) = h.split_once(':') {
            if k.trim().eq_ignore_ascii_case("content-length") {
                len = v.trim().parse().unwrap();
            }
        }
    }
    let mut body = vec![0u8; len];
    r.read_exact(&mut body).await.unwrap();
    (status, String::from_utf8_lossy(&body).to_string())
}

async fn rpc(url: &str, method: &str, params: Value) -> Value {
    let req = json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": params });
    let (status, body) = post_raw(url, &req.to_string(), false).await;
    assert_eq!(status, 200, "{method} returned {status}: {body}");
    serde_json::from_str(&body).unwrap()
}

/// Call a tool and return its text content.
async fn call_tool(url: &str, name: &str, args: Value) -> (String, bool) {
    let v = rpc(
        url,
        "tools/call",
        json!({ "name": name, "arguments": args }),
    )
    .await;
    let result = &v["result"];
    let text = result["content"][0]["text"]
        .as_str()
        .unwrap_or("")
        .to_string();
    (text, result["isError"].as_bool().unwrap_or(false))
}

async fn server() -> McpServer {
    McpServer::start(forge_process::ProcessManager::new(), std::env::temp_dir())
        .await
        .unwrap()
}

#[tokio::test]
async fn initialize_and_list_the_tool_surface() {
    let s = server().await;

    let init = rpc(
        &s.url,
        "initialize",
        json!({ "protocolVersion": "2025-06-18" }),
    )
    .await;
    assert_eq!(init["result"]["protocolVersion"], "2025-06-18");
    assert_eq!(init["result"]["serverInfo"]["name"], "forge");

    let listed = rpc(&s.url, "tools/list", json!({})).await;
    let names: Vec<String> = listed["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap().to_string())
        .collect();

    // The whole surface, and nothing beyond it. A tool appearing here that the
    // permission table does not know about would classify as DESTRUCTIVE and
    // prompt on every call, so the two lists must not drift.
    let mut sorted = names.clone();
    sorted.sort();
    assert_eq!(
        sorted,
        vec![
            "proc_input",
            "proc_list",
            "proc_output",
            "proc_signal",
            "proc_start",
            "proc_status",
            "proc_wait"
        ]
    );
}

#[tokio::test]
async fn the_url_token_is_required() {
    let s = server().await;
    let bad = format!("http://127.0.0.1:{}/0000000000000000/mcp", s.port);
    let (status, _) = post_raw(&bad, r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#, false).await;
    assert_eq!(
        status, 404,
        "another local process must not reach the process manager"
    );
}

#[tokio::test]
async fn start_wait_and_read_a_process() {
    let s = server().await;

    let (text, err) = call_tool(
        &s.url,
        "proc_start",
        json!({ "command": "sh", "args": ["-c", "echo hello from forge; exit 3"] }),
    )
    .await;
    assert!(!err, "{text}");
    let started: Value = serde_json::from_str(&text).unwrap();
    let id = started["process_id"].as_str().unwrap().to_string();
    assert!(id.starts_with("proc-"));

    let (text, err) = call_tool(&s.url, "proc_wait", json!({ "process_id": id })).await;
    assert!(!err, "{text}");
    let done: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(done["state"], "exited");
    assert_eq!(
        done["exit_code"], 3,
        "a non-zero exit is a result, not a transport error"
    );

    let (out, err) = call_tool(&s.url, "proc_output", json!({ "process_id": id })).await;
    assert!(!err, "{out}");
    assert!(out.contains("hello from forge"), "got: {out:?}");
}

#[tokio::test]
async fn agent_sees_progress_output_collapsed() {
    // The rule that shaped the product, applied to the agent's view: a build
    // that rewrites one status line must not cost the agent 300 lines of
    // context, because it never was 300 lines on screen.
    let s = server().await;
    let script = "for i in $(seq 1 300); do printf '\\rTask %s/300' $i; done; printf '\\ndone\\n'";
    let (text, _) = call_tool(
        &s.url,
        "proc_start",
        json!({ "command": "sh", "args": ["-c", script] }),
    )
    .await;
    let id = serde_json::from_str::<Value>(&text).unwrap()["process_id"]
        .as_str()
        .unwrap()
        .to_string();

    call_tool(&s.url, "proc_wait", json!({ "process_id": id })).await;
    let (out, _) = call_tool(&s.url, "proc_output", json!({ "process_id": id })).await;

    let lines: Vec<&str> = out.lines().filter(|l| l.contains("Task")).collect();
    assert_eq!(
        lines.len(),
        1,
        "expected one collapsed progress line, got:\n{out}"
    );
    assert!(lines[0].contains("300/300"), "got: {:?}", lines[0]);
    assert!(out.contains("done"));
}

#[tokio::test]
async fn wait_times_out_without_calling_it_a_failure() {
    let s = server().await;
    let (text, _) = call_tool(
        &s.url,
        "proc_start",
        json!({ "command": "sleep", "args": ["30"] }),
    )
    .await;
    let id = serde_json::from_str::<Value>(&text).unwrap()["process_id"]
        .as_str()
        .unwrap()
        .to_string();

    let (text, err) = call_tool(
        &s.url,
        "proc_wait",
        json!({ "process_id": id, "timeout_ms": 250 }),
    )
    .await;
    assert!(!err, "a timeout is not a tool error");
    let v: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(v["timed_out"], true);
    assert_eq!(v["state"], "running");

    let (_, err) = call_tool(
        &s.url,
        "proc_signal",
        json!({ "process_id": id, "signal": "KILL" }),
    )
    .await;
    assert!(!err);
}

#[tokio::test]
async fn stdin_reaches_the_process() {
    let s = server().await;
    let (text, _) = call_tool(
        &s.url,
        "proc_start",
        json!({ "command": "sh", "args": ["-c", "read line; echo GOT:$line"] }),
    )
    .await;
    let id = serde_json::from_str::<Value>(&text).unwrap()["process_id"]
        .as_str()
        .unwrap()
        .to_string();

    call_tool(
        &s.url,
        "proc_input",
        json!({ "process_id": id, "data": "ping\n" }),
    )
    .await;
    call_tool(
        &s.url,
        "proc_wait",
        json!({ "process_id": id, "timeout_ms": 5000 }),
    )
    .await;

    let (out, _) = call_tool(&s.url, "proc_output", json!({ "process_id": id })).await;
    assert!(out.contains("GOT:ping"), "got: {out:?}");
}

#[tokio::test]
async fn a_command_that_does_not_exist_is_failed_not_exited() {
    let s = server().await;
    let (text, err) = call_tool(
        &s.url,
        "proc_start",
        json!({ "command": "forge-no-such-binary-xyz" }),
    )
    .await;

    // The model must not be able to mistake this for a build that failed: it
    // is told nothing started, and no id comes back to poll.
    assert!(err);
    assert!(text.contains("forge-no-such-binary-xyz"), "got: {text}");
    assert!(text.contains("no process id"), "got: {text}");

    // The attempt is still on the record, and classified apart from `exited`.
    let (list, _) = call_tool(&s.url, "proc_list", json!({})).await;
    let v: Value = serde_json::from_str(&list).unwrap();
    assert_eq!(v["processes"][0]["state"], "failed_to_start");
}

#[tokio::test]
async fn bad_arguments_come_back_as_tool_errors() {
    let s = server().await;

    let (text, err) = call_tool(&s.url, "proc_output", json!({ "process_id": "proc-999" })).await;
    assert!(err, "{text}");

    let (text, err) = call_tool(&s.url, "proc_status", json!({ "process_id": "banana" })).await;
    assert!(err, "{text}");

    let (text, err) = call_tool(&s.url, "not_a_tool", json!({})).await;
    assert!(err, "{text}");

    let (text, err) = call_tool(&s.url, "proc_start", json!({})).await;
    assert!(err, "{text}");
}

#[tokio::test]
async fn process_ids_are_accepted_in_the_forms_a_model_will_produce() {
    let s = server().await;
    let (text, _) = call_tool(&s.url, "proc_start", json!({ "command": "true" })).await;
    let id = serde_json::from_str::<Value>(&text).unwrap()["process_id"]
        .as_str()
        .unwrap()
        .to_string();
    let n = id.trim_start_matches("proc-").parse::<u64>().unwrap();

    for form in [json!(id), json!(n.to_string()), json!(n)] {
        let (text, err) = call_tool(&s.url, "proc_status", json!({ "process_id": form })).await;
        assert!(!err, "{text}");
    }
}

#[tokio::test]
async fn chunked_bodies_and_notifications_are_handled() {
    let s = server().await;

    // Node's HTTP client streams request bodies; a Content-Length-only reader
    // would block here forever.
    let req = json!({ "jsonrpc": "2.0", "id": 7, "method": "tools/list", "params": {} });
    let (status, body) = post_raw(&s.url, &req.to_string(), true).await;
    assert_eq!(status, 200);
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["id"], 7);

    // A notification has no id and must produce no JSON-RPC body.
    let note = json!({ "jsonrpc": "2.0", "method": "notifications/initialized" });
    let (status, body) = post_raw(&s.url, &note.to_string(), false).await;
    assert_eq!(status, 202);
    assert!(body.is_empty());
}

#[tokio::test]
async fn one_connection_serves_many_requests() {
    // The agent keeps a single connection open for the whole session; a server
    // that closed after each response would work in tests and stall in use.
    let s = server().await;
    let rest = s.url.strip_prefix("http://").unwrap();
    let (host, path) = rest.split_once('/').unwrap();
    let path = format!("/{path}");
    let (rd, mut wr) = TcpStream::connect(host).await.unwrap().into_split();
    let mut rd = BufReader::new(rd);

    for i in 1..=3 {
        let body = json!({ "jsonrpc": "2.0", "id": i, "method": "ping" }).to_string();
        wr.write_all(
            format!(
                "POST {path} HTTP/1.1\r\nHost: {host}\r\nContent-Length: {}\r\n\r\n{body}",
                body.len()
            )
            .as_bytes(),
        )
        .await
        .unwrap();
        wr.flush().await.unwrap();

        let mut status_line = String::new();
        rd.read_line(&mut status_line).await.unwrap();
        assert!(
            status_line.starts_with("HTTP/1.1 200"),
            "request {i}: {status_line}"
        );

        let mut len = 0usize;
        loop {
            let mut h = String::new();
            rd.read_line(&mut h).await.unwrap();
            if h.trim().is_empty() {
                break;
            }
            if let Some((k, v)) = h.split_once(':') {
                if k.trim().eq_ignore_ascii_case("content-length") {
                    len = v.trim().parse().unwrap();
                }
            }
        }
        let mut buf = vec![0u8; len];
        rd.read_exact(&mut buf).await.unwrap();
        let v: Value = serde_json::from_slice(&buf).unwrap();
        assert_eq!(v["id"], i, "responses must stay in step with requests");
    }
}

#[test]
fn every_tool_is_classified_by_the_permission_table() {
    // The permission table's fallback for an unknown tool is DESTRUCTIVE, so a
    // tool added here and forgotten there does not become unguarded — it
    // becomes unusable, prompting on every call. Either way it is a drift bug,
    // and this is where it should be caught.
    use forge_agent::Capability;

    for name in forge_mcp::tools::tool_names() {
        let direct = Capability::of_tool(&name);
        // The agent sees the MCP-prefixed wire name, not the bare one.
        let wire = Capability::of_tool(&format!("mcp__forge__{name}"));
        assert_eq!(
            direct, wire,
            "{name}: prefix stripping must not change the capability"
        );

        let expected = match name.as_str() {
            "proc_start" | "proc_input" => Capability::Execute,
            "proc_list" | "proc_status" | "proc_output" | "proc_wait" => Capability::Read,
            "proc_signal" => Capability::Destructive,
            other => panic!("tool {other:?} has no deliberate capability; add one"),
        };
        assert_eq!(
            direct,
            expected,
            "{name} is classified as {}",
            direct.label()
        );
    }

    // The property that makes the gate meaningful: killing a process can never
    // be waved through by a session grant.
    assert!(!Capability::Destructive.auto_approvable(&[
        Capability::Read,
        Capability::Execute,
        Capability::Destructive
    ]));
}

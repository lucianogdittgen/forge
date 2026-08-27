//! The tools Forge gives the agent, and their implementations.
//!
//! Every tool here is a thin projection of [`ProcessManager`]. That is the
//! point of the design: the agent does not get its own way to run programs, it
//! gets a handle on the *same* process manager that feeds the terminal pane.
//! There is no path by which the agent can start work the developer cannot see.
//!
//! Note what is absent. There is no `run_and_return_output`. `proc_start`
//! returns an id immediately and the agent is expected to keep reasoning while
//! the process runs; a build that takes forty minutes is not modelled as a
//! function call that takes forty minutes.
//!
//! Nothing here knows what BitBake is. `bitbake core-image-minimal` reaches the
//! kernel through exactly the same path as `ls`.

use std::time::Duration;

use forge_process::{ProcessManager, ProcessSpec, ProcessState};
use serde_json::{json, Value};

/// Rows and columns used to render retained output for the agent.
///
/// A width matters: output is replayed through the emulator, and a column
/// count that disagrees with the child's would re-wrap its lines.
const AGENT_VIEW_COLS: u16 = 200;
const DEFAULT_TAIL_ROWS: u16 = 120;

/// How long `proc_wait` will block by default. Bounded so a wait on a process
/// that never exits returns a useful "still running" rather than hanging the
/// agent's turn forever.
const DEFAULT_WAIT_MS: u64 = 30_000;
const MAX_WAIT_MS: u64 = 600_000;

/// Grace period before `proc_signal` with `TERM` escalates to `KILL`.
const TERMINATE_GRACE: Duration = Duration::from_secs(5);

/// The tool descriptors sent in response to `tools/list`.
///
/// Descriptions are written for the model, and they carry the operating rules
/// the model cannot infer from the schema: that starting is not waiting, and
/// that the developer is watching the same output.
pub fn descriptors() -> Vec<Value> {
    vec![
        json!({
            "name": "proc_start",
            "description":
                "Start a program in a real terminal and return immediately with its process id. \
                 The developer sees this process live in Forge's terminal pane as it runs — you are \
                 not relaying output to them, they already have it. This does NOT wait for the \
                 program to finish: use proc_wait when you need the result, and keep working \
                 meanwhile when you do not. Long builds are normal and expected.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "command": { "type": "string", "description": "Program to run, e.g. \"bitbake\"." },
                    "args": { "type": "array", "items": { "type": "string" } },
                    "cwd": { "type": "string", "description": "Working directory. Defaults to the workspace root." },
                    "label": { "type": "string", "description": "Short human-readable label shown in the UI." }
                },
                "required": ["command"]
            }
        }),
        json!({
            "name": "proc_list",
            "description": "List every process Forge has started this session, running or finished.",
            "inputSchema": { "type": "object", "properties": {} }
        }),
        json!({
            "name": "proc_status",
            "description":
                "State of one process: running, exited (with code), failed to start, or interrupted. \
                 A non-zero exit code is a normal result to reason about, not a Forge error.",
            "inputSchema": {
                "type": "object",
                "properties": { "process_id": { "type": "string" } },
                "required": ["process_id"]
            }
        }),
        json!({
            "name": "proc_output",
            "description":
                "The tail of a process's output, rendered exactly as it appears on the developer's \
                 screen: progress lines that rewrite themselves with carriage returns are collapsed, \
                 and colour codes are stripped. Safe to call while the process is still running.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "process_id": { "type": "string" },
                    "lines": { "type": "integer", "description": "How many trailing lines to return (default 120, max 1000)." }
                },
                "required": ["process_id"]
            }
        }),
        json!({
            "name": "proc_wait",
            "description":
                "Wait for a process to finish, up to a timeout. Returns as soon as it ends, or \
                 reports that it is still running when the timeout expires — which is not an error.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "process_id": { "type": "string" },
                    "timeout_ms": { "type": "integer", "description": "Default 30000, max 600000." }
                },
                "required": ["process_id"]
            }
        }),
        json!({
            "name": "proc_input",
            "description":
                "Write to a process's stdin, as if the developer had typed it. Include a trailing \
                 newline to submit a line. Use this to answer a prompt from an interactive program.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "process_id": { "type": "string" },
                    "data": { "type": "string" }
                },
                "required": ["process_id", "data"]
            }
        }),
        json!({
            "name": "proc_signal",
            "description":
                "Signal a process. INT is Ctrl-C. TERM asks it to stop and escalates to KILL after \
                 a grace period. This destroys in-flight work and always requires the developer's approval.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "process_id": { "type": "string" },
                    "signal": { "type": "string", "enum": ["INT", "TERM", "KILL", "HUP", "QUIT"] }
                },
                "required": ["process_id", "signal"]
            }
        }),
    ]
}

pub fn tool_names() -> Vec<String> {
    descriptors()
        .iter()
        .filter_map(|d| d.get("name").and_then(|n| n.as_str()).map(String::from))
        .collect()
}

/// The result of a tool call: text for the model, plus whether it failed.
pub struct ToolOutcome {
    pub text: String,
    pub is_error: bool,
}

impl ToolOutcome {
    fn ok(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            is_error: false,
        }
    }
    fn err(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            is_error: true,
        }
    }
    fn json(v: Value) -> Self {
        Self {
            text: serde_json::to_string_pretty(&v).unwrap_or_default(),
            is_error: false,
        }
    }
}

/// Execute one tool call.
///
/// Errors are returned as `is_error` outcomes rather than transport failures:
/// a bad argument is something the model should see and correct, not something
/// that should tear down the session.
pub async fn call(
    pm: &ProcessManager,
    default_cwd: &std::path::Path,
    name: &str,
    args: &Value,
) -> ToolOutcome {
    match name {
        "proc_start" => proc_start(pm, default_cwd, args),
        "proc_list" => proc_list(pm),
        "proc_status" => with_id(pm, args, |pm, id| ToolOutcome::json(record_json(pm, id))),
        "proc_output" => proc_output(pm, args),
        "proc_wait" => proc_wait(pm, args).await,
        "proc_input" => proc_input(pm, args),
        "proc_signal" => proc_signal(pm, args),
        other => ToolOutcome::err(format!("unknown tool {other:?}")),
    }
}

fn proc_start(pm: &ProcessManager, default_cwd: &std::path::Path, args: &Value) -> ToolOutcome {
    let Some(command) = args.get("command").and_then(|v| v.as_str()) else {
        return ToolOutcome::err("proc_start requires a `command`");
    };
    if command.trim().is_empty() {
        return ToolOutcome::err("`command` must not be empty");
    }

    let mut spec = ProcessSpec::new(command);
    if let Some(list) = args.get("args").and_then(|v| v.as_array()) {
        for a in list {
            match a.as_str() {
                Some(s) => spec = spec.arg(s),
                None => return ToolOutcome::err("every entry in `args` must be a string"),
            }
        }
    }
    spec = spec.cwd(
        args.get("cwd")
            .and_then(|v| v.as_str())
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| default_cwd.to_path_buf()),
    );
    if let Some(l) = args.get("label").and_then(|v| v.as_str()) {
        spec = spec.label(l);
    }
    // Wide enough that the agent's replay and the developer's pane agree on
    // where lines wrap; the pane resizes the child to its own geometry the
    // moment it attaches.
    spec = spec.size(50, AGENT_VIEW_COLS);

    match pm.start(spec) {
        Ok(id) => ToolOutcome::json(json!({
            "process_id": id.to_string(),
            "state": "starting",
            "note": "Started. The developer can see this running now. Use proc_wait for the result."
        })),
        // A spawn failure is Forge-level, not a build result. Say so plainly
        // so the model corrects the command instead of reasoning about an
        // imaginary exit code. The record is still listed as `failed_to_start`.
        Err(e) => ToolOutcome::err(format!(
            "could not start `{command}`: {e}. Nothing is running; no process id was created."
        )),
    }
}

fn proc_list(pm: &ProcessManager) -> ToolOutcome {
    let procs: Vec<Value> = pm
        .list()
        .into_iter()
        .map(|r| {
            json!({
                "process_id": r.id.to_string(),
                "command": full_command(&r.command, &r.args),
                "label": r.label,
                "state": state_name(r.state),
                "exit_code": r.exit_code,
                "runtime_secs": r.runtime().map(|d| d.as_secs()),
            })
        })
        .collect();
    ToolOutcome::json(json!({ "processes": procs }))
}

fn proc_output(pm: &ProcessManager, args: &Value) -> ToolOutcome {
    let id = match parse_id(args) {
        Ok(id) => id,
        Err(e) => return ToolOutcome::err(e),
    };
    let rows = args
        .get("lines")
        .and_then(|v| v.as_u64())
        .unwrap_or(DEFAULT_TAIL_ROWS as u64)
        .clamp(1, 1000) as u16;

    let cols = pm
        .get(id)
        .map(|r| r.cols.max(20))
        .unwrap_or(AGENT_VIEW_COLS);

    match pm.output_snapshot(id) {
        Ok((bytes, dropped)) => {
            let text = forge_terminal::render_transcript(&bytes, rows, cols);
            let mut out = String::new();
            if dropped > 0 {
                // Never hand back a silently truncated transcript.
                out.push_str(&format!(
                    "[{dropped} earlier bytes were dropped from Forge's retained buffer]\n"
                ));
            }
            out.push_str(&text);
            ToolOutcome::ok(out)
        }
        Err(e) => ToolOutcome::err(e.to_string()),
    }
}

async fn proc_wait(pm: &ProcessManager, args: &Value) -> ToolOutcome {
    let id = match parse_id(args) {
        Ok(id) => id,
        Err(e) => return ToolOutcome::err(e),
    };
    let timeout = Duration::from_millis(
        args.get("timeout_ms")
            .and_then(|v| v.as_u64())
            .unwrap_or(DEFAULT_WAIT_MS)
            .min(MAX_WAIT_MS),
    );

    // Subscribe *before* reading the state, or a process that exits between the
    // two would leave this waiting for an event that already happened.
    let mut rx = match pm.subscribe(id) {
        Ok(rx) => rx,
        Err(e) => return ToolOutcome::err(e.to_string()),
    };
    match pm.get(id) {
        Ok(r) if r.state.is_terminal() => return ToolOutcome::json(record_json(pm, id)),
        Ok(_) => {}
        Err(e) => return ToolOutcome::err(e.to_string()),
    }

    let waited = tokio::time::timeout(timeout, async {
        loop {
            match rx.recv().await {
                Ok(forge_process::ProcessEvent::Exited { .. }) => return true,
                Ok(forge_process::ProcessEvent::StateChanged(s)) if s.is_terminal() => return true,
                Ok(_) => continue,
                // Lagging on output events is irrelevant to a waiter; the
                // channel is still live, so keep waiting.
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(_) => return true,
            }
        }
    })
    .await;

    let mut v = record_json(pm, id);
    if waited.is_err() {
        v["timed_out"] = json!(true);
        v["note"] = json!(
            "Still running — this is not an error. Wait again, or do something else meanwhile."
        );
    }
    ToolOutcome::json(v)
}

fn proc_input(pm: &ProcessManager, args: &Value) -> ToolOutcome {
    let id = match parse_id(args) {
        Ok(id) => id,
        Err(e) => return ToolOutcome::err(e),
    };
    let Some(data) = args.get("data").and_then(|v| v.as_str()) else {
        return ToolOutcome::err("proc_input requires `data`");
    };
    match pm.write_stdin(id, data.as_bytes()) {
        Ok(()) => ToolOutcome::json(json!({ "written_bytes": data.len() })),
        Err(e) => ToolOutcome::err(e.to_string()),
    }
}

fn proc_signal(pm: &ProcessManager, args: &Value) -> ToolOutcome {
    use nix::sys::signal::Signal;

    let id = match parse_id(args) {
        Ok(id) => id,
        Err(e) => return ToolOutcome::err(e),
    };
    let name = args
        .get("signal")
        .and_then(|v| v.as_str())
        .unwrap_or("TERM");

    let result = match name.trim_start_matches("SIG").to_ascii_uppercase().as_str() {
        // Ctrl-C goes through the same path a human keystroke would: to the
        // foreground process group, so grandchildren get it too.
        "INT" => pm.interrupt(id),
        "TERM" => pm.terminate(id, TERMINATE_GRACE),
        "KILL" => pm.signal(id, Signal::SIGKILL),
        "HUP" => pm.signal(id, Signal::SIGHUP),
        "QUIT" => pm.signal(id, Signal::SIGQUIT),
        other => return ToolOutcome::err(format!("unsupported signal {other:?}")),
    };

    match result {
        Ok(()) => ToolOutcome::json(json!({ "signalled": id.to_string(), "signal": name })),
        Err(e) => ToolOutcome::err(e.to_string()),
    }
}

fn with_id(
    pm: &ProcessManager,
    args: &Value,
    f: impl FnOnce(&ProcessManager, forge_process::ProcessId) -> ToolOutcome,
) -> ToolOutcome {
    match parse_id(args) {
        Ok(id) => match pm.get(id) {
            Ok(_) => f(pm, id),
            Err(e) => ToolOutcome::err(e.to_string()),
        },
        Err(e) => ToolOutcome::err(e),
    }
}

fn record_json(pm: &ProcessManager, id: forge_process::ProcessId) -> Value {
    match pm.get(id) {
        Ok(r) => json!({
            "process_id": r.id.to_string(),
            "command": full_command(&r.command, &r.args),
            "state": state_name(r.state),
            "exit_code": r.exit_code,
            "signal": r.signal,
            "runtime_secs": r.runtime().map(|d| d.as_secs()),
            "bytes_output": r.bytes_out,
        }),
        Err(e) => json!({ "error": e.to_string() }),
    }
}

fn full_command(command: &str, args: &[String]) -> String {
    if args.is_empty() {
        command.to_string()
    } else {
        format!("{command} {}", args.join(" "))
    }
}

/// Names the model sees. `failed` is deliberately distinct from `exited`: a
/// build that returns 1 is a result, a command that does not exist is a
/// mistake, and collapsing them teaches the model the wrong lesson.
fn state_name(s: ProcessState) -> &'static str {
    match s {
        ProcessState::Starting => "starting",
        ProcessState::Running => "running",
        ProcessState::Stopping => "stopping",
        ProcessState::Exited => "exited",
        ProcessState::Failed => "failed_to_start",
        ProcessState::Interrupted => "interrupted",
    }
}

/// Accept `"proc-3"`, `"3"`, or `3`. The model sees `proc-3` everywhere, but
/// rejecting the other two would be pedantry that costs a whole turn.
fn parse_id(args: &Value) -> Result<forge_process::ProcessId, String> {
    let v = args
        .get("process_id")
        .ok_or_else(|| "missing `process_id`".to_string())?;

    let n = match v {
        Value::Number(n) => n.as_u64(),
        Value::String(s) => s.trim().trim_start_matches("proc-").parse::<u64>().ok(),
        _ => None,
    };
    n.map(forge_process::ProcessId)
        .ok_or_else(|| format!("`process_id` must look like \"proc-3\"; got {v}"))
}

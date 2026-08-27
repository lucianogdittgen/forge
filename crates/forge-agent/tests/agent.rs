//! Protocol parsing, capability classification, and the argv safety assertion.

use forge_agent::claude::ClaudeAgentConfig;
use forge_agent::permission::{Capability, Decision};
use forge_agent::protocol::{parse_line, ContentBlock, Incoming};

// ---------------------------------------------------------------- argv safety

/// The default argv must contain every flag the permission model depends on.
///
/// Each of these was proven necessary by the verification spike; dropping any
/// one of them silently reopens a bypass.
#[test]
fn default_argv_contains_the_load_bearing_flags() {
    let cfg = ClaudeAgentConfig::new("/tmp");
    let argv = cfg.argv();
    let joined = argv.join(" ");

    for flag in [
        "--tools",                    // strips all built-ins
        "--strict-mcp-config",        // refuses the user's MCP servers
        "--disable-slash-commands",   // removes the skills surface
        "--setting-sources",          // blocks settings-file allow-rules
        "--permission-prompt-tool",   // the gate itself
        "--include-partial-messages", // needed for deltas
        "--verbose",                  // stream-json emits nothing useful without it
    ] {
        assert!(
            argv.iter().any(|a| a == flag),
            "missing {flag} in: {joined}"
        );
    }

    // --tools names an explicit subset. What matters is not which tools are on
    // the list but that no command-runner is: with `Bash` the agent could run a
    // build the terminal pane never sees, and pay for its output in context.
    let ti = argv.iter().position(|a| a == "--tools").unwrap();
    let listed: Vec<&str> = argv[ti + 1].split(',').collect();
    assert!(listed.contains(&"Read") && listed.contains(&"Edit"));
    for shell in ["Bash", "Task", "Workflow", "Skill"] {
        assert!(
            !listed.contains(&shell),
            "{shell} must not be granted: {listed:?}"
        );
    }
    assert_ne!(argv[ti + 1], "default", "--tools default is every built-in");

    // --setting-sources must be followed by an empty string; a missing value
    // would let user settings plant allow-rules that void the gate.
    let si = argv.iter().position(|a| a == "--setting-sources").unwrap();
    assert_eq!(
        argv[si + 1],
        "",
        "--setting-sources must be given an empty value"
    );

    assert_eq!(
        argv[argv
            .iter()
            .position(|a| a == "--permission-prompt-tool")
            .unwrap()
            + 1],
        "stdio"
    );
}

#[test]
fn default_argv_is_accepted() {
    let argv = ClaudeAgentConfig::new("/tmp").argv();
    assert!(ClaudeAgentConfig::assert_argv_safe(&argv).is_ok());
}

/// Each of these was PROVEN to bypass the gate. Forge must refuse to start.
#[test]
fn forbidden_flags_are_rejected() {
    for bad in [
        vec![
            "--allowed-tools".to_string(),
            "mcp__forge__proc_start".to_string(),
        ],
        vec!["--dangerously-skip-permissions".to_string()],
        vec!["--allow-dangerously-skip-permissions".to_string()],
        vec![
            "--permission-mode".to_string(),
            "bypassPermissions".to_string(),
        ],
    ] {
        let mut argv = ClaudeAgentConfig::new("/tmp").argv();
        argv.extend(bad.clone());
        let res = ClaudeAgentConfig::assert_argv_safe(&argv);
        assert!(res.is_err(), "{bad:?} should have been refused");
        let msg = res.unwrap_err().to_string();
        assert!(msg.contains("permission gate"), "unhelpful error: {msg}");
    }
}

/// `--flag=value` form must be caught too.
#[test]
fn forbidden_flags_are_rejected_in_equals_form() {
    let mut argv = ClaudeAgentConfig::new("/tmp").argv();
    argv.push("--allowed-tools=mcp__forge__proc_start".into());
    assert!(ClaudeAgentConfig::assert_argv_safe(&argv).is_err());
}

/// A harmless permission mode must not be refused.
#[test]
fn benign_permission_mode_is_allowed() {
    let mut argv = ClaudeAgentConfig::new("/tmp").argv();
    argv.extend(["--permission-mode".to_string(), "default".to_string()]);
    assert!(ClaudeAgentConfig::assert_argv_safe(&argv).is_ok());
}

// ------------------------------------------------------------- capabilities

#[test]
fn tools_classify_to_their_capabilities() {
    assert_eq!(
        Capability::of_tool("mcp__forge__proc_start"),
        Capability::Execute
    );
    assert_eq!(
        Capability::of_tool("mcp__forge__proc_status"),
        Capability::Read
    );
    assert_eq!(
        Capability::of_tool("mcp__forge__proc_signal"),
        Capability::Destructive
    );
    // Bare names work as well as wire names.
    assert_eq!(Capability::of_tool("proc_output"), Capability::Read);

    // The built-ins Forge hands the agent are classified too; without this they
    // fall through to Destructive and every edit prompts.
    assert_eq!(Capability::of_tool("Read"), Capability::Read);
    assert_eq!(Capability::of_tool("Edit"), Capability::Write);
    assert_eq!(Capability::of_tool("Write"), Capability::Write);
    assert_eq!(Capability::of_tool("NotebookEdit"), Capability::Write);
    assert_eq!(Capability::of_tool("WebFetch"), Capability::Network);
}

/// An unrecognised tool must be treated as the most dangerous thing it could be.
#[test]
fn unknown_tools_are_destructive() {
    assert_eq!(
        Capability::of_tool("mcp__forge__something_new"),
        Capability::Destructive
    );
    assert_eq!(Capability::of_tool("Bash"), Capability::Destructive);
}

#[test]
fn granted_capabilities_auto_approve_but_destructive_never_does() {
    let granted = vec![
        Capability::Read,
        Capability::Execute,
        Capability::Destructive,
    ];
    assert!(Capability::Read.auto_approvable(&granted));
    assert!(Capability::Execute.auto_approvable(&granted));
    // Even explicitly granted, this must still ask.
    assert!(
        !Capability::Destructive.auto_approvable(&granted),
        "DESTRUCTIVE must never be auto-approved"
    );
    // Not granted -> asks.
    assert!(!Capability::Write.auto_approvable(&granted));
}

// ---------------------------------------------------------------- protocol

#[test]
fn parses_init_and_reports_the_tool_surface() {
    let line = r#"{"type":"system","subtype":"init","session_id":"abc","tools":["mcp__forge__proc_start"]}"#;
    match parse_line(line) {
        Incoming::Init { session_id, tools } => {
            assert_eq!(session_id, "abc");
            assert_eq!(tools, vec!["mcp__forge__proc_start"]);
        }
        other => panic!("expected Init, got {other:?}"),
    }
}

#[test]
fn parses_assistant_text_and_tool_use() {
    let line = r#"{"type":"assistant","message":{"content":[
        {"type":"text","text":"I'll start the build."},
        {"type":"tool_use","id":"toolu_1","name":"mcp__forge__proc_start","input":{"cmd":"bitbake core-image-minimal"}}
    ]}}"#;
    match parse_line(line) {
        Incoming::Assistant { content } => {
            assert_eq!(content.len(), 2);
            assert!(matches!(&content[0], ContentBlock::Text(t) if t.contains("start the build")));
            match &content[1] {
                ContentBlock::ToolUse { id, name, input } => {
                    assert_eq!(id, "toolu_1");
                    assert_eq!(name, "mcp__forge__proc_start");
                    assert_eq!(input["cmd"], "bitbake core-image-minimal");
                }
                other => panic!("expected ToolUse, got {other:?}"),
            }
        }
        other => panic!("expected Assistant, got {other:?}"),
    }
}

#[test]
fn parses_a_control_request() {
    let line = r#"{"type":"control_request","request_id":"req-1","request":{
        "subtype":"can_use_tool","tool_name":"mcp__forge__proc_start",
        "input":{"cmd":"rm -rf /"},"decision_reason":"Forge policy"}}"#;
    match parse_line(line) {
        Incoming::ControlRequest {
            request_id,
            tool_name,
            input,
            reason,
        } => {
            assert_eq!(request_id, "req-1");
            assert_eq!(tool_name, "mcp__forge__proc_start");
            assert_eq!(input["cmd"], "rm -rf /");
            assert_eq!(reason.as_deref(), Some("Forge policy"));
        }
        other => panic!("expected ControlRequest, got {other:?}"),
    }
}

#[test]
fn parses_result_with_cost() {
    let line = r#"{"type":"result","session_id":"s1","is_error":false,"total_cost_usd":0.0067}"#;
    match parse_line(line) {
        Incoming::Result {
            session_id,
            is_error,
            cost_usd,
        } => {
            assert_eq!(session_id, "s1");
            assert!(!is_error);
            assert_eq!(cost_usd, Some(0.0067));
        }
        other => panic!("expected Result, got {other:?}"),
    }
}

#[test]
fn parses_stream_deltas() {
    let line = r#"{"type":"stream_event","event":{"delta":{"type":"text_delta","text":"Hel"}}}"#;
    match parse_line(line) {
        Incoming::StreamDelta { kind, text } => {
            assert_eq!(kind, "text_delta");
            assert_eq!(text, "Hel");
        }
        other => panic!("expected StreamDelta, got {other:?}"),
    }
}

/// Non-JSON output was observed in the wild. It must not be fatal.
#[test]
fn non_json_lines_are_tolerated() {
    assert!(matches!(
        parse_line("No conversation found with session ID: bogus"),
        Incoming::Unparsed(_)
    ));
}

/// Event types we have never seen must be skipped, not treated as errors.
#[test]
fn unknown_event_types_are_skipped() {
    for line in [
        r#"{"type":"rate_limit_event","limit":"x"}"#,
        r#"{"type":"compact_boundary"}"#,
        r#"{"type":"session_end"}"#,
    ] {
        assert!(
            matches!(parse_line(line), Incoming::Other { .. }),
            "{line} should be Other"
        );
    }
}

#[test]
fn empty_lines_are_harmless() {
    assert!(matches!(parse_line("   "), Incoming::Other { .. }));
}

// -------------------------------------------------------- control responses

#[test]
fn allow_serialises_correctly() {
    let s = forge_agent::protocol::control_response("req-1", &Decision::Allow);
    let v: serde_json::Value = serde_json::from_str(&s).unwrap();
    assert_eq!(v["type"], "control_response");
    assert_eq!(v["response"]["request_id"], "req-1");
    assert_eq!(v["response"]["response"]["behavior"], "allow");
}

#[test]
fn deny_carries_forges_message_to_the_model() {
    let s = forge_agent::protocol::control_response(
        "req-2",
        &Decision::Deny {
            message: "DESTRUCTIVE not granted".into(),
        },
    );
    let v: serde_json::Value = serde_json::from_str(&s).unwrap();
    assert_eq!(v["response"]["response"]["behavior"], "deny");
    assert_eq!(
        v["response"]["response"]["message"],
        "DESTRUCTIVE not granted"
    );
}

/// The gate can clamp arguments, not just accept or refuse.
#[test]
fn allow_with_rewritten_input_serialises() {
    let s = forge_agent::protocol::control_response(
        "req-3",
        &Decision::AllowWithInput(serde_json::json!({"cmd": "bitbake -k core-image-minimal"})),
    );
    let v: serde_json::Value = serde_json::from_str(&s).unwrap();
    assert_eq!(v["response"]["response"]["behavior"], "allow");
    assert_eq!(
        v["response"]["response"]["updatedInput"]["cmd"],
        "bitbake -k core-image-minimal"
    );
}

/// A packaged Forge cannot assume `claude` is on PATH — the flake says so in
/// its own package description, which must not be a promise the code breaks.
#[test]
fn the_claude_binary_can_be_overridden() {
    use forge_agent::claude::resolve_binary;

    assert_eq!(resolve_binary(None), "claude");
    assert_eq!(
        resolve_binary(Some("/nix/store/abc/bin/claude".into())),
        "/nix/store/abc/bin/claude"
    );
    // An empty or blank value in a shell profile must not break the default.
    assert_eq!(resolve_binary(Some(String::new())), "claude");
    assert_eq!(resolve_binary(Some("   ".into())), "claude");
}

/// Which tools actually reach Forge's permission gate.
///
/// Forge's whole permission model assumes `--permission-prompt-tool stdio`
/// invokes `canUseTool` for *built-in* tools, not only MCP ones. That is an
/// assumption about the CLI's behaviour, not about Forge's code, so it is
/// checked against the real binary rather than reasoned about.
///
/// Ignored by default: needs the CLI, credentials and a network round trip.
/// Run with `cargo test -p forge-agent -- --ignored --nocapture`.
#[tokio::test]
#[ignore = "spawns the real claude CLI"]
async fn the_gate_sees_built_in_tools() {
    use forge_agent::claude::{ClaudeAgent, ClaudeAgentConfig};
    use forge_agent::{Agent, AgentEvent, Decision};
    use std::time::Duration;

    let dir = std::env::temp_dir().join("forge-gate-probe");
    std::fs::create_dir_all(&dir).unwrap();
    let readable = dir.join("probe.txt");
    std::fs::write(&readable, "before\n").unwrap();
    // Deliberately *outside* the workspace, so the policy asks rather than
    // auto-approving and the gate event is observable at all.
    let outside = std::env::temp_dir().join("forge-gate-probe-outside.txt");
    std::fs::write(&outside, "before\n").unwrap();

    let mut cfg = ClaudeAgentConfig::new(dir.clone());
    cfg.tools = ["Read", "Edit", "Write"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    // Grant nothing, so anything the gate sees must surface as a request.
    cfg.granted = vec![];
    cfg.system_prompt = Some("Do exactly what you are asked, with no commentary.".into());

    let mut agent = ClaudeAgent::spawn(cfg).await.unwrap();
    agent
        .send(&format!(
            "Read the file {} and then use Edit on {} to change the word \
             `before` to `after`.",
            readable.display(),
            outside.display()
        ))
        .await
        .unwrap();

    let mut surface: Vec<String> = Vec::new();
    let mut called: Vec<String> = Vec::new();
    let mut gated: Vec<String> = Vec::new();

    let deadline = tokio::time::Instant::now() + Duration::from_secs(120);
    loop {
        let Ok(ev) = tokio::time::timeout_at(deadline, agent.next_event()).await else {
            break;
        };
        let Some(ev) = ev else { break };
        match ev {
            AgentEvent::Ready { tools } => surface = tools,
            AgentEvent::ToolCall { name, .. } => called.push(name),
            AgentEvent::ApprovalRequested(req) => {
                gated.push(req.tool_name.clone());
                req.respond(Decision::Allow);
            }
            AgentEvent::TurnFinished { .. } => break,
            _ => {}
        }
    }

    eprintln!("surface: {surface:?}");
    eprintln!("called:  {called:?}");
    eprintln!("gated:   {gated:?}");

    assert!(
        !surface.contains(&"Bash".to_string()),
        "Bash leaked in: {surface:?}"
    );
    assert!(
        called.iter().any(|t| t == "Edit"),
        "the model never tried to edit"
    );
    assert!(
        gated.iter().any(|t| t == "Edit"),
        "Edit did NOT reach Forge's gate — the permission model cannot cover edits. \
         called={called:?} gated={gated:?}"
    );

    // The other half, and the one worth pinning down: reads go through the gate
    // too. Nothing is granted here, so a `Read` that Forge cannot auto-approve
    // must surface as a request — and it does. The gate is therefore a real
    // boundary for every capability class, not only for writes.
    //
    // This is easy to get backwards. If `granted` contains `Read`, Forge
    // approves it itself and no request is ever emitted; that looks identical
    // to the CLI not asking. Hence `granted = []` above.
    assert!(
        called.iter().any(|t| t == "Read"),
        "the model never tried to read, so this proves nothing: called={called:?}"
    );
    assert!(
        gated.iter().any(|t| t == "Read"),
        "Read did not reach the gate — Forge cannot deny reads. gated={gated:?}"
    );

    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_file(&outside);
}

mod policy {
    use forge_agent::{Capability, Policy, Verdict};
    use serde_json::json;

    fn workspace() -> (tempdir::Dir, Policy) {
        let d = tempdir::Dir::new("forge-policy");
        let p = Policy::new(d.path(), vec![Capability::Read]);
        (d, p)
    }

    fn edit(path: &str) -> serde_json::Value {
        json!({ "file_path": path })
    }

    #[test]
    fn an_edit_inside_the_workspace_does_not_interrupt() {
        let (d, p) = workspace();
        let inside = d.path().join("src").join("main.rs");
        std::fs::create_dir_all(inside.parent().unwrap()).unwrap();
        assert_eq!(
            p.decide("Edit", &edit(inside.to_str().unwrap())),
            Verdict::Allow
        );
    }

    #[test]
    fn a_file_that_does_not_exist_yet_is_still_inside() {
        // A `Write` names its target before it exists; refusing to resolve it
        // would make creating a file always prompt.
        let (d, p) = workspace();
        let new = d.path().join("does/not/exist/yet.rs");
        assert_eq!(
            p.decide("Write", &edit(new.to_str().unwrap())),
            Verdict::Allow
        );
    }

    #[test]
    fn a_relative_path_is_resolved_against_the_workspace() {
        let (_d, p) = workspace();
        assert_eq!(p.decide("Edit", &edit("src/lib.rs")), Verdict::Allow);
    }

    #[test]
    fn an_edit_outside_the_workspace_asks() {
        let (_d, p) = workspace();
        assert_eq!(p.decide("Edit", &edit("/etc/passwd")), Verdict::Ask);
        assert_eq!(
            p.decide("Write", &edit("/home/someone/.ssh/authorized_keys")),
            Verdict::Ask
        );
    }

    #[test]
    fn dot_dot_cannot_climb_out_of_the_workspace() {
        let (_d, p) = workspace();
        assert_eq!(p.decide("Edit", &edit("../../../etc/passwd")), Verdict::Ask);
        assert_eq!(
            p.decide("Edit", &edit("src/../../outside.rs")),
            Verdict::Ask
        );
    }

    #[test]
    fn dot_dot_that_stays_inside_is_still_inside() {
        let (d, p) = workspace();
        std::fs::create_dir_all(d.path().join("a")).unwrap();
        assert_eq!(p.decide("Edit", &edit("a/../b.rs")), Verdict::Allow);
    }

    #[test]
    fn a_symlink_out_of_the_tree_does_not_smuggle_a_path_in() {
        // The check must resolve links, or `ws/escape/passwd` reads as inside
        // the workspace while pointing at /etc.
        let (d, p) = workspace();
        let link = d.path().join("escape");
        std::os::unix::fs::symlink("/etc", &link).unwrap();
        assert_eq!(p.decide("Edit", &edit("escape/passwd")), Verdict::Ask);
    }

    #[test]
    fn a_call_with_no_path_asks_rather_than_guessing() {
        let (_d, p) = workspace();
        assert_eq!(p.decide("Edit", &json!({})), Verdict::Ask);
        assert_eq!(p.decide("Edit", &edit("")), Verdict::Ask);
    }

    #[test]
    fn reads_and_granted_capabilities_go_through() {
        let (_d, p) = workspace();
        assert_eq!(p.decide("Read", &edit("/anywhere/at/all")), Verdict::Allow);
        assert_eq!(
            p.decide("mcp__forge__proc_list", &json!({})),
            Verdict::Allow
        );
    }

    #[test]
    fn starting_a_process_and_killing_one_both_ask() {
        let (_d, p) = workspace();
        assert_eq!(
            p.decide("mcp__forge__proc_start", &json!({"command": "bitbake"})),
            Verdict::Ask
        );
        assert_eq!(
            p.decide("mcp__forge__proc_signal", &json!({"signal": "KILL"})),
            Verdict::Ask
        );
    }

    #[test]
    fn a_tool_forge_does_not_know_asks_even_if_everything_is_granted() {
        let d = tempdir::Dir::new("forge-policy-granted");
        let p = Policy::new(
            d.path(),
            vec![
                Capability::Read,
                Capability::Write,
                Capability::Execute,
                Capability::Network,
                Capability::Destructive,
            ],
        );
        assert_eq!(p.decide("SomeNewTool", &json!({})), Verdict::Ask);
    }

    /// Minimal scratch directory; the workspace must be a real path because the
    /// policy canonicalises it.
    pub mod tempdir {
        use std::path::{Path, PathBuf};
        use std::sync::atomic::{AtomicU32, Ordering};

        static N: AtomicU32 = AtomicU32::new(0);

        pub struct Dir(PathBuf);

        impl Dir {
            pub fn new(tag: &str) -> Self {
                let n = N.fetch_add(1, Ordering::Relaxed);
                let p = std::env::temp_dir().join(format!("{tag}-{}-{n}", std::process::id()));
                std::fs::create_dir_all(&p).unwrap();
                Self(p)
            }
            pub fn path(&self) -> &Path {
                &self.0
            }
        }

        impl Drop for Dir {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
    }
}

/// The assertion that keeps the product's promise, at startup rather than in a
/// comment: a shell tool would let the agent run work the pane cannot show.
#[test]
fn forge_refuses_to_start_with_a_shell_tool() {
    use forge_agent::claude::ClaudeAgentConfig;

    for shell in ["Bash", "Task", "Workflow", "Skill"] {
        let mut cfg = ClaudeAgentConfig::new("/tmp");
        cfg.tools = vec!["Read".into(), shell.to_string()];
        let err = ClaudeAgentConfig::assert_argv_safe(&cfg.argv())
            .expect_err("{shell} should have been refused");
        let msg = err.to_string();
        assert!(msg.contains(shell), "error should name the tool: {msg}");
    }

    // The default surface must actually pass its own check.
    let cfg = ClaudeAgentConfig::new("/tmp");
    ClaudeAgentConfig::assert_argv_safe(&cfg.argv()).unwrap();
}

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

    // --tools and --setting-sources must be followed by an empty string; a
    // missing value would mean "default", i.e. wide open.
    let ti = argv.iter().position(|a| a == "--tools").unwrap();
    assert_eq!(argv[ti + 1], "", "--tools must be given an empty value");
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
        Capability::of_tool("mcp__forge__fs_write"),
        Capability::Write
    );
    assert_eq!(
        Capability::of_tool("mcp__forge__proc_signal"),
        Capability::Destructive
    );
    // Bare names work as well as wire names.
    assert_eq!(Capability::of_tool("fs_read"), Capability::Read);
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

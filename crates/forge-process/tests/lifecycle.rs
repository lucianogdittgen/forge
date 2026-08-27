//! Behavioural tests for the process manager.
//!
//! These assert the semantics that are easy to get subtly wrong and that only
//! fail under real workloads: the two distinct interrupt mechanisms, group
//! signalling, and the Exited/Failed distinction.

use std::time::Duration;

use forge_process::{ProcessEvent, ProcessManager, ProcessSpec, ProcessState};

/// Collect output until the process reaches a terminal state, or time out.
async fn run_to_exit(
    pm: &ProcessManager,
    id: forge_process::ProcessId,
    timeout: Duration,
) -> (String, Option<i32>) {
    let (snapshot, mut rx) = pm.attach(id).expect("attach");
    let mut out = String::from_utf8_lossy(&snapshot).to_string();
    let deadline = tokio::time::Instant::now() + timeout;

    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return (out, None);
        }
        match tokio::time::timeout(remaining, rx.recv()).await {
            Ok(Ok(ProcessEvent::Output(b))) => out.push_str(&String::from_utf8_lossy(&b)),
            Ok(Ok(ProcessEvent::Exited { code, .. })) => return (out, code),
            Ok(Ok(ProcessEvent::StateChanged(_))) => {}
            Ok(Err(_)) | Err(_) => return (out, None),
        }
    }
}

async fn wait_terminal(pm: &ProcessManager, id: forge_process::ProcessId, timeout: Duration) -> ProcessState {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let st = pm.get(id).expect("record").state;
        if st.is_terminal() || tokio::time::Instant::now() >= deadline {
            return st;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test]
async fn captures_output_and_exit_code() {
    let pm = ProcessManager::new();
    let id = pm
        .start(ProcessSpec::new("sh").arg("-c").arg("echo hello-forge; exit 7"))
        .expect("start");

    let (out, code) = run_to_exit(&pm, id, Duration::from_secs(10)).await;
    assert!(out.contains("hello-forge"), "output was: {out:?}");
    assert_eq!(code, Some(7));
    assert_eq!(pm.get(id).unwrap().state, ProcessState::Exited);
}

/// A non-zero exit is EXITED, not FAILED.
///
/// The agent must be able to tell "the build failed" (informative, reason about
/// it) from "Forge could not run the command" (a tooling problem).
#[tokio::test]
async fn nonzero_exit_is_exited_not_failed() {
    let pm = ProcessManager::new();
    let id = pm.start(ProcessSpec::new("sh").arg("-c").arg("exit 3")).expect("start");
    let (_, code) = run_to_exit(&pm, id, Duration::from_secs(10)).await;
    assert_eq!(code, Some(3));
    assert_eq!(pm.get(id).unwrap().state, ProcessState::Exited);
}

#[tokio::test]
async fn missing_binary_is_failed() {
    let pm = ProcessManager::new();
    let res = pm.start(ProcessSpec::new("forge-no-such-binary-xyzzy"));
    assert!(res.is_err(), "spawning a missing binary must not succeed");

    let rec = pm.list().into_iter().next().expect("a record is still retained");
    assert_eq!(rec.state, ProcessState::Failed);
    assert_eq!(rec.exit_code, None);
}

#[tokio::test]
async fn resize_is_visible_to_the_child() {
    let pm = ProcessManager::new();
    // Give the child a moment to settle before asking, so the resize lands first.
    let id = pm
        .start(
            ProcessSpec::new("sh")
                .arg("-c")
                .arg("sleep 0.4; stty size")
                .size(24, 80),
        )
        .expect("start");

    pm.resize(id, 40, 120).expect("resize");
    let (out, _) = run_to_exit(&pm, id, Duration::from_secs(10)).await;
    assert!(out.contains("40 120"), "child saw: {out:?}");
}

#[tokio::test]
async fn stdin_reaches_the_child() {
    let pm = ProcessManager::new();
    let id = pm.start(ProcessSpec::new("sh").arg("-c").arg("read line; echo got:$line")).expect("start");
    tokio::time::sleep(Duration::from_millis(300)).await;
    pm.write_stdin(id, b"ping\n").expect("write");

    let (out, _) = run_to_exit(&pm, id, Duration::from_secs(10)).await;
    assert!(out.contains("got:ping"), "output was: {out:?}");
}

/// The load-bearing test.
///
/// A child in raw mode (`stty -isig`, as `vim`, `less`, and every full-screen
/// program do) receives `0x03` as ordinary data and is NOT interrupted by it.
/// Only `killpg(SIGINT)` reaches it. A Stop control implemented by writing
/// `0x03` would appear to work in `bash` and silently fail everywhere else.
#[cfg(unix)]
#[tokio::test]
async fn raw_mode_child_ignores_etx_but_not_killpg() {
    let pm = ProcessManager::new();
    let id = pm
        .start(ProcessSpec::new("sh").arg("-c").arg("stty -isig; sleep 30"))
        .expect("start");

    // Let the shell apply raw mode before we test it.
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Writing ETX must NOT kill it.
    pm.write_stdin(id, &[0x03]).expect("write etx");
    tokio::time::sleep(Duration::from_millis(600)).await;
    assert_eq!(
        pm.get(id).unwrap().state,
        ProcessState::Running,
        "0x03 must not interrupt a child that disabled ISIG"
    );

    // killpg(SIGINT) must.
    pm.interrupt(id).expect("interrupt");
    let st = wait_terminal(&pm, id, Duration::from_secs(5)).await;
    assert!(st.is_terminal(), "killpg(SIGINT) must reach a raw-mode child, got {st:?}");
}

/// In cooked mode the keystroke path *does* work, which is why both exist.
#[cfg(unix)]
#[tokio::test]
async fn cooked_mode_child_is_interrupted_by_etx() {
    let pm = ProcessManager::new();
    let id = pm.start(ProcessSpec::new("sh").arg("-c").arg("sleep 30")).expect("start");
    tokio::time::sleep(Duration::from_millis(400)).await;

    pm.write_stdin(id, &[0x03]).expect("write etx");
    let st = wait_terminal(&pm, id, Duration::from_secs(5)).await;
    assert!(st.is_terminal(), "0x03 should interrupt a default-mode child, got {st:?}");
}

/// Signals must reach grandchildren.
///
/// A real build is `sh -c` → `bitbake` → compilers. Signalling only the direct
/// child would leave the actual work running.
#[cfg(unix)]
#[tokio::test]
async fn terminate_reaches_grandchildren() {
    let pm = ProcessManager::new();
    // The inner `sleep` is a separate process; `echo` after it forces sh to fork
    // rather than exec the sleep directly.
    let id = pm
        .start(ProcessSpec::new("sh").arg("-c").arg("sleep 30; echo never"))
        .expect("start");
    tokio::time::sleep(Duration::from_millis(400)).await;

    let pid = pm.get(id).unwrap().pid.expect("pid");
    pm.terminate(id, Duration::from_millis(300)).expect("terminate");
    let st = wait_terminal(&pm, id, Duration::from_secs(8)).await;
    assert_eq!(st, ProcessState::Interrupted, "a stopped process stays INTERRUPTED");

    // The whole group must be gone, not just the shell.
    tokio::time::sleep(Duration::from_millis(300)).await;
    let alive = std::process::Command::new("pgrep")
        .arg("-g")
        .arg(pid.to_string())
        .output()
        .map(|o| !o.stdout.is_empty())
        .unwrap_or(false);
    assert!(!alive, "process group {pid} still has live members after terminate");
}

/// A process that ignores SIGINT must still die.
#[cfg(unix)]
#[tokio::test]
async fn terminate_escalates_past_a_stubborn_child() {
    let pm = ProcessManager::new();
    let id = pm
        .start(ProcessSpec::new("sh").arg("-c").arg("trap '' INT TERM; sleep 30"))
        .expect("start");
    tokio::time::sleep(Duration::from_millis(500)).await;

    pm.terminate(id, Duration::from_millis(400)).expect("terminate");
    let st = wait_terminal(&pm, id, Duration::from_secs(10)).await;
    assert!(st.is_terminal(), "escalation to SIGKILL must terminate it, got {st:?}");
}

/// Attaching late must not lose output already produced.
#[tokio::test]
async fn attach_replays_output_produced_before_attaching() {
    let pm = ProcessManager::new();
    let id = pm
        .start(ProcessSpec::new("sh").arg("-c").arg("echo early-line; sleep 1; echo late-line"))
        .expect("start");

    // Deliberately attach after the first line has certainly been written.
    tokio::time::sleep(Duration::from_millis(500)).await;
    let (out, _) = run_to_exit(&pm, id, Duration::from_secs(10)).await;

    assert!(out.contains("early-line"), "late attach lost early output: {out:?}");
    assert!(out.contains("late-line"), "late attach missed later output: {out:?}");
}

/// A carriage-return progress bar must not be retained as thousands of lines.
/// The emulator collapses it; here we only assert the bytes flow through intact.
#[tokio::test]
async fn carriage_return_progress_streams_through() {
    let pm = ProcessManager::new();
    let id = pm
        .start(ProcessSpec::new("sh").arg("-c").arg(
            "i=0; while [ $i -lt 50 ]; do printf 'progress: %d%%\\r' $i; i=$((i+1)); done; echo",
        ))
        .expect("start");

    let (out, _) = run_to_exit(&pm, id, Duration::from_secs(10)).await;
    assert!(out.contains('\r'), "carriage returns must survive to the emulator");
    assert!(out.contains("progress:"), "output was: {out:?}");
}

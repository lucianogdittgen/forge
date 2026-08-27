//! Forge — an AI development workbench that does not take your terminal away.

mod agent_session;
mod app;

use std::io::stdout;

use anyhow::Result;
use crossterm::event::{
    DisableMouseCapture, EnableMouseCapture, EventStream, KeyboardEnhancementFlags,
    PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use forge_process::ProcessSpec;
use ratatui::prelude::*;

use crate::agent_session::AgentSession;
use crate::app::App;

#[tokio::main]
async fn main() -> Result<()> {
    let cwd = std::env::current_dir()?;
    let workspace = cwd
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "workspace".into());

    // The command to run in the terminal pane. Defaults to the user's shell so
    // the pane is immediately useful.
    let args: Vec<String> = std::env::args().skip(1).collect();
    let spec = if args.is_empty() {
        let shell = std::env::var("SHELL").unwrap_or_else(|_| "bash".into());
        ProcessSpec::new(shell)
    } else {
        let mut s = ProcessSpec::new(&args[0]);
        for a in &args[1..] {
            s = s.arg(a);
        }
        s
    }
    .cwd(cwd.clone());

    let mut app = App::new(workspace);

    // Start the agent before taking over the screen, so a missing `claude`
    // binary prints a plain message instead of a warning nobody sees. Forge is
    // still a usable terminal without it, so this is not fatal.
    match AgentSession::start(app.pm.clone(), cwd).await {
        Ok(session) => app.attach_agent(session),
        Err(e) => app.note_no_agent(&format!("{e:#}")),
    }

    let mut terminal = setup()?;

    if let Err(e) = app.start_process(spec) {
        restore()?;
        eprintln!("forge: could not start process: {e}");
        std::process::exit(1);
    }

    let mut events = EventStream::new();
    let res = run(&mut terminal, &mut app, &mut events).await;

    restore()?;
    res
}

async fn run(
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    app: &mut App,
    events: &mut EventStream,
) -> Result<()> {
    while !app.should_quit() {
        terminal.draw(|f| app.render(f))?;
        app.tick(events).await?;
    }
    Ok(())
}

fn setup() -> Result<Terminal<CrosstermBackend<std::io::Stdout>>> {
    enable_raw_mode()?;
    let mut out = stdout();
    execute!(out, EnterAlternateScreen, EnableMouseCapture)?;
    // Disambiguate key events so we can tell Ctrl-C from other keys reliably.
    // Not every terminal supports it; failure is not fatal.
    let _ = execute!(
        out,
        PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
    );
    Ok(Terminal::new(CrosstermBackend::new(out))?)
}

fn restore() -> Result<()> {
    let mut out = stdout();
    let _ = execute!(out, PopKeyboardEnhancementFlags);
    execute!(out, LeaveAlternateScreen, DisableMouseCapture)?;
    disable_raw_mode()?;
    Ok(())
}

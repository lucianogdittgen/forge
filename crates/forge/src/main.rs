//! Forge — an AI development workbench that does not take your terminal away.

mod app;

use std::io::stdout;

use anyhow::Result;
use crossterm::event::{
    DisableMouseCapture, EnableMouseCapture, EventStream, KeyboardEnhancementFlags,
    PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use forge_process::ProcessSpec;
use ratatui::prelude::*;

use crate::app::App;

#[tokio::main]
async fn main() -> Result<()> {
    let workspace = std::env::current_dir()?
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
    .cwd(std::env::current_dir()?);

    let mut terminal = setup()?;
    let mut app = App::new(workspace);

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

//! Interactive TUI for live hybrid search.
//!
//! Loads the persistent index once on a background OS thread, then
//! debounces queries (~20ms) so each keystroke re-runs in tens of
//! milliseconds. The UI is a ratatui app over crossterm: top bar,
//! query input, results list + preview pane, status keys.
//!
//! On exit, the TUI may print `path:line` to stdout (for shell
//! integration) or spawn `$EDITOR` on the selected file:line.

mod app;
mod search;
mod ui;

use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use crossterm::ExecutableCommand;
use crossterm::cursor::Show;
use crossterm::event::{DisableBracketedPaste, EnableBracketedPaste};
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;

use crate::tui::app::{App, AppRunResult, ExitAction};
use crate::tui::search::{WorkerCmd, spawn_worker};
use crate::util;

pub fn run(
    path: String,
    multilingual: bool,
    include_text_files: bool,
    use_cache: bool,
    debug_keys: bool,
) -> Result<()> {
    // Load the index BEFORE entering the alternate screen so any progress
    // / error / model-download chatter shows up in the user's normal
    // terminal scrollback. The "Loading..." line stays — it's the only
    // feedback for an index build that may take several seconds. The
    // post-load "Loaded X chunks" line is dropped because it would be
    // wiped a few millis later by EnterAlternateScreen and never seen;
    // the stats are surfaced in the TUI's top bar instead.
    eprintln!("Loading index for {path} ...");
    io::stderr().flush().ok();
    let index = util::open_index(&path, multilingual, include_text_files, use_cache)
        .with_context(|| format!("loading index for {path}"))?;
    let stats = index.stats();
    let total_files = stats.indexed_files;
    let total_chunks = stats.total_chunks;
    let index = Arc::new(index);

    // Background search worker.
    let (cmd_tx, cmd_rx) = crossbeam_channel::unbounded::<WorkerCmd>();
    let (msg_tx, msg_rx) = crossbeam_channel::unbounded();
    let worker = spawn_worker(index.clone(), cmd_rx, msg_tx);

    // Resolve repo path for opening files in $EDITOR. We only know how to
    // do this for local paths — git URLs land in a temp dir that we don't
    // currently expose, so editor open will fall back to the bare path.
    let repo_path: PathBuf = if util::is_git_url(&path) {
        PathBuf::from(".")
    } else {
        PathBuf::from(&path)
            .canonicalize()
            .unwrap_or_else(|_| PathBuf::from(&path))
    };

    // Enter the TUI inside a guarded scope so the terminal is always
    // restored, even if the app panics mid-render.
    let app_result;
    let exit_action;
    {
        enable_raw_mode().context("enable raw mode")?;
        let mut stdout = io::stdout();
        crossterm::execute!(stdout, EnterAlternateScreen, EnableBracketedPaste)
            .context("enter alternate screen")?;
        let backend = CrosstermBackend::new(stdout);
        let mut terminal = Terminal::new(backend).context("init terminal")?;
        let _guard = TerminalGuard;

        let _ = multilingual; // currently informational only; TUI inherits the loaded model.
        let mut app = App::new(
            repo_path,
            total_files,
            total_chunks,
            index.clone(),
            cmd_tx.clone(),
            msg_rx,
        );
        app.key_debug = debug_keys;
        // Outer loop: run the App until it asks to quit. Editor-open
        // yields control here so we can suspend the terminal cleanly,
        // spawn $EDITOR, and resume — keeping the App's state intact.
        app_result = loop {
            match app.run(&mut terminal) {
                Ok(AppRunResult::Quit) => break Ok(()),
                Ok(AppRunResult::OpenEditor { file, line }) => {
                    if let Err(e) = suspend_and_edit(&mut terminal, &file, line) {
                        // Editor failure shouldn't kill the TUI — surface
                        // it as a status toast and stay in the search loop.
                        app.status_msg = Some((
                            format!("editor failed: {e}"),
                            std::time::Instant::now() + std::time::Duration::from_secs(3),
                        ));
                    }
                    // The file on disk may have been modified; drop the
                    // cached split lines so the preview reflects reality.
                    app.invalidate_preview_cache();
                }
                Err(e) => break Err(e),
            }
        };
        exit_action = app.exit_action.take();
    }

    // Tear down the worker after the terminal is restored.
    let _ = cmd_tx.send(WorkerCmd::Shutdown);
    drop(cmd_tx);
    let _ = worker.join();

    app_result?;

    if let Some(action) = exit_action {
        match action {
            ExitAction::Print(s) => println!("{s}"),
        }
    }

    Ok(())
}

/// Drop out of the alternate screen + raw mode, spawn the editor
/// synchronously, then restore the TUI. Called from the outer driver
/// in response to `AppRunResult::OpenEditor`; the search worker and
/// crossbeam channels are untouched, so dispatching resumes seamlessly.
fn suspend_and_edit<B: ratatui::backend::Backend + std::io::Write>(
    terminal: &mut Terminal<B>,
    file: &Path,
    line: usize,
) -> Result<()> {
    // Suspend: leave alt screen + disable raw mode so the editor sees a
    // normal terminal. Order matters — disable raw mode AFTER leaving
    // the alt screen so the leave-sequence is processed correctly.
    // Also turn off bracketed paste so the editor doesn't see stray
    // ESC[200~ wrappers when the user pastes into it.
    crossterm::execute!(io::stdout(), DisableBracketedPaste, LeaveAlternateScreen)
        .context("leave alternate screen")?;
    disable_raw_mode().context("disable raw mode")?;
    crossterm::execute!(io::stdout(), Show).context("show cursor")?;

    // Run editor synchronously — the user's shell-style "back to vim
    // for a sec" workflow expects to block here.
    let edit_result = run_editor(file, line);

    // Resume: re-enter alt screen + raw mode + bracketed paste, then
    // force a full repaint.
    enable_raw_mode().context("re-enable raw mode")?;
    crossterm::execute!(io::stdout(), EnterAlternateScreen, EnableBracketedPaste)
        .context("re-enter alternate screen")?;
    terminal
        .clear()
        .context("clear terminal after editor return")?;

    edit_result
}

struct TerminalGuard;

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = io::stdout().execute(DisableBracketedPaste);
        let _ = io::stdout().execute(LeaveAlternateScreen);
        let _ = io::stdout().execute(Show);
    }
}

fn run_editor(file: &Path, line: usize) -> Result<()> {
    let editor = std::env::var("VISUAL")
        .or_else(|_| std::env::var("EDITOR"))
        .unwrap_or_else(|_| "vi".to_string());
    // Heuristic: vi/vim/nvim/emacs/nano accept `+N file`. VS Code,
    // Cursor (which forks VS Code's CLI), and Windsurf accept
    // `--goto file:line`. Helix uses `file:line`.
    let mut cmd = std::process::Command::new(&editor);
    let editor_basename = Path::new(&editor)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(&editor);
    if editor_basename.contains("code")
        || editor_basename.contains("cursor")
        || editor_basename.contains("windsurf")
    {
        cmd.arg("--goto").arg(format!("{}:{line}", file.display()));
    } else if editor_basename.contains("hx") || editor_basename.contains("helix") {
        cmd.arg(format!("{}:{line}", file.display()));
    } else {
        cmd.arg(format!("+{line}")).arg(file);
    }
    cmd.status().with_context(|| format!("running {editor}"))?;
    Ok(())
}

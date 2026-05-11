//! TUI app state, input handling, event loop.
//!
//! The [`App`] owns *all* mutable UI state. It also owns the channel
//! handles for talking to the background search worker, which means the
//! UI thread never blocks on the index — it just dispatches generation-
//! tagged commands and discards stale results.

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use crossbeam_channel::{Receiver, Sender, TryRecvError};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::Terminal;
use ratatui::backend::Backend;
use veles_core::scope::ScopeIndex;
use veles_core::types::{SearchMode, SearchResult};

use crate::tui::search::{Filters, ResultKind, SearchDone, WorkerCmd, WorkerMsg};

/// Wait this long after a query change before kicking the worker.
/// Veles searches finish in tens of ms even on large repos, so this can
/// be tiny — just enough to coalesce a fast keystroke burst.
const DEBOUNCE_MS: u64 = 20;

/// How many results to fetch from the worker. The viewport rarely shows
/// more than ~30 rows so 50 is plenty of overscroll.
const TOP_K: usize = 50;

/// Cap how often the event loop wakes up when nothing is happening.
///
/// crossterm's event::poll has no way to wake on a custom signal — the
/// only way the search worker's "results ready" message reaches the UI
/// thread is via the next poll cycle's `try_recv` drain. Keep this
/// short enough that worker completions feel near-instant
/// (≤ 1 frame at 60fps) while still letting the CPU sleep when the
/// user isn't typing.
const TICK_TIMEOUT_MS: u64 = 100;

/// Tighter tick while a search is in flight — gives results a chance
/// to land within ~one frame after the worker finishes. The cost is a
/// few extra wakeups per second, but they're trivial: each wakeup
/// runs `try_recv` (an atomic load) and re-renders the spinner.
const SEARCHING_TICK_MS: u64 = 25;

/// Wait this long after a dispatch before flashing the "searching..."
/// spinner. Sub-frame searches (BM25 typically lands in 5-20ms) would
/// otherwise create a constant micro-flicker as the user types.
const SPINNER_DELAY_MS: u64 = 80;

/// LRU bound for cached file contents used to render previews.
const PREVIEW_FILE_CACHE: usize = 8;

/// What kind of result list is currently being displayed.
#[derive(Clone)]
pub enum ResultsKind {
    Query { query: String },
    Related { anchor: String },
    Defs { name: String },
    Refs { name: String, def_count: usize },
}

/// Snapshot of everything the back/forward history needs to restore a
/// previously displayed view. Cloned at the moment the user navigates
/// away from it (Ctrl-R / Ctrl-D / Ctrl-F, or first keystroke while a
/// non-Query view is displayed) so Alt-Left can put it back exactly.
#[derive(Clone)]
pub struct ViewSnapshot {
    pub query: String,
    pub cursor_chars: usize,
    pub mode: SearchMode,
    pub filter_lang: Option<String>,
    pub filter_path: String,
    pub results: Vec<SearchResult>,
    pub results_kind: ResultsKind,
    pub elapsed_ms: u64,
    pub selected: usize,
    pub list_offset: usize,
    pub preview_scroll: i32,
}

/// How many past / future views to remember. Each snapshot holds a
/// `Vec<SearchResult>` (50 chunks worth), so 32 is well under a MB.
const HISTORY_CAP: usize = 32;

/// How many past queries to remember for Ctrl-Up / Ctrl-Down recall.
/// Strings only (no result clone), so 50 is cheap.
const QUERY_HISTORY_CAP: usize = 50;

/// Action to perform after the TUI exits cleanly.
pub enum ExitAction {
    /// Print `path:line` to stdout. Pipe-friendly:
    ///   `$EDITOR $(veles tui)` or `veles tui | xargs -r $EDITOR`.
    Print(String),
}

/// What `App::run` returns when its event loop yields control back to
/// the outer driver. `Quit` is final; `OpenEditor` is a pause request
/// — the caller suspends the terminal, runs the editor, restores, and
/// re-enters `run` so the same `App` keeps its query / results / worker.
pub enum AppRunResult {
    Quit,
    OpenEditor { file: PathBuf, line: usize },
}

pub struct App {
    pub repo_path: PathBuf,
    /// Cached `~/relative/path` form of `repo_path`, computed once in
    /// `App::new` instead of on every top-bar render. The home-prefix
    /// lookup hits `std::env::var_os("HOME")` which is a syscall on
    /// macOS — small, but it ran ~60 times/sec at the previous tick
    /// rate for no reason.
    pub repo_short: String,
    pub total_chunks: usize,
    pub total_files: usize,
    /// Shared with the search worker. Held here so the renderer can pull
    /// scope labels (`index.symbols()`) out of the same tree-sitter table
    /// the worker is searching against.
    pub index: Arc<veles_core::VelesIndex>,
    /// Pre-built `file_path → symbol indices` lookup used by the render
    /// loop to resolve scope labels in O(symbols_in_file) instead of
    /// O(total_symbols) per row. Built once from `index.symbols()` and
    /// reused for the lifetime of the TUI.
    pub scope_index: ScopeIndex,

    // Query input.
    pub query: String,
    /// Cursor position counted in chars, not bytes — handles Cyrillic / CJK
    /// queries cleanly. Always in `0..=query.chars().count()`.
    pub cursor_chars: usize,

    // Search dispatch state.
    pub mode: SearchMode,
    pub seq: u64,
    pub displayed_seq: u64,
    pub searching: bool,
    /// Wall-clock instant the current in-flight search began. Used to
    /// gate the "searching..." spinner — searches that finish in under
    /// ~80ms wouldn't visibly flash the spinner anyway, and the brief
    /// appear/disappear was a constant micro-flicker when typing.
    pub search_started_at: Option<Instant>,
    pub pending_query: bool,
    pub next_dispatch_at: Option<Instant>,

    // Results.
    pub results: Vec<SearchResult>,
    pub results_kind: ResultsKind,
    pub elapsed_ms: u64,
    pub selected: usize,
    pub list_offset: usize,
    /// User-driven scroll offset for the preview pane, in lines.
    /// Positive scrolls down, negative scrolls up; reset to 0 whenever
    /// the selection changes so the new chunk is shown from its
    /// match-centred default window.
    pub preview_scroll: i32,

    // Preview file cache (LRU). Files are usually small; we cache their
    // line splits so navigating between results in the same file is free.
    pub current_preview: Option<Arc<Vec<String>>>,
    current_preview_path: Option<String>,
    preview_cache: HashMap<String, Arc<Vec<String>>>,
    preview_order: VecDeque<String>,

    // Renderer caches. Rebuilt on apply_results / restore / typing so
    // the per-frame render path doesn't reallocate. `cached_terms` is
    // the original-case terms list used to display chunk labels;
    // `cached_lower_terms` is the lowercased mirror used by
    // highlight_terms and find_match_line (which otherwise lowercase
    // per call per visible line). `cached_match_line` holds the
    // anchor for the current selection so render_preview doesn't
    // re-scan the chunk on every redraw.
    pub cached_terms: Vec<String>,
    pub cached_lower_terms: Vec<String>,
    pub cached_match_line: Option<usize>,
    /// (selected_idx, file_path) identifying which selection the
    /// `cached_match_line` belongs to.
    cached_match_line_for: Option<(usize, String)>,

    // Filters. `languages` is the sorted distinct language list pulled
    // from `index.stats()` at construction time — used for Ctrl-T cycle.
    // `filter_lang` / `filter_path` are passed to every worker dispatch.
    // When `editing_path_filter` is true, query-input keys edit the path
    // glob instead of `query` (and the query box renders a different
    // prompt). Esc / Enter exit the mode without quitting the TUI.
    pub languages: Vec<String>,
    pub filter_lang: Option<String>,
    pub filter_path: String,
    pub filter_path_cursor: usize,
    pub editing_path_filter: bool,

    // UI state.
    pub help_open: bool,
    /// Vertical scroll offset (in lines) for the help modal. Lets the
    /// content overflow the modal height — without this the bottom
    /// sections were invisible on shorter terminals.
    pub help_scroll: u16,
    /// When true, every keypress is echoed to the status line.
    /// Enabled via the `--debug-keys` CLI flag (not a runtime toggle —
    /// most Ctrl-* combos collide with the user's shell or terminal,
    /// notably Ctrl-\ which is SIGQUIT). Diagnostic aid for users on
    /// platforms where some modifier combos (Alt on default macOS
    /// Terminal, F-keys with system-key takeover) don't arrive.
    pub key_debug: bool,
    pub spinner_tick: u64,
    pub status_msg: Option<(String, Instant)>,
    pub quit: bool,
    pub exit_action: Option<ExitAction>,
    /// When the user hits Ctrl-O we don't quit — we set this and the
    /// run loop yields control via `AppRunResult::OpenEditor`. The outer
    /// driver suspends the terminal, runs $EDITOR, and re-enters `run`.
    pub pending_editor: Option<(PathBuf, usize)>,

    // Back/forward history. `history_back` is a stack of past views
    // (oldest at index 0, most recent at the top); `history_forward`
    // holds views the user popped via Alt-Left and can replay with
    // Alt-Right. `current_view_pushed` guards against double-snapshot:
    // each displayed view is pushed at most once on its way out.
    pub history_back: Vec<ViewSnapshot>,
    pub history_forward: Vec<ViewSnapshot>,
    current_view_pushed: bool,

    // Query history. Recorded at "commit" points (Ctrl-R/D/F, Enter,
    // Ctrl-O, Ctrl-U-clear); navigated with Ctrl-Up / Ctrl-Down.
    // `query_history_idx == None` means "drafting", `query_draft` is
    // the un-recalled text the user had typed before entering history.
    pub query_history: Vec<String>,
    pub query_history_idx: Option<usize>,
    pub query_draft: String,

    // Channels.
    cmd_tx: Sender<WorkerCmd>,
    msg_rx: Receiver<WorkerMsg>,
}

impl App {
    pub fn new(
        repo_path: PathBuf,
        total_files: usize,
        total_chunks: usize,
        index: Arc<veles_core::VelesIndex>,
        cmd_tx: Sender<WorkerCmd>,
        msg_rx: Receiver<WorkerMsg>,
    ) -> Self {
        let scope_index = ScopeIndex::new(index.symbols());
        let repo_short = shorten_with_home(&repo_path);
        // Sorted, deduped language list for Ctrl-T cycling. Pulled once
        // from stats — the index doesn't change during a TUI session.
        let mut languages: Vec<String> = index.stats().languages.keys().cloned().collect();
        languages.sort();
        Self {
            repo_path,
            repo_short,
            total_files,
            total_chunks,
            index,
            scope_index,
            query: String::new(),
            cursor_chars: 0,
            mode: SearchMode::Hybrid,
            seq: 0,
            displayed_seq: 0,
            searching: false,
            search_started_at: None,
            pending_query: false,
            next_dispatch_at: None,
            results: Vec::new(),
            results_kind: ResultsKind::Query {
                query: String::new(),
            },
            elapsed_ms: 0,
            selected: 0,
            list_offset: 0,
            preview_scroll: 0,
            current_preview: None,
            current_preview_path: None,
            preview_cache: HashMap::new(),
            preview_order: VecDeque::new(),
            cached_terms: Vec::new(),
            cached_lower_terms: Vec::new(),
            cached_match_line: None,
            cached_match_line_for: None,
            languages,
            filter_lang: None,
            filter_path: String::new(),
            filter_path_cursor: 0,
            editing_path_filter: false,
            help_open: false,
            help_scroll: 0,
            key_debug: false,
            spinner_tick: 0,
            status_msg: None,
            quit: false,
            exit_action: None,
            pending_editor: None,
            history_back: Vec::new(),
            history_forward: Vec::new(),
            current_view_pushed: false,
            query_history: Vec::new(),
            query_history_idx: None,
            query_draft: String::new(),
            cmd_tx,
            msg_rx,
        }
    }

    fn current_filters(&self) -> Filters {
        Filters {
            lang: self.filter_lang.clone(),
            path: if self.filter_path.trim().is_empty() {
                None
            } else {
                Some(self.filter_path.trim().to_string())
            },
        }
    }

    pub fn run<B: Backend>(&mut self, terminal: &mut Terminal<B>) -> Result<AppRunResult> {
        loop {
            // 1. Drain worker messages without blocking.
            loop {
                match self.msg_rx.try_recv() {
                    Ok(msg) => self.handle_worker_msg(msg),
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        self.quit = true;
                        break;
                    }
                }
            }

            // 2. Maybe kick off a debounced search.
            if let Some(cmd) = self.maybe_dispatch_query() {
                let _ = self.cmd_tx.send(cmd);
            }

            // 3. Refresh preview cache for current selection.
            self.refresh_preview();
            // 3b. Pre-compute the per-selection match-line anchor so
            // render_preview can be `&App` (no mutating ops inside the
            // immutable render path).
            let _ = self.match_line_for_selection();

            // 4. Render.
            terminal.draw(|f| crate::tui::ui::render(f, self))?;

            // 5. Wait for input or short tick.
            let timeout = self.pick_timeout();
            if event::poll(timeout)? {
                match event::read()? {
                    Event::Key(k) if k.kind == KeyEventKind::Press => self.handle_key(k),
                    Event::Paste(s) => self.handle_paste(&s),
                    Event::Resize(_, _) => {
                        // ratatui repaints on the next frame from
                        // fresh layout, but Paragraph caches its line
                        // metrics — force a redraw via clear so the
                        // top bar / help modal pick up the new width.
                        // Also kicks the per-selection match-line
                        // cache so a narrower viewport gets a fresh
                        // anchor on the next render.
                        terminal.clear()?;
                        self.cached_match_line = None;
                        self.cached_match_line_for = None;
                    }
                    _ => {}
                }
            }

            self.spinner_tick = self.spinner_tick.wrapping_add(1);
            if self.quit {
                return Ok(AppRunResult::Quit);
            }
            // Editor-open is checked AFTER quit so a quit from inside
            // an editor handler (shouldn't happen, but cheap to guard)
            // still wins. The outer driver will run $EDITOR and call
            // `run` again — the App's state persists.
            if let Some((file, line)) = self.pending_editor.take() {
                return Ok(AppRunResult::OpenEditor { file, line });
            }
        }
    }

    /// Drop cached file contents — used after the user has been off in
    /// $EDITOR. The file they were viewing may have been modified and
    /// the cached split lines would lie about line numbers and content.
    pub fn invalidate_preview_cache(&mut self) {
        self.preview_cache.clear();
        self.preview_order.clear();
        self.current_preview = None;
        self.current_preview_path = None;
    }

    fn pick_timeout(&self) -> Duration {
        let mut ms = TICK_TIMEOUT_MS;
        if let Some(t) = self.next_dispatch_at {
            let now = Instant::now();
            if t <= now {
                ms = 0;
            } else {
                ms = ms.min((t - now).as_millis() as u64);
            }
        }
        if self.searching {
            ms = ms.min(SEARCHING_TICK_MS);
        }
        if let Some((_, until)) = &self.status_msg {
            let now = Instant::now();
            if *until > now {
                ms = ms.min((*until - now).as_millis() as u64);
            }
        }
        Duration::from_millis(ms)
    }

    /// True when a search has been in flight long enough that flashing
    /// the spinner is worth it. Sub-`SPINNER_DELAY_MS` searches go
    /// invisible to the user — this kills the typing-time flicker.
    pub fn show_spinner(&self) -> bool {
        match (self.searching, self.search_started_at) {
            (true, Some(t)) => t.elapsed() >= Duration::from_millis(SPINNER_DELAY_MS),
            _ => false,
        }
    }

    /// Atomic "begin a new dispatch" — bumps seq, marks the in-flight
    /// flag, records the start timestamp, and clears any pending
    /// debounce. Single chokepoint so the spinner gate never gets out
    /// of sync with `searching`.
    fn begin_search(&mut self) {
        self.seq += 1;
        self.searching = true;
        self.search_started_at = Some(Instant::now());
        self.pending_query = false;
        self.next_dispatch_at = None;
    }

    fn end_search(&mut self) {
        self.searching = false;
        self.search_started_at = None;
    }

    /// Cancel any in-flight search and pending debounce. The worker
    /// can't be interrupted mid-search — its result will still arrive
    /// — but bumping `seq` past `displayed_seq` makes `apply_results`
    /// drop the reply on the floor.
    fn cancel_search(&mut self) {
        if !self.searching && !self.pending_query {
            self.set_status("nothing to cancel");
            return;
        }
        self.pending_query = false;
        self.next_dispatch_at = None;
        self.end_search();
        // Bump seq so the worker's in-flight reply (with the OLD seq)
        // gets discarded by apply_results.
        self.seq += 1;
        self.displayed_seq = self.seq;
        self.set_status("search cancelled");
    }

    /// Commit the current trimmed query to the recall ring buffer.
    /// Called from action sites where the user clearly meant the
    /// query to be "finalised" (Ctrl-R / D / F, Enter, Ctrl-O, Ctrl-U).
    ///
    /// Skips empties and consecutive duplicates so the recall list
    /// stays useful for Ctrl-Up scrolling.
    fn commit_query_history(&mut self) {
        let q = self.query.trim();
        if q.is_empty() {
            return;
        }
        if self.query_history.last().map(String::as_str) == Some(q) {
            return;
        }
        self.query_history.push(q.to_string());
        if self.query_history.len() > QUERY_HISTORY_CAP {
            self.query_history.remove(0);
        }
        self.query_history_idx = None;
    }

    /// Replace the query buffer with a recalled entry (or the saved
    /// draft) and kick off a search without going through
    /// `mark_query_changed` — the latter would reset `query_history_idx`
    /// and lose the user's position in the recall sequence.
    fn apply_recalled_query(&mut self, text: String) {
        self.cursor_chars = text.chars().count();
        self.query = text;
        // Drop any pending non-Query view snapshot — recalling is a
        // fresh query action and shouldn't accumulate stale history.
        self.pending_query = true;
        self.next_dispatch_at = Some(Instant::now());
    }

    fn history_prev(&mut self) {
        if self.query_history.is_empty() {
            self.set_status("query history is empty");
            return;
        }
        let new_idx = match self.query_history_idx {
            None => {
                // Entering history: snapshot whatever the user had
                // typed so Ctrl-Down can put it back.
                self.query_draft = self.query.clone();
                self.query_history.len() - 1
            }
            Some(0) => {
                self.set_status("oldest query");
                return;
            }
            Some(i) => i - 1,
        };
        self.query_history_idx = Some(new_idx);
        let text = self.query_history[new_idx].clone();
        self.apply_recalled_query(text);
    }

    fn history_next(&mut self) {
        let Some(i) = self.query_history_idx else {
            // Not in history mode → no-op (Down at "current draft").
            return;
        };
        if i + 1 >= self.query_history.len() {
            // Past the most recent entry → restore draft.
            self.query_history_idx = None;
            let draft = std::mem::take(&mut self.query_draft);
            self.apply_recalled_query(draft);
        } else {
            self.query_history_idx = Some(i + 1);
            let text = self.query_history[i + 1].clone();
            self.apply_recalled_query(text);
        }
    }

    /// True when result `idx` is a tree-sitter definition (rather than
    /// a BM25 / semantic hit). Used by the renderer to suppress the
    /// fake `score=1.000 source=bm25` badge that the worker stamps on
    /// symbol-derived rows.
    ///
    /// - `Defs` view: every row is a def.
    /// - `Refs` view: the first `def_count` rows are defs, the rest are
    ///   BM25 reference hits.
    /// - Other views: never defs.
    pub fn is_treesitter_row(&self, idx: usize) -> bool {
        match &self.results_kind {
            ResultsKind::Defs { .. } => idx < self.results.len(),
            ResultsKind::Refs { def_count, .. } => idx < *def_count,
            _ => false,
        }
    }

    // ── Worker plumbing ──────────────────────────────────────────────

    fn handle_worker_msg(&mut self, msg: WorkerMsg) {
        match msg {
            WorkerMsg::SearchDone(done) => self.apply_results(done),
        }
    }

    fn apply_results(&mut self, done: SearchDone) {
        // Discard out-of-order completions — the channel preserves order
        // but a Related dispatch interleaved with a Query can race, and
        // the seq counter is the source of truth.
        if done.seq < self.displayed_seq {
            return;
        }
        self.displayed_seq = done.seq;
        self.results = done.results;
        self.elapsed_ms = done.elapsed_ms;
        self.selected = 0;
        self.list_offset = 0;
        self.preview_scroll = 0;
        self.results_kind = match done.kind {
            ResultKind::Query => ResultsKind::Query { query: done.query },
            ResultKind::Related { anchor } => ResultsKind::Related { anchor },
            ResultKind::Defs { name } => ResultsKind::Defs { name },
            ResultKind::Refs { name, def_count } => ResultsKind::Refs { name, def_count },
        };
        if done.seq == self.seq {
            self.end_search();
        }
        // A fresh view is being displayed; the next navigation away from
        // it should snapshot it onto the back stack.
        self.current_view_pushed = false;
        self.rebuild_term_cache();
    }

    // ── Back / forward history ──────────────────────────────────────

    fn snapshot(&self) -> ViewSnapshot {
        ViewSnapshot {
            query: self.query.clone(),
            cursor_chars: self.cursor_chars,
            mode: self.mode,
            filter_lang: self.filter_lang.clone(),
            filter_path: self.filter_path.clone(),
            results: self.results.clone(),
            results_kind: self.results_kind.clone(),
            elapsed_ms: self.elapsed_ms,
            selected: self.selected,
            list_offset: self.list_offset,
            preview_scroll: self.preview_scroll,
        }
    }

    /// Push the currently displayed view onto the back stack if it
    /// hasn't been pushed already since its last apply/restore. Called
    /// from every outgoing navigation point (dispatch_related / defs /
    /// refs, and the first keystroke in a non-Query view).
    ///
    /// Skips the "empty Query with no results" pseudo-view that the TUI
    /// starts in — there's nothing useful to go back to.
    fn push_current(&mut self) {
        if self.current_view_pushed {
            return;
        }
        if self.results.is_empty()
            && matches!(&self.results_kind, ResultsKind::Query { query } if query.is_empty())
        {
            self.current_view_pushed = true;
            return;
        }
        let snap = self.snapshot();
        self.history_back.push(snap);
        if self.history_back.len() > HISTORY_CAP {
            self.history_back.remove(0);
        }
        // Any new branch invalidates the forward stack — same semantics
        // as a web browser.
        self.history_forward.clear();
        self.current_view_pushed = true;
    }

    fn restore(&mut self, snap: ViewSnapshot) {
        self.query = snap.query;
        self.cursor_chars = snap.cursor_chars;
        self.mode = snap.mode;
        self.filter_lang = snap.filter_lang;
        self.filter_path = snap.filter_path;
        // If the snapshot's path filter is empty, normalise the cursor so
        // re-entering edit mode starts at zero rather than dangling past
        // the end of the new (shorter) string.
        self.filter_path_cursor = self.filter_path.chars().count();
        self.editing_path_filter = false;
        self.results = snap.results;
        self.results_kind = snap.results_kind;
        self.elapsed_ms = snap.elapsed_ms;
        self.selected = snap.selected;
        self.list_offset = snap.list_offset;
        self.preview_scroll = snap.preview_scroll;
        // Cancel anything in flight; bump seq so any worker reply that
        // was already mid-air gets discarded by `apply_results`.
        self.pending_query = false;
        self.next_dispatch_at = None;
        self.end_search();
        self.seq += 1;
        self.displayed_seq = self.seq;
        // Force preview reload — the selected chunk likely points at a
        // different file than the previous view.
        self.current_preview_path = None;
        // The restored view is now displayed; subsequent navigation
        // should push it again.
        self.current_view_pushed = false;
        self.rebuild_term_cache();
    }

    fn go_back(&mut self) {
        let Some(snap) = self.history_back.pop() else {
            // Silent no-op feels like a broken keybind. Surface the
            // empty-stack case so the user knows the key WAS received
            // — they just haven't populated history yet (Ctrl-R / D / F
            // are the actions that push a snapshot).
            self.set_status("no history to go back to (try Ctrl-R / Ctrl-D / Ctrl-F first)");
            return;
        };
        let current = self.snapshot();
        self.history_forward.push(current);
        if self.history_forward.len() > HISTORY_CAP {
            self.history_forward.remove(0);
        }
        self.restore(snap);
        self.set_status(&format!("← back ({} more)", self.history_back.len()));
    }

    fn go_forward(&mut self) {
        let Some(snap) = self.history_forward.pop() else {
            self.set_status("no forward history");
            return;
        };
        let current = self.snapshot();
        self.history_back.push(current);
        if self.history_back.len() > HISTORY_CAP {
            self.history_back.remove(0);
        }
        self.restore(snap);
        self.set_status(&format!("→ forward ({} more)", self.history_forward.len()));
    }

    /// Set a transient status message shown in the bottom keys bar for
    /// ~2 seconds. Used to confirm actions and surface no-op edge cases
    /// (empty history, single indexed language, etc.) that would
    /// otherwise look like broken keybindings.
    fn set_status(&mut self, msg: &str) {
        self.status_msg = Some((msg.to_string(), Instant::now() + Duration::from_secs(2)));
    }

    fn maybe_dispatch_query(&mut self) -> Option<WorkerCmd> {
        if !self.pending_query {
            return None;
        }
        if let Some(t) = self.next_dispatch_at
            && Instant::now() < t
        {
            return None;
        }
        self.begin_search();
        Some(WorkerCmd::Search {
            seq: self.seq,
            query: self.query.clone(),
            mode: self.mode,
            top_k: TOP_K,
            filters: self.current_filters(),
        })
    }

    fn mark_query_changed(&mut self) {
        // If the user starts typing while a non-Query view is displayed,
        // record the current view in the back stack so Alt-Left can
        // return to it after the new Query results come in.
        if !matches!(self.results_kind, ResultsKind::Query { .. }) {
            self.push_current();
        }
        // Any edit (typing, backspace, etc) means the user is no longer
        // browsing recall history — they're authoring a new query.
        self.query_history_idx = None;
        self.pending_query = true;
        self.next_dispatch_at = Some(Instant::now() + Duration::from_millis(DEBOUNCE_MS));
    }

    fn dispatch_related(&mut self) {
        let Some(sel) = self.results.get(self.selected) else {
            return;
        };
        let chunk = sel.chunk.clone();
        self.commit_query_history();
        self.push_current();
        self.begin_search();
        let _ = self.cmd_tx.send(WorkerCmd::Related {
            seq: self.seq,
            source: Box::new(chunk),
            top_k: TOP_K,
            filters: self.current_filters(),
        });
    }

    fn dispatch_defs(&mut self) {
        let Some(name) = self.identifier_for_lookup() else {
            return;
        };
        self.commit_query_history();
        self.push_current();
        self.begin_search();
        let _ = self.cmd_tx.send(WorkerCmd::Defs {
            seq: self.seq,
            query: name,
            filters: self.current_filters(),
        });
    }

    fn dispatch_refs(&mut self) {
        let Some(name) = self.identifier_for_lookup() else {
            return;
        };
        self.commit_query_history();
        self.push_current();
        self.begin_search();
        let _ = self.cmd_tx.send(WorkerCmd::Refs {
            seq: self.seq,
            query: name,
            top_k: TOP_K,
            filters: self.current_filters(),
        });
    }

    /// Pick the identifier to look up for Ctrl-D / Ctrl-F:
    ///
    /// 1. A non-empty trimmed query wins — the user explicitly named
    ///    what they want.
    /// 2. Otherwise fall back to the symbol the *currently selected
    ///    result* shows or sits inside (tree-sitter defs first, then the
    ///    innermost enclosing symbol). This lets the user point at a
    ///    function in the results pane and hit Ctrl-F without retyping.
    ///
    /// Returns `None` only when both are unavailable.
    fn identifier_for_lookup(&self) -> Option<String> {
        let q = self.query.trim();
        if !q.is_empty() {
            return Some(q.to_string());
        }
        let sel = self.results.get(self.selected)?;
        let chunk = &sel.chunk;
        let symbols = self.index.symbols();
        // Tier 1: a symbol whose start_line falls inside the chunk —
        // typically the def the chunk is showing.
        if let Some(s) = symbols.iter().find(|s| {
            s.file_path == chunk.file_path
                && s.start_line >= chunk.start_line
                && s.start_line <= chunk.end_line
        }) {
            return Some(s.name.clone());
        }
        // Tier 2: the innermost symbol whose range *strictly contains*
        // the chunk's start line — the chunk is mid-body.
        symbols
            .iter()
            .filter(|s| {
                s.file_path == chunk.file_path
                    && s.start_line < chunk.start_line
                    && chunk.start_line <= s.end_line
            })
            .min_by_key(|s| s.end_line.saturating_sub(s.start_line))
            .map(|s| s.name.clone())
    }

    // ── Preview cache ────────────────────────────────────────────────

    fn refresh_preview(&mut self) {
        // Cheap-path check: compare path slices without allocating.
        // This runs every event-loop tick (60+ Hz when searching) and
        // returns early in the overwhelming majority of cases since
        // results-list navigation usually stays within one file.
        let new_path: Option<&str> = self
            .results
            .get(self.selected)
            .map(|r| r.chunk.file_path.as_str());
        if new_path == self.current_preview_path.as_deref() {
            return;
        }
        // Path actually changed — pay the clone for ownership and load.
        let owned = new_path.map(|s| s.to_string());
        self.current_preview = match &owned {
            Some(p) => self.load_file(p),
            None => None,
        };
        self.current_preview_path = owned;
        // Selection moved to a different file → previous match-line
        // anchor and preview-scroll offset don't apply.
        self.cached_match_line = None;
        self.cached_match_line_for = None;
    }

    /// Cache-or-read a preview file, returning its lines.
    ///
    /// Stays synchronous on purpose: every indexed file is ≤ 1MB
    /// (filtered at walk time — see `walker::MAX_FILE_BYTES`), so a
    /// cache miss is at most ~10ms on an SSD — well below 1 frame at
    /// 60fps. An async refactor here would add a worker thread + a
    /// channel + stale-load coalescing for a regression-class win.
    fn load_file(&mut self, rel: &str) -> Option<Arc<Vec<String>>> {
        if let Some(arc) = self.preview_cache.get(rel) {
            return Some(arc.clone());
        }
        let abs = self.repo_path.join(rel);
        let raw = std::fs::read_to_string(&abs).ok()?;
        let lines: Vec<String> = raw.lines().map(|s| s.to_string()).collect();
        let arc = Arc::new(lines);
        self.preview_cache.insert(rel.to_string(), arc.clone());
        self.preview_order.push_back(rel.to_string());
        while self.preview_order.len() > PREVIEW_FILE_CACHE {
            if let Some(old) = self.preview_order.pop_front() {
                self.preview_cache.remove(&old);
            }
        }
        Some(arc)
    }

    /// Insert pasted text into whichever input field has focus. Strips
    /// newlines and tabs so multi-line clipboard contents don't mangle
    /// the single-line input box; collapses runs of whitespace into
    /// single spaces to keep queries tidy.
    fn handle_paste(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        let normalised: String = text
            .chars()
            .map(|c| if c == '\n' || c == '\t' { ' ' } else { c })
            .collect();
        if self.editing_path_filter {
            for c in normalised.chars() {
                self.path_filter_insert_char(c);
            }
        } else {
            for c in normalised.chars() {
                self.insert_char(c);
            }
        }
    }

    // ── Key handling ─────────────────────────────────────────────────

    fn handle_key(&mut self, k: KeyEvent) {
        // Diagnostic: when key-debug mode is on, every press is echoed
        // into the status line so the user can see exactly what crossterm
        // received (helps with macOS Option-as-Meta / F-key issues).
        // Toggle with Ctrl-\.
        if self.key_debug {
            let mut mods = Vec::new();
            if k.modifiers.contains(KeyModifiers::CONTROL) {
                mods.push("Ctrl");
            }
            if k.modifiers.contains(KeyModifiers::SHIFT) {
                mods.push("Shift");
            }
            if k.modifiers.contains(KeyModifiers::ALT) {
                mods.push("Alt");
            }
            if k.modifiers.contains(KeyModifiers::SUPER) {
                mods.push("Super");
            }
            let mod_str = if mods.is_empty() {
                "none".into()
            } else {
                mods.join("+")
            };
            self.status_msg = Some((
                format!("key: {:?} mod: {}", k.code, mod_str),
                Instant::now() + Duration::from_secs(3),
            ));
        }
        if self.help_open {
            // Allow scrolling through the help content; any other key
            // dismisses. Scroll keys handle their own state and don't
            // close — keeps J/K muscle memory working.
            match k.code {
                KeyCode::Up | KeyCode::Char('k') => {
                    self.help_scroll = self.help_scroll.saturating_sub(1);
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    self.help_scroll = self.help_scroll.saturating_add(1);
                }
                KeyCode::PageUp => {
                    self.help_scroll = self.help_scroll.saturating_sub(10);
                }
                KeyCode::PageDown => {
                    self.help_scroll = self.help_scroll.saturating_add(10);
                }
                KeyCode::Home => self.help_scroll = 0,
                _ => {
                    self.help_open = false;
                    self.help_scroll = 0;
                }
            }
            return;
        }
        // While the user is editing the path-filter glob in the query
        // box, edit keys touch `filter_path` and Esc/Enter dismiss the
        // mode rather than quitting the TUI. Handled in its own arm so
        // the main key map stays self-contained.
        if self.editing_path_filter {
            self.handle_key_path_filter(k);
            return;
        }
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        let shift = k.modifiers.contains(KeyModifiers::SHIFT);
        let alt = k.modifiers.contains(KeyModifiers::ALT);
        match k.code {
            KeyCode::Esc => self.quit = true,
            KeyCode::Char('c') if ctrl => self.quit = true,
            // Ctrl-G follows readline/emacs convention: "abort current
            // operation" — here, cancel the in-flight search (if any)
            // and any pending debounced dispatch. Quit stays on Esc /
            // Ctrl-C.
            KeyCode::Char('g') if ctrl => self.cancel_search(),

            KeyCode::Enter => self.action_print(),
            KeyCode::Char('o') if ctrl => self.action_editor(),
            KeyCode::Char('r') if ctrl => self.dispatch_related(),
            KeyCode::Char('d') if ctrl => self.dispatch_defs(),
            KeyCode::Char('f') if ctrl => self.dispatch_refs(),

            // Filters.
            KeyCode::Char('t') if ctrl => self.cycle_lang_filter(),
            KeyCode::Char('y') if ctrl => self.enter_path_filter_edit(),

            // Back / forward history. Designed so at least one combo
            // works on every common terminal+platform without config:
            //   - Ctrl-B / Ctrl-X: plain Ctrl combos, work everywhere
            //     including default macOS Terminal.app — primary on Mac.
            //   - F2 / F3: nice on Linux; on macOS need Fn or the
            //     "Use F1, F2, etc. keys as standard function keys"
            //     System Settings toggle.
            //   - Alt-←/→ and Alt-h/l: Linux primary; macOS needs the
            //     terminal's "Option as Meta key" preference.
            // Order matters — Alt-guarded arms must precede the plain
            // cursor-movement arrows below.
            KeyCode::Char('b') if ctrl => self.go_back(),
            KeyCode::Char('x') if ctrl => self.go_forward(),
            KeyCode::Left if alt => self.go_back(),
            KeyCode::Right if alt => self.go_forward(),
            KeyCode::Char('h') if alt => self.go_back(),
            KeyCode::Char('l') if alt => self.go_forward(),
            KeyCode::F(2) => self.go_back(),
            KeyCode::F(3) => self.go_forward(),

            // Preview scroll — checked BEFORE plain Up/Down so the
            // shift-modified variants don't fall through to result
            // navigation. Useful when the chunk is bigger than the
            // viewport and you want to see the rest of it without
            // changing selection. F5/F6/F7/F8 are non-modifier
            // alternatives for terminals that swallow Shift+Arrow
            // (some macOS configurations).
            KeyCode::Up if shift => self.scroll_preview(-1),
            KeyCode::Down if shift => self.scroll_preview(1),
            KeyCode::PageUp if shift => self.scroll_preview(-10),
            KeyCode::PageDown if shift => self.scroll_preview(10),
            KeyCode::F(5) => self.scroll_preview(-1),
            KeyCode::F(6) => self.scroll_preview(1),
            KeyCode::F(7) => self.scroll_preview(-10),
            KeyCode::F(8) => self.scroll_preview(10),

            // Query history recall. Ctrl-↑/↓ are the bash/readline
            // analogue; Alt-P / Alt-N mirror Emacs-style for users
            // who can't or don't want to use Ctrl+Arrow. Checked
            // BEFORE plain Up/Down so the modifier variant wins.
            KeyCode::Up if ctrl => self.history_prev(),
            KeyCode::Down if ctrl => self.history_next(),
            KeyCode::Char('p') if alt => self.history_prev(),
            KeyCode::Char('n') if alt => self.history_next(),

            // Result navigation.
            KeyCode::Up => self.move_selected(-1),
            KeyCode::Down => self.move_selected(1),
            KeyCode::Char('p') if ctrl => self.move_selected(-1),
            KeyCode::Char('n') if ctrl => self.move_selected(1),
            KeyCode::PageUp => self.move_selected(-10),
            KeyCode::PageDown => self.move_selected(10),

            // Search mode.
            KeyCode::Tab => self.cycle_mode(false),
            KeyCode::BackTab => self.cycle_mode(true),

            // Help overlay. Only intercept '?' when the query is empty so
            // it remains a typeable character in non-trivial queries.
            KeyCode::Char('?') if !ctrl && self.query.is_empty() => self.help_open = true,

            // Query editing.
            KeyCode::Backspace => self.backspace(),
            KeyCode::Delete => self.delete_forward(),
            KeyCode::Left => self.cursor_left(ctrl),
            KeyCode::Right => self.cursor_right(ctrl),
            KeyCode::Home => self.cursor_chars = 0,
            KeyCode::End => self.cursor_chars = self.query.chars().count(),
            KeyCode::Char('a') if ctrl => self.cursor_chars = 0,
            KeyCode::Char('e') if ctrl => self.cursor_chars = self.query.chars().count(),
            KeyCode::Char('u') if ctrl => self.clear_query(),
            KeyCode::Char('w') if ctrl => self.delete_word_back(),
            KeyCode::Char('k') if ctrl => self.kill_to_end(),

            KeyCode::Char(c) if !ctrl => self.insert_char(c),
            _ => {}
        }
    }

    fn move_selected(&mut self, delta: i32) {
        if self.results.is_empty() {
            return;
        }
        let max = self.results.len() as i32 - 1;
        let new = (self.selected as i32 + delta).clamp(0, max);
        let changed = self.selected != new as usize;
        self.selected = new as usize;
        if changed {
            // New chunk → start from its match-centred default window.
            self.preview_scroll = 0;
        }
    }

    /// Scroll the preview pane up (`delta < 0`) or down (`delta > 0`) by
    /// `delta` lines. The clamp lives in `render_preview` so the cap
    /// adapts to the current viewport height and file length.
    fn scroll_preview(&mut self, delta: i32) {
        self.preview_scroll = self.preview_scroll.saturating_add(delta);
    }

    /// Adjust `list_offset` so `selected` stays inside the visible
    /// window. Called from `render_results` once the actual viewport
    /// height is known (it can only be computed after layout). Pure
    /// mutation, no I/O — kept on `App` so the render path doesn't
    /// have to touch state directly.
    pub fn clamp_list_offset(&mut self, viewport: usize) {
        if viewport == 0 {
            return;
        }
        if self.selected < self.list_offset {
            self.list_offset = self.selected;
        }
        if self.selected >= self.list_offset + viewport {
            self.list_offset = self.selected + 1 - viewport;
        }
    }

    fn cycle_mode(&mut self, reverse: bool) {
        self.mode = if reverse {
            match self.mode {
                SearchMode::Hybrid => SearchMode::Semantic,
                SearchMode::Semantic => SearchMode::Bm25,
                SearchMode::Bm25 => SearchMode::Hybrid,
            }
        } else {
            match self.mode {
                SearchMode::Hybrid => SearchMode::Bm25,
                SearchMode::Bm25 => SearchMode::Semantic,
                SearchMode::Semantic => SearchMode::Hybrid,
            }
        };
        // Re-run with the new mode immediately.
        self.mark_query_changed();
        self.next_dispatch_at = Some(Instant::now()); // skip debounce on Tab
    }

    fn insert_char(&mut self, c: char) {
        let byte = char_idx_to_byte(&self.query, self.cursor_chars);
        self.query.insert(byte, c);
        self.cursor_chars += 1;
        self.mark_query_changed();
    }

    fn backspace(&mut self) {
        if self.cursor_chars == 0 {
            return;
        }
        let to = char_idx_to_byte(&self.query, self.cursor_chars);
        let from = char_idx_to_byte(&self.query, self.cursor_chars - 1);
        self.query.replace_range(from..to, "");
        self.cursor_chars -= 1;
        self.mark_query_changed();
    }

    fn delete_forward(&mut self) {
        let total = self.query.chars().count();
        if self.cursor_chars >= total {
            return;
        }
        let from = char_idx_to_byte(&self.query, self.cursor_chars);
        let to = char_idx_to_byte(&self.query, self.cursor_chars + 1);
        self.query.replace_range(from..to, "");
        self.mark_query_changed();
    }

    fn cursor_left(&mut self, by_word: bool) {
        if self.cursor_chars == 0 {
            return;
        }
        if by_word {
            self.cursor_chars = word_boundary_left(&self.query, self.cursor_chars);
        } else {
            self.cursor_chars -= 1;
        }
    }

    fn cursor_right(&mut self, by_word: bool) {
        let total = self.query.chars().count();
        if self.cursor_chars >= total {
            return;
        }
        if by_word {
            self.cursor_chars = word_boundary_right(&self.query, self.cursor_chars, total);
        } else {
            self.cursor_chars += 1;
        }
    }

    fn clear_query(&mut self) {
        // Ctrl-U is the "I'm done with this one, starting fresh" gesture
        // — perfect commit point for recall history.
        self.commit_query_history();
        self.query.clear();
        self.cursor_chars = 0;
        self.mark_query_changed();
    }

    fn delete_word_back(&mut self) {
        if self.cursor_chars == 0 {
            return;
        }
        let target = word_boundary_left(&self.query, self.cursor_chars);
        let from = char_idx_to_byte(&self.query, target);
        let to = char_idx_to_byte(&self.query, self.cursor_chars);
        self.query.replace_range(from..to, "");
        self.cursor_chars = target;
        self.mark_query_changed();
    }

    fn kill_to_end(&mut self) {
        let total = self.query.chars().count();
        if self.cursor_chars >= total {
            return;
        }
        let from = char_idx_to_byte(&self.query, self.cursor_chars);
        self.query.truncate(from);
        self.mark_query_changed();
    }

    // ── Filters ──────────────────────────────────────────────────────

    /// Advance the language filter through the indexed languages.
    /// Sequence: None → langs[0] → langs[1] → ... → None. Re-runs the
    /// current view immediately so the user sees the new result set
    /// without an extra keystroke.
    fn cycle_lang_filter(&mut self) {
        if self.languages.is_empty() {
            self.set_status("no languages indexed");
            return;
        }
        let next = match &self.filter_lang {
            None => Some(self.languages[0].clone()),
            Some(cur) => {
                let idx = self.languages.iter().position(|l| l == cur).unwrap_or(0);
                if idx + 1 >= self.languages.len() {
                    None
                } else {
                    Some(self.languages[idx + 1].clone())
                }
            }
        };
        self.filter_lang = next.clone();
        // Make the cycle visible — without this, on a single-language
        // index it looks like Ctrl-T does nothing (the chip toggles
        // but the visual change is subtle).
        let msg = match (&next, self.languages.len()) {
            (Some(l), 1) => {
                format!("lang filter: {l} (only language indexed; --include-text-files for more)")
            }
            (Some(l), n) => format!("lang filter: {l} ({}/{})", self.cur_lang_idx() + 1, n),
            (None, _) => "lang filter: off".to_string(),
        };
        self.set_status(&msg);
        self.rerun_current_view();
    }

    fn cur_lang_idx(&self) -> usize {
        match &self.filter_lang {
            Some(cur) => self.languages.iter().position(|l| l == cur).unwrap_or(0),
            None => 0,
        }
    }

    fn enter_path_filter_edit(&mut self) {
        self.editing_path_filter = true;
        self.filter_path_cursor = self.filter_path.chars().count();
    }

    fn exit_path_filter_edit(&mut self) {
        self.editing_path_filter = false;
        self.rerun_current_view();
    }

    /// Re-issue whatever the current view is (Query / Related / Defs /
    /// Refs) so a filter change takes effect without the user having to
    /// retype or re-trigger by hand. Skips the debounce.
    fn rerun_current_view(&mut self) {
        match self.results_kind.clone() {
            ResultsKind::Query { .. } => {
                self.pending_query = true;
                self.next_dispatch_at = Some(Instant::now());
            }
            ResultsKind::Related { .. } => self.dispatch_related(),
            ResultsKind::Defs { .. } => self.dispatch_defs(),
            ResultsKind::Refs { .. } => self.dispatch_refs(),
        }
    }

    /// Key handler that runs while the user is editing the path-glob
    /// filter inside the query box. Mirrors the relevant edit arms from
    /// `handle_key` but routes them to `filter_path` and intercepts
    /// Esc/Enter so they dismiss the mode rather than quitting.
    fn handle_key_path_filter(&mut self, k: KeyEvent) {
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        match k.code {
            KeyCode::Esc | KeyCode::Enter => self.exit_path_filter_edit(),
            // Allow Ctrl-C / Ctrl-G to still quit from filter mode — same
            // muscle memory as the main view.
            KeyCode::Char('c') if ctrl => self.quit = true,
            KeyCode::Char('g') if ctrl => self.quit = true,
            KeyCode::Backspace => self.path_filter_backspace(),
            KeyCode::Delete => self.path_filter_delete_forward(),
            KeyCode::Left => self.path_filter_cursor_left(),
            KeyCode::Right => self.path_filter_cursor_right(),
            KeyCode::Home => self.filter_path_cursor = 0,
            KeyCode::End => self.filter_path_cursor = self.filter_path.chars().count(),
            KeyCode::Char('a') if ctrl => self.filter_path_cursor = 0,
            KeyCode::Char('e') if ctrl => {
                self.filter_path_cursor = self.filter_path.chars().count();
            }
            KeyCode::Char('u') if ctrl => {
                self.filter_path.clear();
                self.filter_path_cursor = 0;
            }
            KeyCode::Char(c) if !ctrl => self.path_filter_insert_char(c),
            _ => {}
        }
    }

    fn path_filter_insert_char(&mut self, c: char) {
        let byte = char_idx_to_byte(&self.filter_path, self.filter_path_cursor);
        self.filter_path.insert(byte, c);
        self.filter_path_cursor += 1;
    }

    fn path_filter_backspace(&mut self) {
        if self.filter_path_cursor == 0 {
            return;
        }
        let to = char_idx_to_byte(&self.filter_path, self.filter_path_cursor);
        let from = char_idx_to_byte(&self.filter_path, self.filter_path_cursor - 1);
        self.filter_path.replace_range(from..to, "");
        self.filter_path_cursor -= 1;
    }

    fn path_filter_delete_forward(&mut self) {
        let total = self.filter_path.chars().count();
        if self.filter_path_cursor >= total {
            return;
        }
        let from = char_idx_to_byte(&self.filter_path, self.filter_path_cursor);
        let to = char_idx_to_byte(&self.filter_path, self.filter_path_cursor + 1);
        self.filter_path.replace_range(from..to, "");
    }

    fn path_filter_cursor_left(&mut self) {
        if self.filter_path_cursor > 0 {
            self.filter_path_cursor -= 1;
        }
    }

    fn path_filter_cursor_right(&mut self) {
        let total = self.filter_path.chars().count();
        if self.filter_path_cursor < total {
            self.filter_path_cursor += 1;
        }
    }

    fn action_print(&mut self) {
        let Some(sel) = self.results.get(self.selected) else {
            return;
        };
        let s = format!("{}:{}", sel.chunk.file_path, sel.chunk.start_line);
        self.commit_query_history();
        self.exit_action = Some(ExitAction::Print(s));
        self.quit = true;
    }

    fn action_editor(&mut self) {
        let Some(sel) = self.results.get(self.selected) else {
            return;
        };
        let abs = self.repo_path.join(&sel.chunk.file_path);
        let line = sel.chunk.start_line;
        self.commit_query_history();
        // Signal the outer driver to suspend the terminal, run $EDITOR,
        // and re-enter `run`. We do NOT set `quit` — the user expects
        // to come back to the same query / results after editing.
        self.pending_editor = Some((abs, line));
    }

    /// Cached query terms in original case (for label / status).
    pub fn query_terms(&self) -> &[String] {
        &self.cached_terms
    }

    /// Cached lowercased query terms. Used by `find_match_line` and
    /// `highlight_terms` so they don't re-allocate the lowercase Vec
    /// on every visible preview line.
    pub fn lower_terms(&self) -> &[String] {
        &self.cached_lower_terms
    }

    /// Rebuild the term caches from `results_kind`. Cheap (small Vec
    /// of small Strings) — called whenever a new view is applied
    /// (apply_results, restore). Keeps the per-frame renderer free of
    /// repeated splits / lowercase conversions.
    fn rebuild_term_cache(&mut self) {
        let (terms, lower): (Vec<String>, Vec<String>) = match &self.results_kind {
            ResultsKind::Query { query } => {
                let t: Vec<String> = query
                    .split_whitespace()
                    .filter(|s| !s.is_empty())
                    .map(|s| s.to_string())
                    .collect();
                let l = t.iter().map(|s| s.to_ascii_lowercase()).collect();
                (t, l)
            }
            ResultsKind::Defs { name } | ResultsKind::Refs { name, .. } => {
                if name.is_empty() {
                    (Vec::new(), Vec::new())
                } else {
                    (vec![name.clone()], vec![name.to_ascii_lowercase()])
                }
            }
            ResultsKind::Related { .. } => (Vec::new(), Vec::new()),
        };
        self.cached_terms = terms;
        self.cached_lower_terms = lower;
        // Terms changed → the per-selection match-line anchor is stale.
        self.cached_match_line = None;
        self.cached_match_line_for = None;
    }

    /// Cached anchor — the first line inside the selected chunk that
    /// contains any cached_lower_term. `None` for empty term sets
    /// (Related view) or when no match exists. The cache is invalidated
    /// on selection change, term-cache rebuild, and preview file load.
    pub fn match_line_for_selection(&mut self) -> Option<usize> {
        let chunk = self.results.get(self.selected)?.chunk.clone();
        let key = (self.selected, chunk.file_path.clone());
        if self.cached_match_line_for.as_ref() == Some(&key) {
            return self.cached_match_line;
        }
        let file_lines = self.current_preview.clone();
        let anchor = match file_lines {
            Some(lines) if !self.cached_lower_terms.is_empty() => {
                let last = chunk.end_line.min(lines.len());
                let mut found = None;
                for ln in chunk.start_line..=last {
                    let Some(content) = lines.get(ln - 1) else {
                        break;
                    };
                    let lower = content.to_ascii_lowercase();
                    if self.cached_lower_terms.iter().any(|t| lower.contains(t)) {
                        found = Some(ln);
                        break;
                    }
                }
                found
            }
            _ => None,
        };
        self.cached_match_line = anchor;
        self.cached_match_line_for = Some(key);
        anchor
    }
}

fn char_idx_to_byte(s: &str, idx: usize) -> usize {
    s.char_indices().nth(idx).map(|(b, _)| b).unwrap_or(s.len())
}

/// Render the path as `~/...` when it starts with `$HOME`. Same logic
/// the previous `ui::repo_short` used; lifted here so it can be
/// computed once at App construction time.
fn shorten_with_home(p: &std::path::Path) -> String {
    let s = p.display().to_string();
    if let Some(home) = std::env::var_os("HOME") {
        let home = home.to_string_lossy().into_owned();
        if let Some(rest) = s.strip_prefix(&home) {
            return format!("~{rest}");
        }
    }
    s
}

/// Walk left from `cursor_chars` to the start of the current word.
///
/// Skips trailing whitespace first, then non-whitespace, matching
/// readline `Meta-b` semantics. Equivalent to the prior `Vec<char>`
/// implementation but allocation-free: forward-scan the prefix and
/// record the start of every word transition; the final value is the
/// nearest word-start at or before the cursor.
fn word_boundary_left(s: &str, cursor_chars: usize) -> usize {
    if cursor_chars == 0 {
        return 0;
    }
    let mut last_word_start = 0usize;
    let mut prev_was_ws = true;
    for (i, c) in s.chars().take(cursor_chars).enumerate() {
        if prev_was_ws && !c.is_whitespace() {
            last_word_start = i;
        }
        prev_was_ws = c.is_whitespace();
    }
    last_word_start
}

/// Walk right from `cursor_chars` to the start of the next word.
///
/// Skips whitespace, then non-whitespace, matching readline `Meta-f`
/// semantics. Uses a streaming iterator and bails when it crosses the
/// cursor's target, so a long query doesn't pay full-string walks.
fn word_boundary_right(s: &str, cursor_chars: usize, total: usize) -> usize {
    if cursor_chars >= total {
        return cursor_chars;
    }
    let mut iter = s.chars().skip(cursor_chars);
    let mut i = cursor_chars;
    // Skip whitespace.
    while let Some(c) = iter.next() {
        if !c.is_whitespace() {
            // Already consumed one non-whitespace char — count it and
            // fall through into the non-whitespace skip loop.
            i += 1;
            for c2 in iter.by_ref() {
                if c2.is_whitespace() {
                    return i;
                }
                i += 1;
            }
            return i;
        }
        i += 1;
    }
    i
}

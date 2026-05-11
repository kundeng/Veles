//! Background search worker for the TUI.
//!
//! Runs on its own OS thread and owns an `Arc<VelesIndex>`. The UI thread
//! posts `WorkerCmd::Search { gen, ... }` whenever the query changes; the
//! worker drains any backlog and only services the most recent command,
//! so a fast typist doesn't queue up dozens of doomed searches behind a
//! 50ms one. Each result carries the `gen` it originated from, and the UI
//! discards anything older than the latest dispatched generation.

use std::sync::Arc;
use std::time::Instant;

use crossbeam_channel::{Receiver, Sender};
use veles_core::VelesIndex;
use veles_core::filter::resolve_path_filter;
use veles_core::symbols::Symbol;
use veles_core::types::{Chunk, SearchMode, SearchResult};

/// Optional language / path filters carried with every dispatch.
///
/// `None` for both means "no filter" — the historical default.
/// `Some(lang)` restricts to chunks tagged with that language;
/// `Some(glob)` restricts to file paths matching the glob (matched by
/// the core selector pipeline).
#[derive(Clone, Default)]
pub struct Filters {
    pub lang: Option<String>,
    pub path: Option<String>,
}

impl Filters {
    /// Wrap the single language into a 1-element `Vec<String>` so it
    /// matches the slice-shaped API of `VelesIndex::search`.
    pub fn lang_slice(&self) -> Option<Vec<String>> {
        self.lang.as_ref().map(|s| vec![s.clone()])
    }
}

pub enum WorkerCmd {
    Search {
        seq: u64,
        query: String,
        mode: SearchMode,
        top_k: usize,
        filters: Filters,
    },
    Related {
        seq: u64,
        source: Box<Chunk>,
        top_k: usize,
        filters: Filters,
    },
    /// Look up tree-sitter definitions whose name equals `query`. Filters
    /// are applied as a post-pass since defs come from the symbol table,
    /// not the core search pipeline.
    Defs {
        seq: u64,
        query: String,
        filters: Filters,
    },
    /// Definitions + BM25 reference hits for `query`, with reference chunks
    /// that overlap a definition site filtered out.
    Refs {
        seq: u64,
        query: String,
        top_k: usize,
        filters: Filters,
    },
    Shutdown,
}

pub struct SearchDone {
    pub seq: u64,
    pub query: String,
    pub results: Vec<SearchResult>,
    pub elapsed_ms: u64,
    pub kind: ResultKind,
}

pub enum ResultKind {
    Query,
    Related { anchor: String },
    Defs { name: String },
    Refs { name: String, def_count: usize },
}

pub enum WorkerMsg {
    SearchDone(SearchDone),
}

pub fn spawn_worker(
    index: Arc<VelesIndex>,
    cmd_rx: Receiver<WorkerCmd>,
    msg_tx: Sender<WorkerMsg>,
) -> std::thread::JoinHandle<()> {
    std::thread::Builder::new()
        .name("veles-tui-search".into())
        .spawn(move || worker_loop(index, cmd_rx, msg_tx))
        .expect("spawn veles-tui-search thread")
}

fn worker_loop(index: Arc<VelesIndex>, cmd_rx: Receiver<WorkerCmd>, msg_tx: Sender<WorkerMsg>) {
    while let Ok(mut cmd) = cmd_rx.recv() {
        // Coalesce: pull any commands queued behind us and keep only the
        // newest. A `Shutdown` always wins so the worker tears down quickly.
        loop {
            match cmd_rx.try_recv() {
                Ok(WorkerCmd::Shutdown) => return,
                Ok(newer) => cmd = newer,
                Err(_) => break,
            }
        }

        match cmd {
            WorkerCmd::Shutdown => return,
            WorkerCmd::Search {
                seq,
                query,
                mode,
                top_k,
                filters,
            } => {
                let started = Instant::now();
                let resolved = ResolvedFilters::resolve(&index, &filters);
                let results = if query.trim().is_empty() {
                    Vec::new()
                } else {
                    index.search(
                        &query,
                        top_k,
                        mode,
                        None,
                        resolved.lang_slice(),
                        resolved.path_slice(),
                    )
                };
                let elapsed_ms = started.elapsed().as_millis() as u64;
                let _ = msg_tx.send(WorkerMsg::SearchDone(SearchDone {
                    seq,
                    query,
                    results,
                    elapsed_ms,
                    kind: ResultKind::Query,
                }));
            }
            WorkerCmd::Related {
                seq,
                source,
                top_k,
                filters,
            } => {
                let started = Instant::now();
                let resolved = ResolvedFilters::resolve(&index, &filters);
                let anchor = format!("{}:{}", source.file_path, source.start_line);
                let results = index.find_related(
                    &source,
                    top_k,
                    resolved.lang_slice(),
                    resolved.path_slice(),
                );
                let elapsed_ms = started.elapsed().as_millis() as u64;
                let _ = msg_tx.send(WorkerMsg::SearchDone(SearchDone {
                    seq,
                    query: String::new(),
                    results,
                    elapsed_ms,
                    kind: ResultKind::Related { anchor },
                }));
            }
            WorkerCmd::Defs {
                seq,
                query,
                filters,
            } => {
                let started = Instant::now();
                let resolved = ResolvedFilters::resolve(&index, &filters);
                let name = query.trim().to_string();
                let results = if name.is_empty() {
                    Vec::new()
                } else {
                    collect_defs(&index, &name, &resolved)
                };
                let elapsed_ms = started.elapsed().as_millis() as u64;
                let _ = msg_tx.send(WorkerMsg::SearchDone(SearchDone {
                    seq,
                    query: name.clone(),
                    results,
                    elapsed_ms,
                    kind: ResultKind::Defs { name },
                }));
            }
            WorkerCmd::Refs {
                seq,
                query,
                top_k,
                filters,
            } => {
                let started = Instant::now();
                let resolved = ResolvedFilters::resolve(&index, &filters);
                let name = query.trim().to_string();
                let (results, def_count) = if name.is_empty() {
                    (Vec::new(), 0)
                } else {
                    collect_refs(&index, &name, top_k, &resolved)
                };
                let elapsed_ms = started.elapsed().as_millis() as u64;
                let _ = msg_tx.send(WorkerMsg::SearchDone(SearchDone {
                    seq,
                    query: name.clone(),
                    results,
                    elapsed_ms,
                    kind: ResultKind::Refs { name, def_count },
                }));
            }
        }
    }
}

/// Filters resolved against the live index. Holds the concrete list of
/// file paths matching the user's path glob (so per-symbol post-filters
/// can do an O(log N) membership check instead of re-globbing each
/// path), and the language string repeated as a 1-element slice so it
/// can be handed to `index.search` / `find_related` as-is.
///
/// Created per-dispatch in the worker; cheap because the heaviest path
/// (`resolve_path_filter`) only runs when a glob is actually set.
struct ResolvedFilters {
    lang: Option<Vec<String>>,
    /// Matched indexed paths, sorted, deduped. `None` ⇒ no path filter.
    paths: Option<Vec<String>>,
}

impl ResolvedFilters {
    fn resolve(index: &VelesIndex, filters: &Filters) -> Self {
        let lang = filters.lang_slice();
        // resolve_path_filter bails when nothing matches; we treat that
        // as "filter excludes everything" silently — the UI surfaces the
        // (no results) state anyway, and an error here would just drop
        // the dispatch with no user feedback.
        let paths = filters.path.as_ref().and_then(|g| {
            resolve_path_filter(index, std::slice::from_ref(g), &[])
                .ok()
                .flatten()
        });
        Self { lang, paths }
    }
    fn lang_slice(&self) -> Option<&[String]> {
        self.lang.as_deref()
    }
    fn path_slice(&self) -> Option<&[String]> {
        self.paths.as_deref()
    }
    /// True when no filter is set, or the path is in the resolved set.
    /// Used for post-filtering symbol-driven results (Defs/Refs) since
    /// those don't flow through `index.search`'s selector pipeline.
    fn allows_path(&self, p: &str) -> bool {
        match &self.paths {
            Some(list) => list.binary_search_by(|x| x.as_str().cmp(p)).is_ok(),
            None => true,
        }
    }
    fn allows_lang(&self, lang: &str) -> bool {
        match &self.lang {
            Some(list) => list.iter().any(|l| l == lang),
            None => true,
        }
    }
}

/// Materialise a tree-sitter [`Symbol`] as a [`SearchResult`] so it can flow
/// through the same rendering pipeline as search hits. The preview pane
/// loads its actual content from disk via `App::load_file`, so an empty
/// `content` here is fine — `chunk_scope_label` only looks at the symbol
/// table and line ranges, both of which are populated.
fn symbol_to_result(s: &Symbol) -> SearchResult {
    SearchResult {
        chunk: Chunk {
            content: String::new(),
            file_path: s.file_path.clone(),
            start_line: s.start_line,
            end_line: s.end_line,
            language: Some(s.language.clone()),
        },
        score: 1.0,
        source: SearchMode::Bm25,
    }
}

fn collect_defs(index: &VelesIndex, name: &str, filters: &ResolvedFilters) -> Vec<SearchResult> {
    let mut hits: Vec<&Symbol> = index
        .symbols()
        .iter()
        .filter(|s| s.name == name)
        .filter(|s| filters.allows_lang(&s.language) && filters.allows_path(&s.file_path))
        .collect();
    hits.sort_by(|a, b| {
        a.file_path
            .cmp(&b.file_path)
            .then(a.start_line.cmp(&b.start_line))
    });
    hits.iter().map(|s| symbol_to_result(s)).collect()
}

fn collect_refs(
    index: &VelesIndex,
    name: &str,
    top_k: usize,
    filters: &ResolvedFilters,
) -> (Vec<SearchResult>, usize) {
    let mut defs: Vec<&Symbol> = index
        .symbols()
        .iter()
        .filter(|s| s.name == name)
        .filter(|s| filters.allows_lang(&s.language) && filters.allows_path(&s.file_path))
        .collect();
    defs.sort_by(|a, b| {
        a.file_path
            .cmp(&b.file_path)
            .then(a.start_line.cmp(&b.start_line))
    });

    // Pull a few extra BM25 hits so dropping chunks that overlap a definition
    // site still leaves the caller with roughly the requested count. Matches
    // the overshoot used by the CLI's `refs` handler.
    let bm25_overshoot = top_k + (top_k / 2).max(1);
    let bm25_hits: Vec<SearchResult> = index
        .search(
            name,
            bm25_overshoot,
            SearchMode::Bm25,
            None,
            filters.lang_slice(),
            filters.path_slice(),
        )
        .into_iter()
        .filter(|hit| {
            !defs.iter().any(|d| {
                d.file_path == hit.chunk.file_path
                    && d.start_line >= hit.chunk.start_line
                    && d.start_line <= hit.chunk.end_line
            })
        })
        .take(top_k)
        .collect();

    let mut results: Vec<SearchResult> = defs.iter().map(|s| symbol_to_result(s)).collect();
    let def_count = results.len();
    results.extend(bm25_hits);
    (results, def_count)
}

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
use veles_core::symbols::Symbol;
use veles_core::types::{Chunk, SearchMode, SearchResult};

pub enum WorkerCmd {
    Search {
        seq: u64,
        query: String,
        mode: SearchMode,
        top_k: usize,
    },
    Related {
        seq: u64,
        source: Box<Chunk>,
        top_k: usize,
    },
    /// Look up tree-sitter definitions whose name equals `query`.
    Defs {
        seq: u64,
        query: String,
    },
    /// Definitions + BM25 reference hits for `query`, with reference chunks
    /// that overlap a definition site filtered out.
    Refs {
        seq: u64,
        query: String,
        top_k: usize,
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
            } => {
                let started = Instant::now();
                let results = if query.trim().is_empty() {
                    Vec::new()
                } else {
                    index.search(&query, top_k, mode, None, None, None)
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
            WorkerCmd::Related { seq, source, top_k } => {
                let started = Instant::now();
                let anchor = format!("{}:{}", source.file_path, source.start_line);
                let results = index.find_related(&source, top_k, None, None);
                let elapsed_ms = started.elapsed().as_millis() as u64;
                let _ = msg_tx.send(WorkerMsg::SearchDone(SearchDone {
                    seq,
                    query: String::new(),
                    results,
                    elapsed_ms,
                    kind: ResultKind::Related { anchor },
                }));
            }
            WorkerCmd::Defs { seq, query } => {
                let started = Instant::now();
                let name = query.trim().to_string();
                let results = if name.is_empty() {
                    Vec::new()
                } else {
                    collect_defs(&index, &name)
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
            WorkerCmd::Refs { seq, query, top_k } => {
                let started = Instant::now();
                let name = query.trim().to_string();
                let (results, def_count) = if name.is_empty() {
                    (Vec::new(), 0)
                } else {
                    collect_refs(&index, &name, top_k)
                };
                let elapsed_ms = started.elapsed().as_millis() as u64;
                let _ = msg_tx.send(WorkerMsg::SearchDone(SearchDone {
                    seq,
                    query: name.clone(),
                    results,
                    elapsed_ms,
                    kind: ResultKind::Refs {
                        name,
                        def_count,
                    },
                }));
            }
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

fn collect_defs(index: &VelesIndex, name: &str) -> Vec<SearchResult> {
    let mut hits: Vec<&Symbol> = index.symbols().iter().filter(|s| s.name == name).collect();
    hits.sort_by(|a, b| {
        a.file_path
            .cmp(&b.file_path)
            .then(a.start_line.cmp(&b.start_line))
    });
    hits.iter().map(|s| symbol_to_result(s)).collect()
}

fn collect_refs(index: &VelesIndex, name: &str, top_k: usize) -> (Vec<SearchResult>, usize) {
    let mut defs: Vec<&Symbol> = index.symbols().iter().filter(|s| s.name == name).collect();
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
        .search(name, bm25_overshoot, SearchMode::Bm25, None, None, None)
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

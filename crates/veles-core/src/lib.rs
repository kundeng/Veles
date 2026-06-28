//! `veles-core` — fast, hybrid (BM25 + semantic) local code search.
//!
//! `veles-core` is the indexing and search engine that powers the [Veles]
//! CLI, MCP server, and gRPC service. It walks a directory, chunks source
//! files, builds a BM25 inverted index plus a dense
//! [`model2vec-rs`][model2vec] embedding index, and serves hybrid queries
//! using Reciprocal Rank Fusion. Tree-sitter is used to extract
//! definitions for symbol-level lookups.
//!
//! Design goals:
//!
//! - **No GPU, no transformer forward pass at query time.** Embeddings
//!   come from a static [model2vec] model, so query latency stays in
//!   the tens of milliseconds on CPU.
//! - **Persistent on-disk index.** Indexes live under `<repo>/.veles/`
//!   and support incremental updates that reuse embeddings of unchanged
//!   files.
//! - **Pure Rust.** No Python interpreter, no protobuf compiler, no
//!   native ML runtime — `cargo build --release` is enough.
//!
//! # Quick start
//!
//! ```no_run
//! use std::path::Path;
//! use veles_core::{SearchMode, VelesIndex};
//!
//! # fn main() -> anyhow::Result<()> {
//! // Build an index from a directory. The first call downloads the
//! // default embedding model (~64 MB) into the HuggingFace cache.
//! let index = VelesIndex::from_path(Path::new("."), None, None, false)?;
//!
//! // Hybrid (BM25 + semantic) search — the default for most queries.
//! let results = index.search(
//!     "parse config file",
//!     5,
//!     SearchMode::Hybrid,
//!     None,  // alpha — auto-detect from query type
//!     None,  // language filter
//!     None,  // path filter
//! );
//!
//! for r in results {
//!     println!("{} [{:.3}]", r.chunk.location(), r.score);
//! }
//! # Ok(())
//! # }
//! ```
//!
//! # Persistence
//!
//! Indexes can be saved to and loaded from `<repo>/.veles/`:
//!
//! ```no_run
//! # use std::path::Path;
//! # use veles_core::VelesIndex;
//! # fn main() -> anyhow::Result<()> {
//! let repo = Path::new(".");
//! let index = VelesIndex::from_path(repo, None, None, false)?;
//! index.save(repo)?;
//!
//! // Later, reload without re-embedding:
//! let model = veles_core::model::load_model(None)?;
//! let mut reloaded = VelesIndex::load(repo, model)?;
//!
//! // Refresh files that changed on disk; unchanged files keep their
//! // embeddings. Bare `touch` (mtime drift, identical bytes) is a
//! // manifest-only refresh via the BLAKE3 content_hash fallback.
//! let report = reloaded.update_from_path(repo)?;
//! eprintln!("{} added, {} modified, {} removed, {} mtime-only",
//!     report.added_files, report.modified_files,
//!     report.removed_files, report.mtime_refreshed_files);
//! # Ok(())
//! # }
//! ```
//!
//! # Module overview
//!
//! - [`veles_index`] — the main [`VelesIndex`] type combining BM25, dense,
//!   symbols, and persistence.
//! - [`chunker`] — line-based source chunking with overlap.
//! - [`tokenizer`] — identifier-aware tokeniser (camelCase, snake_case,
//!   Cyrillic, CJK).
//! - [`index`] — sparse ([`index::sparse`]) and dense
//!   ([`index::dense`]) indexes, [`index::search`] entry points, and
//!   [`index::topk`] selection.
//! - [`ranking`] — query-type detection, definition boosts, file-path
//!   penalties, file-saturation decay.
//! - [`symbols`] — tree-sitter symbol extraction for Rust, Python,
//!   JavaScript, TypeScript, and Go.
//! - [`persist`] — on-disk format under `.veles/`.
//! - [`walker`] — `.gitignore`-aware file walker (built on
//!   [`ignore`]).
//! - [`model`] — wrapper around [`model2vec-rs`][model2vec] for loading
//!   the default and multilingual static embedding models.
//!
//! [Veles]: https://github.com/julymetodiev/Veles
//! [model2vec]: https://github.com/MinishLab/model2vec-rs

pub mod cache;
pub mod chunker;
pub mod distill;
pub mod filter;
pub mod format;
pub mod index;
pub mod ingest;
pub mod lease;
pub mod lock;
pub mod model;
pub mod persist;
pub mod pipeline;
pub mod ranking;
/// Optional transformer reranker — only built with `--features rerank`.
#[cfg(feature = "rerank")]
pub mod rerank;
pub mod runtime;
pub mod scope;
pub mod symbols;
pub mod tokenizer;
pub mod types;
pub mod veles_index;
pub mod walker;

// Re-export the main types.
pub use types::{Chunk, IndexStats, SearchMode, SearchResult};
pub use veles_index::VelesIndex;

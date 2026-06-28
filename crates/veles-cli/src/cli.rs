//! Command-line interface definition (clap derives).
//!
//! Kept separate from `main.rs` so the actual dispatch loop stays small and
//! handlers can import a single `Commands` enum without pulling in the
//! handler logic.

use clap::{Parser, Subcommand};
use clap_complete::Shell;

#[derive(Parser)]
#[command(name = "veles")]
#[command(about = "Fast and Accurate Code Search for Agents")]
#[command(long_about = "\
Veles is a fast hybrid (BM25 + semantic) local code search engine for AI \
agents and humans. It runs entirely on CPU using static embeddings via \
model2vec-rs, maintains a persistent on-disk index under <repo>/.veles/, \
and exposes its functionality through a CLI, an MCP stdio server, and a \
gRPC service.

Typical workflow:

  veles index .                      # build & save the index
  veles search 'parse config file'   # hybrid search reuses the cache
  veles update .                     # incremental refresh after edits

Symbol-aware lookups (Rust, Python, JavaScript, TypeScript, Go):

  veles symbols src/main.rs          # outline a single file
  veles defs Manifest                # find every definition
  veles refs save_index              # defs + BM25 references

Output formats (shared by the CLI and the MCP/gRPC surfaces): pretty, compact, \
ripgrep, locations, paths, json, jsonl. `search`/`find-related` default to \
`auto` — pretty in a terminal, compact when piped.

Exit codes: 0 ok · 1 error · 2 usage · 3 not found. `index`/`update`/`add` and \
searches are idempotent — safe to re-run.
Run `veles <SUBCOMMAND> --help` for per-subcommand details.")]
#[command(version)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Commands>,
}

#[derive(Subcommand)]
pub enum Commands {
    /// Search a codebase with a natural-language or code query.
    Search {
        /// Natural language or code query.
        query: String,
        /// Local path or git URL (default: current directory).
        #[arg(default_value = ".")]
        path: String,
        /// Number of results.
        #[arg(short, long, default_value = "5")]
        top_k: usize,
        /// Search mode.
        #[arg(short, long, default_value = "hybrid")]
        mode: String,
        /// Output format. Default `auto`: pretty in a terminal, compact when
        /// piped/redirected. Other values: pretty, compact, ripgrep, locations,
        /// paths, json, jsonl (shared with the MCP `format` arg).
        #[arg(short, long, default_value = "auto")]
        format: String,
        /// Restrict to specific languages (comma-separated, e.g. `rust,python`).
        #[arg(short = 'l', long, value_delimiter = ',')]
        lang: Vec<String>,
        /// Glob pattern of paths to include (repeatable, e.g. `--path 'src/**/*.rs'`).
        #[arg(short = 'g', long = "path", value_name = "GLOB")]
        path_glob: Vec<String>,
        /// Glob pattern of paths to exclude (repeatable, e.g. `--exclude 'tests/**'`).
        #[arg(short = 'x', long = "exclude", value_name = "GLOB")]
        exclude_glob: Vec<String>,
        /// Drop results scoring below this threshold.
        #[arg(long)]
        min_score: Option<f64>,
        /// Use the multilingual embedding model (potion-multilingual-128M)
        /// instead of the default English/code-focused model. Recommended for
        /// codebases or queries containing Cyrillic, CJK, Greek, Arabic, etc.
        #[arg(long)]
        multilingual: bool,
        /// Force a fresh in-memory build, ignoring any `.veles/` cache.
        #[arg(long)]
        no_cache: bool,
        /// Re-rank the recall set with a transformer (bge-small-en-v1.5) for a
        /// precision boost. Requires a build with `--features rerank`; auto-uses
        /// the GPU when present.
        #[arg(long)]
        rerank: bool,
        /// Recall depth feeding the reranker (how many candidates the cheap
        /// stage pulls before re-scoring). Only used with `--rerank`.
        #[arg(long, default_value = "50")]
        rerank_k: usize,
    },

    /// Find code similar to a specific location.
    FindRelated {
        /// File path as shown in search results.
        file_path: String,
        /// Line number (1-indexed).
        line: usize,
        /// Local path or git URL (default: current directory).
        #[arg(default_value = ".")]
        path: String,
        /// Number of similar chunks to return.
        #[arg(short, long, default_value = "5")]
        top_k: usize,
        /// Output format. Default `auto`: pretty in a terminal, compact when
        /// piped/redirected. Other values: pretty, compact, ripgrep, locations,
        /// paths, json, jsonl (shared with the MCP `format` arg).
        #[arg(short, long, default_value = "auto")]
        format: String,
        /// Restrict results to these languages (repeatable, e.g. `-l rust -l python`).
        /// Overrides the default of "same language as the source chunk".
        #[arg(short, long, value_name = "LANG", value_delimiter = ',')]
        lang: Vec<String>,
        /// Glob pattern of paths to include (repeatable, e.g. `--path 'src/**/*.rs'`).
        #[arg(short = 'g', long = "path", value_name = "GLOB")]
        path_glob: Vec<String>,
        /// Glob pattern of paths to exclude (repeatable, e.g. `--exclude 'tests/**'`).
        #[arg(short = 'x', long = "exclude", value_name = "GLOB")]
        exclude_glob: Vec<String>,
        /// Drop results scoring below this threshold.
        #[arg(long)]
        min_score: Option<f64>,
        /// Use the multilingual embedding model.
        #[arg(long)]
        multilingual: bool,
        /// Force a fresh in-memory build, ignoring any `.veles/` cache.
        #[arg(long)]
        no_cache: bool,
    },

    /// Build the index and persist it to `<path>/.veles/`.
    Index {
        /// Local path to index (default: current directory).
        #[arg(default_value = ".")]
        path: String,
        /// Also index non-code text files.
        #[arg(long)]
        include_text_files: bool,
        /// Use the multilingual embedding model.
        #[arg(long)]
        multilingual: bool,
        /// Rebuild from scratch even if a `.veles/` cache already exists.
        #[arg(long)]
        force: bool,
    },

    /// Incrementally update an existing index for files that changed on disk.
    Update {
        /// Local path of the indexed repo (default: current directory).
        #[arg(default_value = ".")]
        path: String,
        /// Use the multilingual embedding model (must match how it was built).
        #[arg(long)]
        multilingual: bool,
    },

    /// Run an ingest pipeline once: for each stage, (re)derive changed
    /// sources via their external transform commands and (incrementally)
    /// index the destination. Idempotent and lock-guarded — safe to re-run.
    ///
    /// veles stays format-blind: the transform command owns all knowledge of
    /// the source format and emits indexable text on stdout.
    Transform {
        /// Path to a pipeline config (JSON). See `veles.pipeline.json`.
        #[arg(default_value = "veles.pipeline.json")]
        config: String,
    },

    /// Add a folder to a workspace's read-set so it joins searches — live, no
    /// restart. The folder gets its own coordinator (started now); a
    /// verbose-JSON folder (e.g. agent transcripts) is distilled automatically.
    /// Equivalent to the dashboard's "add repo" button or hand-editing
    /// `<workspace>/.veles/config.toml [related]`.
    Add {
        /// Folder to add (absolute or relative to the workspace).
        folder: String,
        /// Workspace whose read-set gains the folder (default: current dir).
        #[arg(long, default_value = ".")]
        repo: String,
    },

    /// Show stats about the persisted index at `<path>/.veles/`.
    Status {
        /// Local path of the indexed repo (default: current directory).
        #[arg(default_value = ".")]
        path: String,
    },

    /// Remove the persisted index at `<path>/.veles/`.
    Clean {
        /// Local path of the indexed repo (default: current directory).
        #[arg(default_value = ".")]
        path: String,
    },

    /// Start a gRPC server.
    ServeGrpc {
        /// Address to bind to.
        #[arg(short, long, default_value = "[::1]:50051")]
        addr: String,
    },

    /// Start an MCP server over stdio.
    ServeMcp {
        /// Workspace used when a tool omits `repo`, and preloaded for the
        /// dashboard. Defaults to VELES_WORKSPACE, CLAUDE_PROJECT_DIR, then
        /// the server process's current directory.
        path: Option<String>,
        /// Also index non-code text files.
        #[arg(long)]
        include_text_files: bool,
        /// Deprecated compatibility flag. Workspace updates are automatic.
        #[arg(long)]
        watch: bool,
        /// Explicitly request the per-repo dashboard. In a `--features dashboard`
        /// build the dashboard is already on by default; this only matters for a
        /// stock build. The dashboard is served by each repo's coordinator.
        #[arg(long)]
        dashboard: bool,
        /// Opt out of the (default-on) dashboard for coordinators this server spawns.
        #[arg(long)]
        no_dashboard: bool,
        /// Preferred dashboard port — only a preference. Each coordinator binds
        /// its OWN port and falls back to a free one if busy; never assume a
        /// fixed port. 0 = always OS-chosen.
        #[arg(long, default_value_t = 0)]
        dashboard_port: u16,
        /// Opt out of auto-opening the dashboard in a browser (on by default
        /// when the dashboard is served — one tab per repository).
        #[arg(long)]
        no_dashboard_open: bool,
    },

    /// Internal: run the per-repository coordinator daemon — the sole writer
    /// for one repository (holds the writer lock, watches, indexes, publishes,
    /// serves the dashboard, idle-exits when no reader remains). Normally
    /// spawned automatically by `serve-mcp`; not part of the everyday UX.
    #[command(hide = true)]
    Coordinator {
        /// Repository to coordinate.
        path: String,
        /// Also index non-code text files.
        #[arg(long)]
        include_text_files: bool,
        /// Explicitly request the dashboard (on by default in a `--features
        /// dashboard` build).
        #[arg(long)]
        dashboard: bool,
        /// Opt out of the (default-on) dashboard.
        #[arg(long)]
        no_dashboard: bool,
        /// Preferred dashboard port (0 = OS-chosen). Only a preference; a busy
        /// port falls back to a free one — each coordinator binds its own.
        #[arg(long, default_value_t = 0)]
        dashboard_port: u16,
        /// Opt out of auto-opening the dashboard when the daemon starts.
        #[arg(long)]
        no_dashboard_open: bool,
    },

    /// List definitions in a single file (functions, structs, classes, ...).
    ///
    /// Tree-sitter parses the file directly — no index required.
    Symbols {
        /// File to outline.
        file: String,
        /// Output format: pretty (default), compact, paths, json, jsonl.
        #[arg(short, long, default_value = "pretty")]
        format: String,
    },

    /// Find every definition with the given name across the indexed repo.
    Defs {
        /// Symbol name to look up (exact match).
        name: String,
        /// Local path of the indexed repo (default: current directory).
        #[arg(default_value = ".")]
        path: String,
        /// Restrict to specific languages (comma-separated).
        #[arg(short = 'l', long, value_delimiter = ',')]
        lang: Vec<String>,
        /// Restrict to a kind (function, struct, class, enum, trait, interface, type, const, static, var, module, method, macro).
        #[arg(short = 'k', long)]
        kind: Option<String>,
        /// Output format: pretty (default), compact, paths, json, jsonl.
        #[arg(short, long, default_value = "pretty")]
        format: String,
        /// Use the multilingual embedding model (must match how the index was built).
        #[arg(long)]
        multilingual: bool,
    },

    /// Find references to a symbol — its definitions plus BM25 hits in chunks.
    Refs {
        /// Symbol name to search for.
        name: String,
        /// Local path of the indexed repo (default: current directory).
        #[arg(default_value = ".")]
        path: String,
        /// Number of BM25 hits to include alongside definitions.
        #[arg(short, long, default_value = "20")]
        top_k: usize,
        /// Output format: pretty (default), compact, ripgrep, paths, json, jsonl.
        #[arg(short, long, default_value = "pretty")]
        format: String,
        /// Use the multilingual embedding model.
        #[arg(long)]
        multilingual: bool,
    },

    /// Launch the interactive terminal UI for live hybrid search.
    ///
    /// Loads the persistent index once, then debounces queries so each
    /// keystroke re-runs in tens of milliseconds. Arrow keys navigate the
    /// hit list, Tab cycles through hybrid/bm25/semantic, Enter opens the
    /// selected result in $EDITOR, Ctrl-R finds related code, ? shows
    /// keybindings.
    ///
    /// Examples:
    ///   veles tui
    ///   veles tui ./my-repo --multilingual
    Tui {
        /// Local path of the indexed repo (default: current directory).
        #[arg(default_value = ".")]
        path: String,
        /// Use the multilingual embedding model (must match how the index was built).
        #[arg(long)]
        multilingual: bool,
        /// Also index non-code text files when building the index from scratch.
        #[arg(long)]
        include_text_files: bool,
        /// Force a fresh in-memory build, ignoring any `.veles/` cache.
        #[arg(long)]
        no_cache: bool,
        /// Echo every keypress to the status bar so you can verify which
        /// modifier/key codes your terminal actually forwards. Useful on
        /// macOS where Alt-* and F-keys often need explicit configuration.
        #[arg(long)]
        debug_keys: bool,
    },

    /// Print a shell completion script to stdout.
    ///
    /// Examples:
    ///   veles completions zsh > ~/.zfunc/_veles
    ///   veles completions bash > /etc/bash_completion.d/veles
    ///   veles completions fish > ~/.config/fish/completions/veles.fish
    Completions {
        /// Target shell.
        #[arg(value_enum)]
        shell: Shell,
    },

    /// Generate roff(7) man pages.
    ///
    /// With no flag, prints just the top-level `veles.1` to stdout.
    /// With `--out-dir`, writes one page per subcommand
    /// (`veles.1`, `veles-search.1`, `veles-defs.1`, ...) so that
    /// `man veles-search` resolves correctly, like git's man layout.
    ///
    /// Examples:
    ///   veles man > veles.1
    ///   veles man --out-dir ~/.local/share/man/man1
    Man {
        /// Write a `veles.1` plus one `veles-<sub>.1` per subcommand
        /// into this directory. The directory is created if needed.
        #[arg(long, value_name = "DIR")]
        out_dir: Option<std::path::PathBuf>,
    },
}

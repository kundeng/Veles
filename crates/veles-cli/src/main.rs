//! Veles CLI — Fast and Accurate Code Search for Agents.
//!
//! This file is intentionally thin: it sets up logging, parses the CLI, and
//! dispatches to per-subcommand handlers. The real logic lives in the
//! sibling modules:
//!
//! - `cli`      — clap-derived `Cli` / `Commands` definitions
//! - `handlers` — one `handle_*` function per subcommand
//! - `output`   — stdout sinks for rendered formatter output
//! - `util`     — index/model loaders and glob-filter helpers
//! - `format`   — re-export of the shared `veles-core` renderer so the CLI and
//!   the MCP/gRPC surfaces speak one format vocabulary

mod cli;
/// Shared, canonical output-format taxonomy — defined once in `veles-core`
/// and reused by every surface so the CLI and agent-facing tools can never
/// drift into separate format vocabularies.
pub use veles_core::format;
mod handlers;
mod output;
mod tui;
mod util;

use anyhow::Result;
use clap::Parser;

use crate::cli::{Cli, Commands};

/// Stable process exit codes — documented so scripts and agents can branch on
/// *why* a run ended, not just success vs failure:
///   0  success
///   1  runtime error (I/O, index, internal)
///   2  usage error (bad flags/args — emitted by clap before we run)
///   3  not found (a requested location / symbol / chunk does not exist)
fn exit_code_for(err: &anyhow::Error) -> i32 {
    if err.downcast_ref::<crate::util::NotFound>().is_some() {
        3
    } else {
        1
    }
}

#[tokio::main]
async fn main() {
    // Initialize logging (to stderr so it doesn't interfere with MCP stdio).
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .with_writer(std::io::stderr)
        .init();

    let cli = Cli::parse();

    let result: Result<()> = match cli.command {
        Some(Commands::Search {
            query,
            path,
            top_k,
            mode,
            format,
            lang,
            path_glob,
            exclude_glob,
            min_score,
            multilingual,
            no_cache,
        }) => handlers::handle_search(
            query,
            path,
            top_k,
            mode,
            format,
            lang,
            path_glob,
            exclude_glob,
            min_score,
            multilingual,
            no_cache,
        ),

        Some(Commands::FindRelated {
            file_path,
            line,
            path,
            top_k,
            format,
            lang,
            path_glob,
            exclude_glob,
            min_score,
            multilingual,
            no_cache,
        }) => handlers::handle_find_related(
            file_path,
            line,
            path,
            top_k,
            format,
            lang,
            path_glob,
            exclude_glob,
            min_score,
            multilingual,
            no_cache,
        ),

        Some(Commands::Index {
            path,
            include_text_files,
            multilingual,
            force,
        }) => handlers::handle_index(path, include_text_files, multilingual, force),

        Some(Commands::Update { path, multilingual }) => {
            handlers::handle_update(path, multilingual)
        }

        Some(Commands::Transform { config }) => handlers::handle_transform(config),

        Some(Commands::Add { folder, repo }) => handlers::handle_add(folder, repo),

        Some(Commands::Status { path }) => handlers::handle_status(path),

        Some(Commands::Clean { path }) => handlers::handle_clean(path),

        Some(Commands::Symbols { file, format }) => handlers::handle_symbols(file, format),

        Some(Commands::Defs {
            name,
            path,
            lang,
            kind,
            format,
            multilingual,
        }) => handlers::handle_defs(name, path, lang, kind, format, multilingual),

        Some(Commands::Refs {
            name,
            path,
            top_k,
            format,
            multilingual,
        }) => handlers::handle_refs(name, path, top_k, format, multilingual),

        Some(Commands::Tui {
            path,
            multilingual,
            include_text_files,
            no_cache,
            debug_keys,
        }) => handlers::handle_tui(path, multilingual, include_text_files, no_cache, debug_keys),

        Some(Commands::Completions { shell }) => handlers::handle_completions(shell),

        Some(Commands::Man { out_dir }) => handlers::handle_man(out_dir),

        Some(Commands::ServeGrpc { addr }) => handlers::handle_serve_grpc(addr).await,

        Some(Commands::ServeMcp {
            path,
            include_text_files,
            watch,
            dashboard,
            no_dashboard,
            dashboard_port,
            no_dashboard_open,
        }) => {
            // Dashboard defaults ON in a `--features dashboard` build (served by
            // each spawned coordinator); `--no-dashboard` opts out. Auto-open is
            // on when served unless `--no-dashboard-open`.
            let dashboard = !no_dashboard && (dashboard || cfg!(feature = "dashboard"));
            let dashboard_open = dashboard && !no_dashboard_open;
            handlers::handle_serve_mcp(
                path,
                include_text_files,
                watch,
                dashboard,
                dashboard_port,
                dashboard_open,
            )
            .await
        }

        Some(Commands::Coordinator {
            path,
            include_text_files,
            dashboard,
            no_dashboard,
            dashboard_port,
            no_dashboard_open,
        }) => {
            let dashboard = !no_dashboard && (dashboard || cfg!(feature = "dashboard"));
            let dashboard_open = dashboard && !no_dashboard_open;
            handlers::handle_coordinator(
                path,
                include_text_files,
                dashboard,
                dashboard_port,
                dashboard_open,
            )
            .await
        }

        None => handlers::handle_default().await,
    };

    if let Err(e) = result {
        eprintln!("Error: {e:#}");
        std::process::exit(exit_code_for(&e));
    }
}

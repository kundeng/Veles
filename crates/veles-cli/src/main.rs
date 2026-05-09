//! Veles CLI — Fast and Accurate Code Search for Agents.
//!
//! Subcommands:
//! - `search` — Search a codebase
//! - `find-related` — Find code similar to a location
//! - `serve-grpc` — Start a gRPC server
//! - `serve-mcp` — Start an MCP server (default if no subcommand)

use std::path::Path;

use anyhow::Result;
use clap::{Parser, Subcommand};

use veles_core::VelesIndex;
use veles_core::model;
use veles_core::types::SearchMode;

#[derive(Parser)]
#[command(name = "veles")]
#[command(about = "Fast and Accurate Code Search for Agents")]
#[command(version)]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
enum Commands {
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
        /// Also index non-code text files.
        #[arg(long)]
        include_text_files: bool,
        /// Use the multilingual embedding model (potion-multilingual-128M)
        /// instead of the default English/code-focused model. Recommended for
        /// codebases or queries containing Cyrillic, CJK, Greek, Arabic, etc.
        #[arg(long)]
        multilingual: bool,
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
        /// Also index non-code text files.
        #[arg(long)]
        include_text_files: bool,
        /// Use the multilingual embedding model.
        #[arg(long)]
        multilingual: bool,
    },

    /// Start a gRPC server.
    ServeGrpc {
        /// Address to bind to.
        #[arg(short, long, default_value = "[::1]:50051")]
        addr: String,
    },

    /// Start an MCP server over stdio.
    ServeMcp {
        /// Optional local path to pre-index at startup.
        path: Option<String>,
        /// Also index non-code text files.
        #[arg(long)]
        include_text_files: bool,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize logging (to stderr so it doesn't interfere with MCP stdio).
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .with_writer(std::io::stderr)
        .init();

    let cli = Cli::parse();

    match cli.command {
        Some(Commands::Search {
            query,
            path,
            top_k,
            mode,
            include_text_files,
            multilingual,
        }) => {
            let model = if multilingual {
                model::load_multilingual_model()?
            } else {
                model::load_model(None)?
            };
            let index = if is_git_url(&path) {
                VelesIndex::from_git(&path, None, Some(model), include_text_files)?
            } else {
                VelesIndex::from_path(Path::new(&path), Some(model), None, include_text_files)?
            };

            let search_mode = mode.parse::<SearchMode>().unwrap_or(SearchMode::Hybrid);
            let results = index.search(&query, top_k, search_mode, None, None, None);

            if results.is_empty() {
                println!("No results found.");
            } else {
                let header = format!("Search results for: {query:?} (mode={mode})");
                println!("{}", format_results(&header, &results));
            }
        }

        Some(Commands::FindRelated {
            file_path,
            line,
            path,
            top_k,
            include_text_files,
            multilingual,
        }) => {
            let mdl = if multilingual {
                model::load_multilingual_model()?
            } else {
                model::load_model(None)?
            };
            let index = if is_git_url(&path) {
                VelesIndex::from_git(&path, None, Some(mdl), include_text_files)?
            } else {
                VelesIndex::from_path(Path::new(&path), Some(mdl), None, include_text_files)?
            };

            let chunk = match index.resolve_chunk(&file_path, line) {
                Some(c) => c.clone(),
                None => {
                    eprintln!("No chunk found at {file_path}:{line}.");
                    std::process::exit(1);
                }
            };

            let results = index.find_related(&chunk, top_k);
            if results.is_empty() {
                println!("No related chunks found for {file_path}:{line}.");
            } else {
                let header = format!("Chunks related to {file_path}:{line}");
                println!("{}", format_results(&header, &results));
            }
        }

        Some(Commands::ServeGrpc { addr }) => {
            let mdl = model::load_model(None)?;
            veles_grpc::serve(&addr, mdl).await?;
        }

        Some(Commands::ServeMcp {
            path: _,
            include_text_files: _,
        }) => {
            let mdl = model::load_model(None)?;
            let server = veles_mcp::McpServer::new(mdl);
            server.run().await?;
        }

        None => {
            // Default: start MCP server.
            let mdl = model::load_model(None)?;
            let server = veles_mcp::McpServer::new(mdl);
            server.run().await?;
        }
    }

    Ok(())
}

/// Format search results as numbered, fenced code blocks.
fn format_results(header: &str, results: &[veles_core::types::SearchResult]) -> String {
    let mut lines: Vec<String> = vec![header.to_string(), String::new()];
    for (i, r) in results.iter().enumerate() {
        lines.push(format!(
            "## {}. {}  [score={:.3}]",
            i + 1,
            r.chunk.location(),
            r.score
        ));
        lines.push("```".to_string());
        lines.push(r.chunk.content.trim().to_string());
        lines.push("```".to_string());
        lines.push(String::new());
    }
    lines.join("\n")
}

/// Check if a path looks like a git URL.
fn is_git_url(path: &str) -> bool {
    path.starts_with("https://")
        || path.starts_with("http://")
        || path.starts_with("ssh://")
        || path.starts_with("git://")
        || path.starts_with("git+ssh://")
}

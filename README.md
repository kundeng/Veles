# Veles

Fast local code search for AI agents. Pure Rust — no Python dependencies.

Indexes a repository (local path or git URL), chunks source files, and provides hybrid search combining BM25 and dense vector similarity via [model2vec-rs](https://github.com/MinishLab/model2vec-rs) static embeddings.

## Interfaces

- **CLI** — `veles search "query" ./my-repo`
- **MCP server** — stdio JSON-RPC for AI agent integration (Claude, Cursor, etc.)
- **gRPC** — tonic-based service with `Index`, `Search`, `FindRelated`, `GetStats` RPCs

## Features

- Hybrid search with Reciprocal Rank Fusion (RRF) blending BM25 and semantic scores
- Identifier-aware tokenizer — splits camelCase, snake_case, and mixed-script names
- Query-type detection — symbol queries lean BM25, natural language leans semantic
- Definition boosting — promotes chunks that define the queried symbol
- Path penalties — demotes test files, compat dirs, re-export files
- File saturation — avoids stacking all results from one file
- Multilingual embedding model option for Cyrillic, CJK, Arabic, etc.

## Usage

### CLI

```sh
# Search the current directory
veles search "parse config file"

# Search a remote repo
veles search "BM25 inverted index" https://github.com/user/repo

# Find code related to a specific location
veles find-related src/main.rs 42

# Search modes
veles search "handler" . --mode bm25
veles search "authentication flow" . --mode semantic

# Multilingual model for non-English queries
veles search "функция обработка" . --multilingual
```

### MCP Server

```sh
# Start MCP server (default if no subcommand given)
veles serve-mcp
veles
```

Exposed tools: `search`, `find_related`.

### gRPC Server

```sh
veles serve-grpc --addr "[::1]:50051"
```

## Build

```sh
cargo build --release
```

## Architecture

```
Veles/
  crates/
    veles-core/    indexing, chunking, BM25, dense search, ranking
    veles-grpc/    gRPC service (tonic + prost)
    veles-mcp/     MCP server over stdio
    veles-cli/     CLI binary
  proto/
    veles.proto    gRPC service definition
```

## License

MIT

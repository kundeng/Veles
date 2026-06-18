# Automatic workspace indexing

Status: Implemented  
Audience: Veles maintainers and integration authors

## Product contract

A user configures the Veles MCP server once and then forgets about indexing:

```toml
[mcp_servers.veles]
command = "veles"
args = ["serve-mcp"]
```

For every coding-agent session, Veles discovers the session workspace,
prepares its search index, keeps it current, and shares the persisted index
with other Veles MCP processes using the same repository.

The common experience must not require users to understand or operate:

- pipelines or stages;
- writer and reader processes;
- owner/follower election;
- lock files;
- dashboard ports;
- index generations.

Those terms may appear in diagnostics intended for maintainers, but not in the
normal setup or workflow.

## User-visible behavior

### Workspace discovery

The workspace is selected in this order:

1. explicit `veles serve-mcp PATH`;
2. `VELES_WORKSPACE`;
3. `CLAUDE_PROJECT_DIR`;
4. the MCP process working directory.

The selected path is canonicalized before it is used as a cache key or
coordination identity. Veles refuses to start if it cannot resolve a local
workspace directory, rather than accidentally indexing an unrelated launcher
directory.

### First use

MCP initialization is immediate. Workspace preparation runs in the background.
If a search arrives before preparation completes, that request waits for the
same shared preparation operation rather than launching duplicate work.

### Normal use

- Tools default to the discovered workspace when `repo` is omitted.
- The workspace is kept fresh automatically; `--watch` is not required.
- Any number of MCP servers may run for one repository.
- Different repositories are completely independent.
- One process performs index writes for a repository at a time.
- Other processes read the last committed index and reload when it changes.
- If the active writer exits, another live MCP process can take over on its
  next repository access.

### Failure behavior

- A failed update does not invalidate the last successfully committed index.
- A reader continues serving the last committed index while another process
  updates it.
- Errors use product language such as “could not update this workspace” and
  include repository and underlying diagnostic details.
- Advanced status output may identify the lock holder and persisted
  generation.

## Internal architecture

### Repository coordinator

Each MCP process owns a `RepositoryCoordinator`. It tracks repository-local
state keyed by canonical path.

On repository access the coordinator:

1. attempts to acquire `<repo>/.veles/writer.lock`;
2. when acquired, retains the lock for the process lifetime and starts the
   recursive watcher;
3. when held elsewhere, remains read-only and checks the persisted generation
   before serving;
4. retries acquisition on later access, allowing automatic takeover after the
   previous writer exits.

Coordination is per repository. TCP ports and dashboards have no role in
writer selection.

### Write lifecycle

The writer performs the complete repository loop:

```text
discover -> transform if configured -> index -> publish -> watch -> repeat
```

For an ordinary code repository the transform step is the identity operation:
the repository files are indexed directly.

The current automatic path uses the identity transform for ordinary
repositories. Existing `veles.pipeline.json` transform stages remain an
advanced ingestion facility: they namespace transformed output, resolve paths
relative to their configuration, and publish through the same generation-safe
index store. Continuous external-source scheduling is an extension point and
is not required for the zero-configuration coding-workspace contract.

### Readers

MCP tool handlers do not write indexes directly. They:

1. ensure repository coordination has been attempted;
2. wait for an initial index when this process became the writer;
3. compare the persisted generation marker with the cached generation;
4. reload the committed index when the marker changes;
5. execute the requested read operation.

The MCP `update` tool is retained for compatibility but delegates to the
coordinator. It never bypasses destination locking.

### Persistence

An index is published as an immutable generation:

```text
.veles/
  CURRENT
  generations/
    <generation-id>/
      manifest.json
      chunks.bin
      bm25.bin
      dense.bin
      symbols.bin
```

The writer creates and fsyncs a complete generation, then atomically replaces
`CURRENT`. Readers resolve `CURRENT` once and therefore never combine files
from different saves. Legacy flat indexes remain readable and are migrated on
their next successful save.

Published generations are immutable. Best-effort garbage collection removes
non-current generations only after they are at least 24 hours old, so a slow
reader cannot lose files from the generation it already selected.

### Dashboard

The dashboard is optional observability. Every MCP process may bind its own
ephemeral or requested port.

It reports:

- current workspace;
- preparation/index state;
- whether this process is writing or reading;
- watch state;
- current persisted generation;
- file and chunk counts;
- recent updates and errors.

Dashboard availability, port conflicts, or browser opening never affect index
coordination or search correctness.

## Configuration

Ordinary repositories require no configuration.

Advanced transformed sources use `veles.pipeline.json`. Each input must have a
stable name so derived paths cannot collide:

```json
{
  "stages": [{
    "name": "agent-sessions",
    "dest": "~/.veles-corpora/sessions",
    "inputs": [{
      "name": "codex",
      "source": "~/.codex/sessions/**/rollout-*.jsonl",
      "transform": ["python3", "scripts/codex_distill.py"]
    }]
  }]
}
```

Relative configuration paths are resolved against the directory containing
the configuration file. Derived output is namespaced as
`<dest>/<input-name>/<source-relative-path>.md`.

## Compatibility

- `veles serve-mcp --watch` remains accepted but is redundant.
- Explicit `repo` tool arguments remain supported.
- Existing flat `.veles` indexes remain readable.
- Existing pipeline inputs without `name` derive a stable name from their
  ordinal position for deserialization compatibility, but validation warns
  maintainers to add explicit names.

## Verification requirements

The implementation is not complete without tests proving:

1. two MCP coordinators for one repository produce one writer;
2. coordinators for different repositories both become writers;
3. a reader reloads after a committed generation change;
4. takeover succeeds after the previous writer drops its lock;
5. dashboard port conflicts do not affect coordination;
6. MCP notifications receive no JSON-RPC response;
7. `include_text_files` reaches initial indexing;
8. transformed inputs cannot overwrite each other's outputs;
9. readers never observe a partially published generation;
10. ordinary `serve-mcp` setup requires no watch or pipeline flags.

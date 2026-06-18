# Coordination and pipeline model

Status: Design rationale and roadmap  
Audience: Veles maintainers and contributors

This document records *why* automatic workspace indexing is built the way it
is, and where the architecture is going. It is the reasoning companion to
[automatic-workspace-indexing.md](automatic-workspace-indexing.md), which
specifies the behavior that ships today. Where the two could be read to
disagree, the implemented spec wins for current behavior and this document
describes the intended direction.

A status tag marks each section:

- **[implemented]** — true of the shipping build.
- **[proposed]** — agreed direction, not yet built. Do not document these to
  users as if they exist.

## The boundary that matters

Every other decision follows from one ownership boundary:

```text
Writer  = the sole process mutating one destination/index
Readers = any number of processes consuming that committed index
```

Different repositories are completely independent. There is no global
coordinator, no shared daemon, and **no TCP-port election**. Coordination is
per destination, decided by a file lock, and invisible to users.

This replaced an earlier model in which the dashboard port doubled as a
singleton-election token (owner binds the port; followers reload). That coupled
correctness to an optional UI and a network port. The lock-based model is
correct with no dashboard, survives a writer crash without cleanup, and lets
unrelated repositories run without false contention. It is both simpler and
more robust — the rare case where a design gets smaller as it gets better.

## Why a file lock **[implemented]**

The writer holds a non-blocking `flock(2)` on `<repo>/.veles/writer.lock` for
its entire lifetime (see `veles-core/src/lock.rs`).

- **Crash safety for free.** The kernel releases the lock the instant the
  holder dies — `SIGKILL`, panic, power loss. There is no stale-lock cleanup
  and no "is that recorded PID still alive?" guessing that a PID file forces.
- **Lifetime = ownership.** The lock lives exactly as long as the open file
  descriptor. In Rust terms the guard's `Drop` releases it; keeping the guard
  alive keeps the lock held. Writer identity (`pid`, start time, label) is
  written into the file purely for `who holds this?` diagnostics — the
  guarantee is the `flock`, never the file's existence.
- **Keyed by destination.** Two writers on *different* destinations never
  contend; only a genuine same-destination collision is refused.

The non-unix fallback currently always "acquires" (documented limitation, not a
silent false guarantee): the persistent-writer daemon is expected under a unix
supervisor, and Windows is reader-only in practice. A real Windows file-lock is
**[proposed]** before persistent multi-writer use is supported there.

## Crash-safe publication **[implemented]**

Readers must never observe half of an update. The writer publishes immutable
generations and swaps a pointer atomically (see `veles-core/src/persist.rs`):

```text
.veles/
  CURRENT                 # atomically-replaced generation id
  generations/
    000042/
      manifest.json
      chunks.bin
      bm25.bin
      dense.bin
      symbols.bin
```

The writer fsyncs a complete generation directory, then atomically rewrites
`CURRENT`. A reader resolves `CURRENT` once per operation and therefore never
mixes files from two saves. A failed update cannot invalidate the last
committed generation. Old generations are garbage-collected, retaining the
current and immediately previous one. Legacy flat `.veles` indexes remain
readable and migrate on their next successful save.

Reader freshness uses the monotonic generation id, not manifest mtime: compare
`CURRENT` before a tool request and reload only when it changes.

## Current implementation **[implemented]**

Today the coordinator lives inside `veles-mcp` (`src/watch.rs`):

- Each `serve-mcp` process runs a per-repository coordinator keyed by canonical
  path. The process that wins the destination lock becomes the writer for its
  lifetime, runs the debounced recursive watcher, and publishes updates.
- Other `serve-mcp` processes are readers; they retry lock acquisition on later
  access, so when the writer exits another live process takes over
  automatically.
- The watcher reuses `update_from_path`: a `notify` debouncer (~1.5s) coalesces
  save-storms and `git pull` bursts; only files whose BLAKE3 content hash
  changed are re-embedded. Writes under `.veles/` and standard heavy dirs are
  filtered *relative to each watched root* so the index's own `save()` never
  self-triggers.
- `veles transform` performs a one-shot, lock-guarded pipeline run for
  configured transformed corpora. There is no long-running pipeline daemon yet.

So the writer is "whichever MCP process won the lock." That is correct, but it
puts the write lifecycle inside the reader binary.

## Target architecture **[proposed]**

Make the ownership boundary structural rather than incidental. A long-running
per-repository coordinator (an index-lifecycle *service*) is the sole writer;
MCP is a pure reader that ensures such a service exists.

```text
Source files
   │ watch
Coordinator (sole writer; holds writer.lock for its lifetime)
   │ transform if configured
Derived corpus
   │ incremental index
Atomic index generation  ──►  any number of MCP readers
```

```rust
// the orchestration the `veles` executable should perform invisibly
serve_mcp(workspace):
    ensure_repo_service(workspace)   // start a detached coordinator if none holds the lock
    run_read_only_mcp(workspace)     // never writes, indexes, or watches source
```

Coordinator responsibilities: acquire the destination lock; load+validate
config; initial discovery; transform changed inputs; incrementally publish;
watch source roots; debounce and coalesce; handle add/modify/rename/remove;
keep serving the last valid index after a transform failure; expose
status/events; release the lock on exit. Ordinary code repositories fit the
same model as transformed corpora — the code repo is just an input with an
identity transform.

### Startup UX, in order of priority

1. **Explicit service [proposed]** — `veles index-service <path>` (avoid
   exposing the internal word "pipeline" in normal docs), suitable for
   launchd/systemd.
2. **Managed startup [proposed]** — `serve-mcp <path> --ensure-service` starts
   a *detached* coordinator only if no live writer holds the lock. MCP still
   never becomes the writer. Simultaneous starts race safely on the lock.

### Discovery via a runtime file **[proposed]**

The lock decides ownership; a `<repo>/.veles/runtime.json` is informational
only (never authoritative) and powers `status`/dashboard discovery:

```json
{ "pid": 12345, "started_at": "...", "dashboard_url": "http://127.0.0.1:49321",
  "generation": 42, "state": "watching" }
```

### Process lifecycle **[proposed]**

The coordinator need not live forever: start on first search, stay alive while
MCP clients are active plus a short idle grace, then exit and restart on the
next request. MCP must keep serving the last committed index while a
replacement starts. Large corpora may opt into persistent operation.

### Component split **[proposed]**

```text
veles-core      IndexReader · IndexWriter · AtomicGenerationStore ·
                DestinationLock · PipelineEngine · PipelineState
veles-pipeline  source watchers · transform execution · debounce/scheduling ·
                status/dashboard · long-running daemon
veles-mcp       MCP protocol · read-only generation cache · workspace handling
```

The split is an *operational ownership boundary* (who may write), not a tidy
grouping of features.

### Dashboard placement **[proposed]**

The dashboard belongs to the coordinator, which owns the meaningful state
(source roots, generation, watch status, transform queue/failures, writer PID
and uptime). Each coordinator binds an ephemeral port advertised via
`runtime.json`. The dashboard remains pure observability — it never affects
coordination or correctness, today or under this plan.

## User-visible contract

Configure once; never operate indexing:

```toml
[mcp_servers.veles]
command = "veles"
args = ["serve-mcp"]
```

First use may say *"Veles is preparing the index for this workspace; searches
will be available shortly."* Normal use is silent. Errors use product language
("Veles could not index this workspace"; "search is using the last successful
index"). Diagnostics — not normal output — may expose locks, generations,
writer/reader roles, transform queues, and daemon coordination.

Workspace selection (precedence): explicit `PATH` > `VELES_WORKSPACE` >
`CLAUDE_PROJECT_DIR` > the MCP process working directory; canonicalized; Veles
refuses to start on an unresolved workspace rather than indexing `$HOME` or a
launcher directory. **[implemented]**

CLI surface stays small for the common path (`veles serve-mcp`); internals live
behind advanced/diagnostic commands (`veles status`, and **[proposed]**
`veles doctor` / `veles logs` / `veles dashboard`). `veles transform` exists
today; a user-facing long-running service command is **[proposed]**.

## Open design issues

| # | Issue | Status |
|---|-------|--------|
| 1 | Atomic generation publication; readers never see a partial save | **[implemented]** |
| 2 | Writer lock keyed by canonical destination, held for the whole lifetime (not per update) | **[implemented]** |
| 3 | Two different pipeline configs targeting one destination must not silently share state — store a config fingerprint in runtime state + manifest; reject with both fingerprints and the owner | **[proposed]** |
| 4 | Every input needs a stable output namespace so different sources cannot collide on a derived path | partial — stage/input naming exists; collision rejection **[proposed]** |
| 5 | Transform-failure semantics: keep last good output, mark source stale, continue others, retry on change, surface the error | **[proposed]** |
| 6 | Event storms/backpressure: per-source debounce, bounded queue, coalescing, at most one active update per destination, a dirty flag for changes during indexing | partial — debounce + single watcher exist; bounded queue + dirty-flag **[proposed]** |
| 7 | Self-triggering: exclude the index dir and any generated-output subtree from watched source roots; validate topology | partial — `.veles`/heavy dirs excluded relative to root; derived-output topology check **[proposed]** |
| 8 | Reader freshness via monotonic generation id, not mtime | **[implemented]** |
| 9 | Process lifecycle: crash releases the OS lock automatically; supervisor/`--ensure-service` restarts; runtime metadata never determines ownership | partial — crash-release + takeover **[implemented]**; supervised idle lifecycle **[proposed]** |
| 10 | Windows: real file-lock mechanism, or explicitly refuse persistent writer mode on unsupported platforms | **[proposed]** (current fallback allows multiple writers) |

## Summary

The durable principle is the writer/reader ownership boundary enforced by a
destination file lock and atomic generation publication. It is implemented for
the MCP-internal case today. The roadmap moves the write lifecycle into an
explicit, lazily-started, idle-aware coordinator so that `veles serve-mcp`
remains the entire user-facing contract while the internals gain a clean,
testable ownership separation.

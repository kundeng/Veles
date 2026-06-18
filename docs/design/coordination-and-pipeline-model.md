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
puts the write lifecycle inside the reader binary — and that is the wrong place.

## Decision (2026-06-18): the coordinator is an out-of-process daemon

The in-process model above is **interim**. The decided target is a separate,
out-of-process coordinator per destination; the MCP server is a pure reader
that *ensures* one exists. Three concrete problems with hosting the write
lifecycle inside the MCP process drove the decision:

1. **Cross-repository search exposes the conflation.** An MCP launched in
   repo A may be asked to search repo B. In the in-process model it then
   becomes B's writer *inside the A session* — it starts watching and indexing
   B just because someone searched it. Indexing repo B is not the job of "the
   MCP server for A." The correct response to "B has no writer" is *ensure a
   coordinator daemon for B is running*, then read B's committed index — not to
   turn this reader into B's indexer.

2. **Lifecycle coupling.** If the writer lives inside an MCP process, B's index
   freshness is hostage to the A agent's session: when that MCP exits, B's
   writer dies with it even though other agents may still be reading B. A
   daemon's lifecycle is tied to the *repository's* need, not one client's.

3. **Dashboard ownership and ports** (see below): "which process serves the
   dashboard" only has a clean answer when there is exactly one coordinator per
   repository. With N MCP processes there is no such answer; with one daemon
   per repo there is.

The interim in-process coordinator stays until the daemon lands, because it is
correct for the common single-repo single-agent case and is fully verified. New
work targets the daemon model below; it does not extend the in-process writer.

## Target architecture **[decided]**

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

### Startup: MCP self-spawns a detached coordinator **[decided]**

The default requires zero user setup. On attaching to a repo, the MCP process
runs `ensure_coordinator(repo)`: if no live process holds `repo/.veles/writer.lock`,
it spawns a **detached** `veles` coordinator subprocess for that repo, then
proceeds as a pure reader. Simultaneous starts race safely on the lock — the
loser exits, having done no harm. The MCP never becomes the writer itself.

A supervisor-managed mode stays available for persistent/large deployments
(`veles index-service <path>` under launchd/systemd) — same daemon, externally
managed lifecycle. It is opt-in; the self-spawn default covers the common case.

### Repository scope: the read-set **[decided]**

An MCP serves a **read-set** of repositories. By default the read-set is exactly
its own workspace — **MCP for repo A reads only A.** A tool call naming a repo
outside the read-set is out of scope; it does **not** silently make this process
coordinate that repo. (This is what retires the interim "search `repo=B` turns
A's MCP into B's indexer" behavior.)

Cross-repository search is explicit and durable. "A and B are related, search
both" is a persistent fact stored in the workspace, not a per-session toggle:

```toml
# A/.veles/config.toml
[related]
repos = ["../repoB"]
```

The dashboard edits this config. Adding B: persist it → `ensure_coordinator(B)`
(spawn B's daemon if unheld) → drop a reader lease in B (below) → include B in
search scope. On next startup A re-attaches its related repos automatically.

### Discovery via a runtime file **[decided]**

The lock decides ownership; `<repo>/.veles/runtime.json` is informational only
(never authoritative) and powers `status` and dashboard discovery — "open the
dashboard for repo X" resolves through X's `runtime.json`, since there is no
fixed port:

```json
{ "pid": 12345, "started_at": "...", "dashboard_url": "http://127.0.0.1:49321",
  "generation": 42, "state": "watching" }
```

### Process lifecycle: reader leases + idle-exit **[decided]**

The coordinator does not live forever; it exits once nothing is reading its
repo, and restarts on the next access. Liveness is *observed*, not declared
(the same philosophy as `flock`), so a reader crash needs no cleanup:

- **Reader leases.** When an MCP attaches to repo X it creates a lease file
  `X/.veles/readers/<uuid>` and refreshes its mtime on a fixed interval
  (≈15s). A reader attached to several repos holds one lease per repo.
- **Idle-exit.** X's coordinator counts leases whose mtime is fresh (within
  ≈2× the interval). When that count is zero for a grace window, the daemon
  releases `writer.lock` and exits. A dead reader simply stops refreshing; its
  lease ages out and is swept — no detach handshake.
- **Restart-safe reads.** While a coordinator is absent or restarting, readers
  keep serving the last committed generation; the next access re-spawns one.
- **Persistent opt-in.** Large corpora may run the supervisor-managed daemon,
  which ignores idle-exit.

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

### Dashboard placement and ports **[decided]**

The dashboard belongs to the **coordinator**, which owns the meaningful state
(source roots, generation, watch status, role, transform queue/failures, writer
PID and uptime). This is the second reason coordination must be out-of-process:
one coordinator per repository means exactly one dashboard per repository, with
a clear owner and no contention.

Ports follow from there:

- **No fixed port.** Multiple veles processes cannot share one port, so a fixed
  port is incoherent as a default — it is at most a *preference* that silently
  falls back to an OS-chosen free port when busy. Code must never assume a
  process can bind a specific port, and discovery must never depend on one.
- **Each coordinator binds its own ephemeral port** and records the resulting
  URL in `<repo>/.veles/runtime.json` (the discovery file above). "Open the
  dashboard for repo X" = read X's `runtime.json` and visit its `dashboard_url`
  — works regardless of how many repos/agents are live.
- **Auto-open** is a per-coordinator convenience (one tab per repository, when
  its daemon starts), not per MCP client — so N agents on one repo do not open
  N tabs.

Interim note: in today's in-process model there is no daemon to own the
dashboard, so a dashboard (when enabled) is served by the MCP process on an
ephemeral port and is not reliably discoverable across processes. That
limitation is a direct consequence of the in-process writer and is resolved by
the daemon, not by a fixed port. The dashboard remains pure observability — it
never affects coordination or correctness, in either model.

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
| 9 | Process lifecycle: crash releases the OS lock automatically; runtime metadata never determines ownership | crash-release + takeover **[implemented]**; idle-exit daemon **[decided]** (below) |
| 10 | Windows: real file-lock mechanism, or explicitly refuse persistent writer mode on unsupported platforms | **[proposed]** (current fallback allows multiple writers) |
| 11 | Coordinator is an out-of-process daemon, one per destination; MCP is a pure reader that self-spawns a detached coordinator when no lock holder exists | **[decided]**, not yet built |
| 12 | Default read-set is the MCP's own workspace; cross-repo search is explicit and persisted in `repo/.veles/config.toml [related]`, edited via the dashboard | **[decided]**, not yet built |
| 13 | Reader leases (`repo/.veles/readers/<uuid>`, mtime-refreshed) + idle-exit when no fresh lease for a grace window | **[decided]**, not yet built |
| 14 | Dashboard owned by the coordinator (one per repo), ephemeral port, discovered via `runtime.json`; no fixed port — at most a preference with ephemeral fallback | **[decided]**, not yet built |

## Summary

The durable principle is the writer/reader ownership boundary enforced by a
destination file lock and atomic generation publication, implemented today for
the interim MCP-internal case. The **decided** target makes that boundary an
out-of-process daemon: one coordinator per repository (the sole writer), spawned
on demand by an MCP that is otherwise a pure reader. An MCP reads only its own
workspace by default; cross-repo search is explicit and persisted. Coordinators
idle-exit once no reader lease remains, and each owns its repository's dashboard
on an ephemeral port discovered via `runtime.json`. The user-facing contract
stays `veles serve-mcp`; the internals gain a clean, testable ownership
separation. None of the daemon model is built yet — it is the agreed design for
the next implementation phase.

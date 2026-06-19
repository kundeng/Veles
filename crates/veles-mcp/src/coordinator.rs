//! The out-of-process coordinator daemon (`veles coordinator <repo>`).
//!
//! Exactly one coordinator is the **sole writer** for a repository: it holds
//! `<repo>/.veles/writer.lock`, builds and incrementally publishes the index,
//! watches the source tree, and serves the optional dashboard. MCP servers
//! never write — they spawn a coordinator on demand (see [`crate::spawn`]) and
//! read the committed generations it publishes.
//!
//! Lifecycle is lease-driven: the daemon exits once no reader lease has been
//! refreshed within the freshness window (after a startup grace), and the next
//! reader access re-spawns it. A crash releases the `flock` automatically, so a
//! standby coordinator (or a fresh spawn) can take over with no cleanup.

use std::path::Path;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use model2vec_rs::model::StaticModel;

use veles_core::cache::IndexCache;
use veles_core::ingest::{self, IngestMode};
use veles_core::{lease, lock, persist, pipeline, runtime};

use crate::watch::{RepoRole, WatchManager};

/// How often the daemon checks for live readers and refreshes runtime state.
const CHECK_INTERVAL: Duration = Duration::from_secs(10);
/// The daemon never idle-exits within this window of startup, so a reader that
/// spawned it has time to establish (and keep refreshing) its lease.
const STARTUP_GRACE: Duration = Duration::from_secs(30);
/// How often a distill coordinator re-derives changed sources. The index is
/// resident and updated in place, so this bounds freshness, not cost — and a
/// live agent session rewriting its transcript needn't re-index every tick.
const DISTILL_INTERVAL: Duration = Duration::from_secs(60);

/// Read a `Duration` from an env var (whole seconds), falling back to `default`.
/// Lets tests shrink the idle-exit timers; unset in normal use.
fn secs_env(var: &str, default: Duration) -> Duration {
    std::env::var(var)
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .map(Duration::from_secs)
        .unwrap_or(default)
}

fn now_epoch_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn runtime_state(
    repo: &Path,
    state: &str,
    dashboard_url: Option<String>,
    started: i64,
) -> runtime::RuntimeState {
    runtime::RuntimeState {
        pid: std::process::id(),
        started_at: started,
        dashboard_url,
        generation: persist::current_generation(repo),
        state: state.to_string(),
    }
}

/// Run the coordinator for `repo` until it idles out (or the lock is already
/// held by a live coordinator, in which case it returns immediately).
pub async fn run(
    model: StaticModel,
    repo: String,
    include_text_files: bool,
    dashboard: bool,
    dashboard_port: u16,
    dashboard_open: bool,
) -> Result<()> {
    let _ = (dashboard, dashboard_port, dashboard_open); // used in the `dashboard` build
    let canonical = std::fs::canonicalize(&repo)
        .with_context(|| format!("resolve coordinator repo {repo:?}"))?;

    // Decide how this folder is ingested. A verbose-JSON folder (e.g. agent
    // transcripts) is distilled into a veles-owned shadow index instead of
    // being indexed in place; the source folder is never written to.
    let plan = ingest::establish_plan(&canonical)
        .with_context(|| format!("resolve ingest plan for {}", canonical.display()))?;
    if plan.mode == IngestMode::Distill {
        return run_distill(model, plan).await;
    }

    let key = canonical.to_string_lossy().into_owned();
    let started = now_epoch_secs();

    let cache = std::sync::Arc::new(IndexCache::new(model));
    let (events, _rx) = tokio::sync::broadcast::channel::<String>(256);
    let watch = WatchManager::new(cache.clone(), events.clone()).context("start watch manager")?;

    // Acquire the destination writer lock by becoming this repo's writer. If
    // another live coordinator already holds it, stand down cleanly.
    match watch.ensure(&key)? {
        RepoRole::Reader => {
            eprintln!("veles coordinator: {key} already has a writer; exiting");
            return Ok(());
        }
        RepoRole::Writer => {}
    }
    eprintln!(
        "veles coordinator: writing {key} (pid {})",
        std::process::id()
    );

    // Initial build + publish so readers have a committed generation promptly.
    let index = cache.get_or_load(&key, include_text_files).await?;
    if !persist::index_exists(&canonical) {
        index.write().await.save(&canonical)?;
    }
    runtime::write(
        &canonical,
        &runtime_state(&canonical, "watching", None, started),
    )?;

    // Optional dashboard, owned by the coordinator (one per repo).
    #[cfg(feature = "dashboard")]
    let dashboard_url = if dashboard {
        match crate::dashboard::bind(dashboard_port) {
            Ok((listener, addr)) => {
                let url = format!("http://{addr}");
                eprintln!("veles dashboard: {url}");
                crate::dashboard::serve(
                    listener,
                    cache.clone(),
                    Some(watch.clone()),
                    events.clone(),
                    key.clone(),
                );
                if dashboard_open {
                    crate::open_browser(&url);
                }
                Some(url)
            }
            Err(e) => {
                eprintln!("veles dashboard: UI disabled: {e}");
                None
            }
        }
    } else {
        None
    };
    #[cfg(not(feature = "dashboard"))]
    let dashboard_url: Option<String> = None;

    if dashboard_url.is_some() {
        runtime::write(
            &canonical,
            &runtime_state(&canonical, "watching", dashboard_url.clone(), started),
        )?;
    }

    // Idle-exit loop: leave once no reader lease is fresh, after a startup
    // grace. The watcher itself runs in WatchManager's spawned consumer.
    let check_interval = secs_env("VELES_COORD_CHECK_SECS", CHECK_INTERVAL);
    let startup_grace = secs_env("VELES_COORD_STARTUP_GRACE_SECS", STARTUP_GRACE);
    let begin = Instant::now();
    loop {
        tokio::time::sleep(check_interval).await;
        let fresh = lease::count_fresh(&canonical);
        // Keep runtime state (generation) current for status/dashboard.
        let _ = runtime::write(
            &canonical,
            &runtime_state(&canonical, "watching", dashboard_url.clone(), started),
        );
        if fresh == 0 && begin.elapsed() > startup_grace {
            eprintln!("veles coordinator: no active readers for {key}; exiting");
            break;
        }
    }

    runtime::remove(&canonical);
    // Dropping `watch` here releases the writer lock; process exit would too.
    drop(watch);
    Ok(())
}

/// Coordinator loop for a **distill** folder.
///
/// Shares the in-place path's memory model: the index is **resident** (loaded
/// once via [`IndexCache`]) and updated **in place** — never reloaded. The only
/// difference from in-place is that this watches a verbose-JSON *source* and
/// indexes a derived *shadow*, so each refresh first runs the cheap derive step
/// ([`pipeline::derive_folder`], a `(size, mtime)` stat-scan) and only touches
/// the resident index when something actually changed.
///
/// Refresh is throttled (`VELES_DISTILL_INTERVAL_SECS`, default 60s): an active
/// agent session rewrites its transcript constantly, and re-indexing every few
/// seconds buys no useful freshness. Idle-exit is still checked on the faster
/// `CHECK_INTERVAL` so the daemon leaves promptly once no reader holds a lease.
async fn run_distill(model: StaticModel, plan: ingest::Plan) -> Result<()> {
    let source = plan.source;
    let root = plan.index_root; // the shadow derived dir
    let root_key = root.to_string_lossy().into_owned();
    let started = now_epoch_secs();
    std::fs::create_dir_all(&root)
        .with_context(|| format!("create shadow index root {}", root.display()))?;

    // Sole-writer on the shadow root (not the source). Stand down if another
    // coordinator already owns it.
    let _guard = match lock::try_acquire(&root, "distill", started)? {
        lock::LockOutcome::Acquired(g) => g,
        lock::LockOutcome::Held { holder } => {
            eprintln!(
                "veles coordinator: {} already has a writer ({holder}); exiting",
                root.display()
            );
            return Ok(());
        }
    };
    eprintln!(
        "veles coordinator: distilling {} → {} (pid {})",
        source.display(),
        root.display(),
        std::process::id()
    );

    // Initial derive so the derived tree exists, then load the index ONCE.
    let (init, _) = pipeline::derive_folder(&source, &root)?;
    eprintln!(
        "veles distill: initial — {} source(s), +{} derived, -{} removed",
        init.sources_seen, init.derived_written, init.derived_removed
    );
    let cache = std::sync::Arc::new(IndexCache::new(model));
    let index = cache.get_or_load(&root_key, true).await?;
    if !persist::index_exists(&root) {
        index.write().await.save(&root)?;
    }
    runtime::write(&root, &runtime_state(&root, "watching", None, started))?;

    let check_interval = secs_env("VELES_COORD_CHECK_SECS", CHECK_INTERVAL);
    let startup_grace = secs_env("VELES_COORD_STARTUP_GRACE_SECS", STARTUP_GRACE);
    let distill_interval = secs_env("VELES_DISTILL_INTERVAL_SECS", DISTILL_INTERVAL);
    let begin = Instant::now();
    let mut last_derive = Instant::now();
    loop {
        tokio::time::sleep(check_interval).await;

        // Throttled re-derive; update the RESIDENT index in place only on change.
        if last_derive.elapsed() >= distill_interval {
            last_derive = Instant::now();
            match pipeline::derive_folder(&source, &root) {
                Ok((rpt, changed)) if changed => {
                    let mut idx = index.write().await;
                    match idx.update_from_path(&root) {
                        Ok(update) => {
                            if !update.is_noop()
                                && let Err(e) = idx.save(&root)
                            {
                                eprintln!("veles distill: save failed: {e}");
                            }
                            let s = idx.stats();
                            eprintln!(
                                "veles distill: +{} derived, -{} removed → {} files / {} chunks",
                                rpt.derived_written,
                                rpt.derived_removed,
                                s.indexed_files,
                                s.total_chunks
                            );
                        }
                        Err(e) => eprintln!("veles distill: index update failed: {e}"),
                    }
                }
                Ok(_) => {} // nothing changed — resident index untouched
                Err(e) => eprintln!("veles distill: derive failed: {e}"),
            }
        }

        let _ = runtime::write(&root, &runtime_state(&root, "watching", None, started));
        let fresh = lease::count_fresh(&root);
        if fresh == 0 && begin.elapsed() > startup_grace {
            eprintln!(
                "veles coordinator: no active readers for {}; exiting",
                root.display()
            );
            break;
        }
    }

    runtime::remove(&root);
    Ok(())
}

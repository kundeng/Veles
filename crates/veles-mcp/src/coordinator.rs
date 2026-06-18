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
use veles_core::{lease, persist, runtime};

use crate::watch::{RepoRole, WatchManager};

/// How often the daemon checks for live readers and refreshes runtime state.
const CHECK_INTERVAL: Duration = Duration::from_secs(10);
/// The daemon never idle-exits within this window of startup, so a reader that
/// spawned it has time to establish (and keep refreshing) its lease.
const STARTUP_GRACE: Duration = Duration::from_secs(30);

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

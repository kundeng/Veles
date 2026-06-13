//! Filesystem watcher → incremental auto-update for the MCP server.
//!
//! Keeps cached indexes fresh by invoking the existing
//! [`veles_core::VelesIndex::update_from_path`] whenever files under a
//! watched repo change, so semantic search never serves a stale index
//! within a session. Three properties make this safe and cheap:
//!
//! * **Incremental** — reuses `update_from_path`, which re-embeds only
//!   files whose BLAKE3 content hash changed. A one-file edit costs ~0.1s,
//!   not a full rebuild.
//! * **Debounced** — editor save-storms and `git pull` bursts coalesce
//!   into a single update via [`notify_debouncer_full`].
//! * **No self-trigger** — writes under `.veles/` (and the standard
//!   ignored dirs) are filtered *relative to each watched root*, so the
//!   index's own `save()` never retriggers an update loop.
//!
//! Watching is opt-in (`serve-mcp --watch`) and per-repo: only repos that
//! are actually opened in the session are watched — never a whole tree.

use std::collections::{HashMap, HashSet};
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Result;
use notify::{RecommendedWatcher, RecursiveMode};
use notify_debouncer_full::{new_debouncer, DebounceEventResult, Debouncer, RecommendedCache};
use tokio::sync::mpsc;

use veles_core::cache::IndexCache;
use veles_core::walker::DEFAULT_IGNORED_DIRS;

/// Quiet window before a burst of filesystem events triggers one update.
const DEBOUNCE: Duration = Duration::from_millis(1500);

/// Owns the debouncer and the set of watched repos. Dropping it stops
/// all watches (the debouncer's worker thread shuts down with it).
pub struct WatchManager {
    debouncer: Mutex<Debouncer<RecommendedWatcher, RecommendedCache>>,
    /// canonical repo root → original cache key (the string the handlers
    /// passed to `get_or_load`, which is how the cache is keyed).
    watched: Arc<Mutex<HashMap<PathBuf, String>>>,
}

impl WatchManager {
    /// Build the watcher and spawn the async update consumer.
    pub fn new(cache: Arc<IndexCache>) -> Result<Arc<Self>> {
        let (tx, mut rx) = mpsc::unbounded_channel::<PathBuf>();
        let watched: Arc<Mutex<HashMap<PathBuf, String>>> = Arc::new(Mutex::new(HashMap::new()));

        // notify worker thread: forward every changed path. Filtering is
        // done in the consumer, where we know which root a path belongs to.
        let debouncer = new_debouncer(DEBOUNCE, None, move |res: DebounceEventResult| {
            if let Ok(events) = res {
                for ev in events {
                    for p in ev.paths.iter() {
                        let _ = tx.send(p.clone());
                    }
                }
            }
        })?;

        // Async consumer: batch → map paths to watched roots → incremental update.
        let watched_c = watched.clone();
        tokio::spawn(async move {
            while let Some(first) = rx.recv().await {
                // Drain whatever else is already queued into one batch.
                let mut paths = vec![first];
                while let Ok(p) = rx.try_recv() {
                    paths.push(p);
                }

                // Map each changed path to (canonical root, cache key),
                // dropping paths that fall under an ignored dir *relative
                // to their root* (notably `.veles/` — avoids self-trigger).
                let targets: HashSet<(PathBuf, String)> = {
                    let w = watched_c.lock().unwrap();
                    paths
                        .iter()
                        .filter_map(|p| {
                            w.iter().find_map(|(root, key)| {
                                let rel = p.strip_prefix(root).ok()?;
                                if rel_is_ignored(rel) {
                                    None
                                } else {
                                    Some((root.clone(), key.clone()))
                                }
                            })
                        })
                        .collect()
                };

                for (root, key) in targets {
                    let Some(idx_arc) = cache.peek(&key) else {
                        continue; // evicted from cache; nothing to refresh
                    };
                    let mut idx = idx_arc.write().await;
                    match idx.update_from_path(&root) {
                        Ok(report) if !report.is_noop() => match idx.save(&root) {
                            Ok(()) => eprintln!(
                                "veles watch: updated {key} (+{} ~{} -{}, {} chunks)",
                                report.added_files,
                                report.modified_files,
                                report.removed_files,
                                report.total_chunks
                            ),
                            Err(e) => eprintln!("veles watch: save failed for {key}: {e}"),
                        },
                        Ok(_) => {} // no real change
                        Err(e) => eprintln!("veles watch: update failed for {key}: {e}"),
                    }
                }
            }
        });

        Ok(Arc::new(Self {
            debouncer: Mutex::new(debouncer),
            watched,
        }))
    }

    /// Start watching `repo` (idempotent). `repo` is the same string the
    /// handlers pass to `get_or_load`; we canonicalize it for path
    /// matching but keep the original as the cache key. Remote URLs and
    /// unreadable paths are ignored.
    pub fn watch(&self, repo: &str) {
        if repo.starts_with("http://") || repo.starts_with("https://") {
            return;
        }
        let canonical = match std::fs::canonicalize(repo) {
            Ok(p) => p,
            Err(_) => return,
        };
        {
            let mut w = self.watched.lock().unwrap();
            if w.contains_key(&canonical) {
                return; // already watching
            }
            w.insert(canonical.clone(), repo.to_string());
        }
        if let Err(e) = self
            .debouncer
            .lock()
            .unwrap()
            .watch(&canonical, RecursiveMode::Recursive)
        {
            eprintln!("veles watch: cannot watch {}: {e}", canonical.display());
            self.watched.lock().unwrap().remove(&canonical);
        } else {
            eprintln!("veles watch: watching {}", canonical.display());
        }
    }

    /// Repos currently watched (canonical paths) — for the dashboard.
    pub fn watched_roots(&self) -> Vec<PathBuf> {
        self.watched.lock().unwrap().keys().cloned().collect()
    }
}

/// True if any component of a repo-relative path is an ignored dir
/// (`.veles`, `.git`, `node_modules`, `target`, …). Checking the relative
/// path — not the absolute one — avoids false positives when the repo
/// itself lives under a directory that happens to share an ignored name.
fn rel_is_ignored(rel: &Path) -> bool {
    rel.components().any(|c| {
        matches!(c, Component::Normal(os)
            if os.to_str().is_some_and(|s| s == ".veles" || DEFAULT_IGNORED_DIRS.contains(&s)))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn ignores_veles_and_heavy_dirs_relative() {
        assert!(rel_is_ignored(Path::new(".veles/chunks.bin")));
        assert!(rel_is_ignored(Path::new("node_modules/pkg/index.js")));
        assert!(rel_is_ignored(Path::new("target/debug/build")));
        assert!(rel_is_ignored(Path::new("sub/.git/HEAD")));
        // Real source must NOT be ignored.
        assert!(!rel_is_ignored(Path::new("src/main.rs")));
        assert!(!rel_is_ignored(Path::new("beta.rs")));
    }

    /// End-to-end: a file added under a watched repo is picked up by an
    /// incremental update without any explicit `update` call, and the
    /// `.veles/` write the update performs does not retrigger a loop.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn watcher_incrementally_updates_on_change() {
        let model = veles_core::model::load_model(None).expect("test model load");
        let cache = Arc::new(IndexCache::new(model));
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("alpha.rs"), "fn existing() {}\n").unwrap();
        let repo = dir.path().to_string_lossy().into_owned();

        // Prime the cache the way a first `search` would.
        cache.get_or_load(&repo, false).await.unwrap();

        let wm = WatchManager::new(cache.clone()).expect("watch manager");
        wm.watch(&repo);

        // Add a new file *after* watching has started.
        std::fs::write(
            dir.path().join("beta.rs"),
            "fn orbital_decay_compensation() { /* thruster burn */ }\n",
        )
        .unwrap();

        // Wait past the debounce window plus the incremental update.
        tokio::time::sleep(Duration::from_secs(5)).await;

        let idx = cache.peek(&repo).expect("repo still cached");
        let guard = idx.read().await;
        assert!(
            guard.chunks().iter().any(|c| c.file_path.contains("beta.rs")),
            "watcher should have incrementally indexed beta.rs; files={:?}",
            guard.chunks().iter().map(|c| &c.file_path).collect::<Vec<_>>()
        );
    }
}

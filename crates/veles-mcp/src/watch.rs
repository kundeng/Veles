//! Hidden per-repository coordinator for automatic MCP workspace indexing.
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
//! Every MCP process attempts the repository-local writer lock. Exactly one
//! process watches and publishes updates for a repository; concurrent MCP
//! processes read committed generations and can take over after the writer
//! exits. Different repositories coordinate independently.

use std::collections::{HashMap, HashSet};
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Result;
use notify::{RecommendedWatcher, RecursiveMode};
use notify_debouncer_full::{DebounceEventResult, Debouncer, RecommendedCache, new_debouncer};
use tokio::sync::mpsc;

use veles_core::cache::IndexCache;
use veles_core::lock::{self, LockOutcome, WriterLock};
use veles_core::walker::DEFAULT_IGNORED_DIRS;

/// Quiet window before a burst of filesystem events triggers one update.
const DEBOUNCE: Duration = Duration::from_millis(1500);

/// Owns the debouncer and the set of watched repos. Dropping it stops
/// all watches (the debouncer's worker thread shuts down with it).
pub struct WatchManager {
    debouncer: Mutex<Debouncer<RecommendedWatcher, RecommendedCache>>,
    repos: Arc<Mutex<HashMap<PathBuf, RepoRegistration>>>,
}

struct RepoRegistration {
    cache_key: String,
    writer: Option<WriterLock>,
    watching: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RepoRole {
    Writer,
    Reader,
}

/// Per-repository coordination snapshot. Consumed only by the optional
/// dashboard, so it is gated to avoid dead-code warnings in default builds.
#[cfg(feature = "dashboard")]
#[derive(Debug, Clone)]
pub struct RepoStatus {
    pub path: PathBuf,
    pub role: RepoRole,
    pub watching: bool,
}

impl WatchManager {
    /// Build the watcher and spawn the async update consumer. `events`
    /// receives a one-line message after each applied update (for the
    /// dashboard feed); it is fine for it to have no subscribers.
    pub fn new(
        cache: Arc<IndexCache>,
        events: tokio::sync::broadcast::Sender<String>,
    ) -> Result<Arc<Self>> {
        let (tx, mut rx) = mpsc::unbounded_channel::<PathBuf>();
        let repos: Arc<Mutex<HashMap<PathBuf, RepoRegistration>>> =
            Arc::new(Mutex::new(HashMap::new()));

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
        let repos_c = repos.clone();
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
                    let repos = repos_c.lock().unwrap();
                    paths
                        .iter()
                        .filter_map(|p| {
                            repos.iter().find_map(|(root, registration)| {
                                registration.writer.as_ref()?;
                                let rel = p.strip_prefix(root).ok()?;
                                if rel_is_ignored(rel) {
                                    None
                                } else {
                                    Some((root.clone(), registration.cache_key.clone()))
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
                            Ok(()) => {
                                let msg = format!(
                                    "updated {key} (+{} ~{} -{}, {} chunks)",
                                    report.added_files,
                                    report.modified_files,
                                    report.removed_files,
                                    report.total_chunks
                                );
                                eprintln!("veles watch: {msg}");
                                let _ = events.send(msg);
                            }
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
            repos,
        }))
    }

    /// Ensure repository-local coordination. Exactly one process can retain
    /// the destination writer lock; other MCP processes remain readers and
    /// retry on later access so takeover is automatic after a writer exits.
    pub fn ensure(&self, repo: &str) -> Result<RepoRole> {
        if repo.starts_with("http://") || repo.starts_with("https://") {
            return Ok(RepoRole::Reader);
        }
        let canonical = std::fs::canonicalize(repo)?;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);

        let mut repos = self.repos.lock().unwrap();
        if let Some(registration) = repos.get_mut(&canonical) {
            if registration.writer.is_some() {
                return Ok(RepoRole::Writer);
            }
            if let LockOutcome::Acquired(writer) =
                lock::try_acquire(&canonical, "automatic workspace indexing", now)?
            {
                registration.writer = Some(writer);
                registration.watching = self.start_watch(&canonical);
                return Ok(RepoRole::Writer);
            }
            return Ok(RepoRole::Reader);
        }

        let (writer, role) =
            match lock::try_acquire(&canonical, "automatic workspace indexing", now)? {
                LockOutcome::Acquired(writer) => (Some(writer), RepoRole::Writer),
                LockOutcome::Held { .. } => (None, RepoRole::Reader),
            };
        let watching = writer.is_some() && self.start_watch(&canonical);
        repos.insert(
            canonical.clone(),
            RepoRegistration {
                cache_key: canonical.to_string_lossy().into_owned(),
                writer,
                watching,
            },
        );
        Ok(role)
    }

    fn start_watch(&self, canonical: &Path) -> bool {
        if let Err(e) = self
            .debouncer
            .lock()
            .unwrap()
            .watch(canonical, RecursiveMode::Recursive)
        {
            eprintln!("veles watch: cannot watch {}: {e}", canonical.display());
            false
        } else {
            eprintln!("veles watch: watching {}", canonical.display());
            true
        }
    }

    #[cfg(feature = "dashboard")]
    pub fn statuses(&self) -> Vec<RepoStatus> {
        self.repos
            .lock()
            .unwrap()
            .iter()
            .map(|(path, registration)| RepoStatus {
                path: path.clone(),
                role: if registration.writer.is_some() {
                    RepoRole::Writer
                } else {
                    RepoRole::Reader
                },
                watching: registration.watching,
            })
            .collect()
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
        let repo = std::fs::canonicalize(dir.path())
            .unwrap()
            .to_string_lossy()
            .into_owned();

        // Prime the cache the way a first `search` would.
        cache.get_or_load(&repo, false).await.unwrap();

        let (events, _rx) = tokio::sync::broadcast::channel(16);
        let wm = WatchManager::new(cache.clone(), events).expect("watch manager");
        assert_eq!(wm.ensure(&repo).unwrap(), RepoRole::Writer);

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
            guard
                .chunks()
                .iter()
                .any(|c| c.file_path.contains("beta.rs")),
            "watcher should have incrementally indexed beta.rs; files={:?}",
            guard
                .chunks()
                .iter()
                .map(|c| &c.file_path)
                .collect::<Vec<_>>()
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn coordination_is_per_repository() {
        let model = veles_core::model::load_model(None).expect("test model load");
        let cache_a = Arc::new(IndexCache::new(model.clone()));
        let cache_b = Arc::new(IndexCache::new(model));
        let (events_a, _) = tokio::sync::broadcast::channel(16);
        let (events_b, _) = tokio::sync::broadcast::channel(16);
        let first = WatchManager::new(cache_a, events_a).unwrap();
        let second = WatchManager::new(cache_b, events_b).unwrap();
        let repo_a = tempfile::tempdir().unwrap();
        let repo_b = tempfile::tempdir().unwrap();

        assert_eq!(
            first.ensure(repo_a.path().to_str().unwrap()).unwrap(),
            RepoRole::Writer
        );
        assert_eq!(
            second.ensure(repo_a.path().to_str().unwrap()).unwrap(),
            RepoRole::Reader
        );
        assert_eq!(
            second.ensure(repo_b.path().to_str().unwrap()).unwrap(),
            RepoRole::Writer
        );

        first
            .repos
            .lock()
            .unwrap()
            .remove(&std::fs::canonicalize(repo_a.path()).unwrap());
        assert_eq!(
            second.ensure(repo_a.path().to_str().unwrap()).unwrap(),
            RepoRole::Writer,
            "a reader should take over after the previous writer releases the repo lock"
        );
    }
}

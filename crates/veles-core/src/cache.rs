//! Process-wide lockfree-ish cache of loaded `VelesIndex`es.
//!
//! Shared between the MCP and gRPC servers (and anything else that
//! wants to serve searches from in-memory indexes without re-walking
//! `<repo>/.veles` per request).
//!
//! Design notes:
//!
//! - **Storage**: `DashMap<String, CacheEntry>` — sharded internal
//!   locks, so concurrent operations on different repos never contend.
//!   The "lockfree" label is the practical kind: contention is bounded
//!   to a single shard, not the whole map.
//!
//! - **Per-index synchronization**: each cache entry holds an
//!   `Arc<RwLock<VelesIndex>>`. Read-only operations (search, defs,
//!   refs, ...) take a shared read lock; `update_from_path` takes the
//!   exclusive write lock. Two clients searching the same repo proceed
//!   in parallel; an `update` briefly blocks readers.
//!
//! - **Build deduplication**: a `OnceCell` lives inside every entry,
//!   so several concurrent loaders of the same repo cooperate — one
//!   thread does the (slow) walk + embed + load, the others await its
//!   result. No wasted duplicate builds.
//!
//! - **LRU eviction**: each entry stores an `AtomicU64 last_access`.
//!   Every hit / miss bumps a global counter and writes it into the
//!   entry. When the cache exceeds capacity we scan and evict the
//!   smallest. O(N) but N is small (≤ 16 in practice).
//!
//! Tests assume the eviction is "eventually correct", not strictly LRU
//! under contention — two threads concurrently bumping `last_access`
//! on different entries may finish in arbitrary order. For the actual
//! workload (10-slot cache, ~tens of repos per session) this is fine.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Result, bail};
use dashmap::DashMap;
use model2vec_rs::model::StaticModel;
use tokio::sync::{OnceCell, RwLock};

use crate::VelesIndex;
use crate::persist;

/// How many `VelesIndex` entries the cache keeps before evicting LRU.
pub const DEFAULT_CACHE_SIZE: usize = 10;

/// Mtime (Unix secs) of `<repo>/.veles/manifest.json`, or `0` if the repo is
/// not a local path with a persisted index. The manifest is rewritten on
/// every persisted update, so its mtime is a cheap "has this index changed
/// on disk" signal that needs no JSON parse.
fn manifest_mtime_secs(repo: &str) -> u64 {
    let manifest = persist::index_dir_for(Path::new(repo)).join("manifest.json");
    std::fs::metadata(&manifest)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// One cached index plus the metadata we need for LRU + build dedup.
struct CacheEntry {
    /// Initialised lazily: the first `get_or_load` for this repo
    /// triggers the build; concurrent callers await the same future
    /// rather than launching their own. `Arc` on the outside so we
    /// can hand it back to callers cheaply after init.
    cell: Arc<OnceCell<Arc<RwLock<VelesIndex>>>>,
    /// Monotonic counter snapshot of the last hit / miss touching this
    /// entry. Newer = larger. Updated lockfree via relaxed store.
    last_access: AtomicU64,
    /// `.veles/manifest.json` mtime (Unix secs) observed *at the moment
    /// this index was loaded from disk*; `0` when unknown (git URL, fresh
    /// in-memory build, or never loaded). Written once inside the build
    /// closure — never on a cache hit — so [`IndexCache::refresh_if_stale`]
    /// can tell whether another process has since rewritten the index.
    /// `Arc` so the build closure can capture and set it.
    loaded_mtime: Arc<AtomicU64>,
}

/// Lockfree-ish process cache of loaded indexes.
pub struct IndexCache {
    entries: DashMap<String, CacheEntry>,
    model: StaticModel,
    capacity: usize,
    /// Global monotonic clock for LRU ordering.
    counter: AtomicU64,
}

impl IndexCache {
    /// Build a cache with the default capacity (`DEFAULT_CACHE_SIZE`).
    pub fn new(model: StaticModel) -> Self {
        Self::with_capacity(model, DEFAULT_CACHE_SIZE)
    }

    /// Build a cache with an explicit capacity.
    pub fn with_capacity(model: StaticModel, capacity: usize) -> Self {
        Self {
            entries: DashMap::with_capacity(capacity.max(1)),
            model,
            capacity: capacity.max(1),
            counter: AtomicU64::new(0),
        }
    }

    /// Get or lazily build the `VelesIndex` for `repo`.
    ///
    /// Returns an `Arc<RwLock<VelesIndex>>` the caller can `.read()` or
    /// `.write()` independently of the cache lock. Multiple concurrent
    /// loaders of the same repo share a single in-flight build via the
    /// internal `OnceCell`.
    ///
    /// `repo` is either a local directory path or an `https://` git URL.
    /// Local paths prefer the persisted `.veles/` index when one exists
    /// (fast load) and fall back to a fresh in-memory build otherwise.
    pub async fn get_or_load(
        &self,
        repo: &str,
        include_text_files: bool,
    ) -> Result<Arc<RwLock<VelesIndex>>> {
        // Take or create the cell, update LRU timestamp. The shard lock
        // is held only for this `entry()` call — building runs outside.
        let (cell, loaded_mtime) = {
            let entry = self
                .entries
                .entry(repo.to_string())
                .or_insert_with(|| CacheEntry {
                    cell: Arc::new(OnceCell::new()),
                    last_access: AtomicU64::new(0),
                    loaded_mtime: Arc::new(AtomicU64::new(0)),
                });
            entry.last_access.store(self.tick(), Ordering::Relaxed);
            (entry.cell.clone(), entry.loaded_mtime.clone())
        };

        // Initialise the cell. `get_or_try_init` ensures exactly one
        // caller runs the closure; others await its result. On error
        // the cell stays empty so the next call retries. The manifest
        // mtime is recorded *inside* the closure so it reflects the index
        // we actually loaded — a cache hit must not refresh it, or a
        // follower could never detect that the owner rewrote the index.
        let index = cell
            .get_or_try_init(|| async {
                let built = self.build_index(repo, include_text_files)?;
                loaded_mtime.store(manifest_mtime_secs(repo), Ordering::Relaxed);
                anyhow::Ok(Arc::new(RwLock::new(built)))
            })
            .await
            .map_err(|e| anyhow::anyhow!("failed to load {repo}: {e}"))?;

        // Opportunistic LRU eviction. Done after insert (not before) so
        // we never evict a fresh entry that's about to be returned.
        if self.entries.len() > self.capacity {
            self.evict_lru();
        }

        Ok(index.clone())
    }

    /// Look up `repo` without building. Returns `Some` only if the
    /// cell has been initialised (i.e. a previous `get_or_load` for
    /// this repo has completed successfully). Used by callers that
    /// want to gate on "is this repo bootstrapped yet?" without
    /// triggering an expensive build — e.g. the gRPC `GetStats` RPC.
    pub fn peek(&self, repo: &str) -> Option<Arc<RwLock<VelesIndex>>> {
        let entry = self.entries.get(repo)?;
        entry.last_access.store(self.tick(), Ordering::Relaxed);
        entry.cell.get().cloned()
    }

    /// Drop the cached entry for `repo` if present. Useful for tests
    /// and explicit invalidation.
    pub fn invalidate(&self, repo: &str) -> bool {
        self.entries.remove(repo).is_some()
    }

    /// Drop the cached entry for `repo` iff its on-disk `.veles/` index has
    /// been rewritten since we loaded it (manifest mtime advanced). Returns
    /// `true` when it invalidated — the next [`get_or_load`] then reloads the
    /// fresh persisted index (no re-embed). Cheap: one `stat` and an atomic
    /// read, no deserialisation.
    ///
    /// This is how a *follower* MCP process — one that deliberately does not
    /// run a filesystem watcher because another process owns updating this
    /// repo — still serves fresh results: it reload-on-reads instead of
    /// re-embedding. A no-op for repos that aren't cached, aren't loaded
    /// yet, or have no manifest (git URLs / fresh in-memory builds).
    pub fn refresh_if_stale(&self, repo: &str) -> bool {
        let loaded = {
            let Some(entry) = self.entries.get(repo) else {
                return false;
            };
            // Only meaningful once the index is actually loaded.
            if entry.cell.get().is_none() {
                return false;
            }
            entry.loaded_mtime.load(Ordering::Relaxed)
        };
        let disk = manifest_mtime_secs(repo);
        // `disk == 0` means no manifest to compare against — leave as-is.
        // `loaded == 0` with a real disk mtime means we loaded a fresh
        // in-memory build that has since been persisted by the owner;
        // treat that as stale too so the follower picks up the real index.
        if disk != 0 && disk > loaded {
            return self.invalidate(repo);
        }
        false
    }

    /// Keys (repo identifiers) of all currently-cached repos. Used by the
    /// MCP dashboard to enumerate what this server has loaded.
    pub fn loaded_repos(&self) -> Vec<String> {
        self.entries.iter().map(|e| e.key().clone()).collect()
    }

    /// Current number of cached repos.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// True if no repos are cached.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Configured capacity.
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    fn tick(&self) -> u64 {
        self.counter.fetch_add(1, Ordering::Relaxed)
    }

    /// Walk every entry to find the smallest `last_access` and drop it.
    /// O(N) on the cache size — fine because N is bounded to `capacity`
    /// (default 10).
    fn evict_lru(&self) {
        let oldest = self
            .entries
            .iter()
            .min_by_key(|e| e.value().last_access.load(Ordering::Relaxed))
            .map(|e| e.key().clone());
        if let Some(key) = oldest {
            self.entries.remove(&key);
        }
    }

    /// Build a `VelesIndex` for `repo`. Synchronous, CPU-bound — runs
    /// inside the `OnceCell::get_or_try_init` future. A future refactor
    /// (`spawn_blocking`) can offload this from the tokio worker; for
    /// now it matches the legacy MCP / gRPC behaviour.
    fn build_index(&self, repo: &str, include_text_files: bool) -> Result<VelesIndex> {
        let model = self.model.clone();
        let path = Path::new(repo);

        if path.is_dir() {
            // Persisted index wins over a fresh build: keeps subsequent
            // `stats` / `status` / `update` consistent with the on-disk
            // chunk count, and avoids re-embedding on every cold start.
            // Fall back to a fresh build if load fails (incompatible
            // format, missing sidecar files, ...).
            if persist::index_exists(path) {
                match VelesIndex::load(path, model.clone()) {
                    Ok(idx) => return Ok(idx),
                    Err(_) => {
                        // load failed — fall through to fresh build
                    }
                }
            }
            VelesIndex::from_path(path, Some(model), None, include_text_files)
        } else if repo.starts_with("https://") || repo.starts_with("http://") {
            VelesIndex::from_git(repo, None, Some(model), include_text_files)
        } else {
            bail!("Invalid repo: must be a local directory or https:// URL")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_model() -> StaticModel {
        crate::model::load_model(None).expect("test model load")
    }

    #[tokio::test]
    async fn caches_same_repo_across_calls() {
        let cache = IndexCache::new(test_model());
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.rs"), "fn hello() {}\n").unwrap();
        let repo = dir.path().to_string_lossy().into_owned();

        let a = cache.get_or_load(&repo, false).await.unwrap();
        let b = cache.get_or_load(&repo, false).await.unwrap();
        assert!(Arc::ptr_eq(&a, &b), "cache miss on repeat lookup");
        assert_eq!(cache.len(), 1);
    }

    #[tokio::test]
    async fn evicts_lru_when_over_capacity() {
        let cache = IndexCache::with_capacity(test_model(), 2);
        let dirs: Vec<_> = (0..3)
            .map(|i| {
                let d = tempfile::tempdir().unwrap();
                std::fs::write(d.path().join("a.rs"), format!("fn fn_{i}() {{}}\n")).unwrap();
                d
            })
            .collect();
        let paths: Vec<String> = dirs
            .iter()
            .map(|d| d.path().to_string_lossy().into_owned())
            .collect();

        // Load three repos into a 2-slot cache.
        let _ = cache.get_or_load(&paths[0], false).await.unwrap();
        let _ = cache.get_or_load(&paths[1], false).await.unwrap();
        // Re-touch [0] so it's newer than [1].
        let _ = cache.get_or_load(&paths[0], false).await.unwrap();
        // Inserting [2] should evict the LRU — which is now [1].
        let _ = cache.get_or_load(&paths[2], false).await.unwrap();

        assert_eq!(cache.len(), 2);
        // [1] was evicted; [0] and [2] remain.
        assert!(cache.entries.contains_key(&paths[0]));
        assert!(cache.entries.contains_key(&paths[2]));
        assert!(!cache.entries.contains_key(&paths[1]));
    }

    #[tokio::test]
    async fn refresh_if_stale_reloads_when_manifest_advances() {
        let model = test_model();
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.rs"), "fn hello() {}\n").unwrap();
        let repo = dir.path().to_string_lossy().into_owned();

        // Persist a real index so get_or_load takes the load-from-disk path
        // (and so a manifest mtime exists to compare against).
        {
            let mut idx =
                VelesIndex::from_path(dir.path(), Some(model.clone()), None, false).unwrap();
            idx.save(dir.path()).unwrap();
        }

        let cache = IndexCache::new(model);
        let _ = cache.get_or_load(&repo, false).await.unwrap();
        // Nothing changed on disk since load → not stale.
        assert!(!cache.refresh_if_stale(&repo), "should not be stale right after load");

        // Simulate the owner rewriting the index: bump the manifest mtime.
        // Manifest mtime is second-granularity, so wait past a 1s boundary.
        std::thread::sleep(std::time::Duration::from_millis(1100));
        let manifest = dir.path().join(".veles").join("manifest.json");
        let bytes = std::fs::read(&manifest).unwrap();
        std::fs::write(&manifest, &bytes).unwrap();

        // Now the on-disk index is newer than what we loaded → invalidate.
        assert!(cache.refresh_if_stale(&repo), "should detect the advanced manifest");
        assert!(cache.peek(&repo).is_none(), "stale entry should have been dropped");
        // Idempotent: nothing cached now → no-op.
        assert!(!cache.refresh_if_stale(&repo));
    }

    #[tokio::test]
    async fn invalidate_removes_entry() {
        let cache = IndexCache::new(test_model());
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.rs"), "fn x() {}\n").unwrap();
        let repo = dir.path().to_string_lossy().into_owned();
        let _ = cache.get_or_load(&repo, false).await.unwrap();
        assert!(cache.invalidate(&repo));
        assert!(cache.is_empty());
        // Idempotent — second invalidate is a no-op.
        assert!(!cache.invalidate(&repo));
    }
}

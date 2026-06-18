//! Reader leases — how a coordinator daemon learns whether anyone is still
//! reading its repository, so it can idle-exit instead of living forever.
//!
//! A *reader* (an MCP process consuming the committed index) drops a lease
//! file at `<repo>/.veles/readers/<uuid>` and refreshes its mtime on a fixed
//! interval. The *coordinator* counts leases whose mtime is fresh (within a
//! multiple of that interval); when none are fresh for a grace window it
//! releases the writer lock and exits.
//!
//! Liveness is **observed, not declared** — the same philosophy as the
//! `flock` writer lock. A reader that crashes simply stops refreshing; its
//! lease ages out and is swept on the next count. No clean-shutdown handshake
//! is required, so a `SIGKILL`ed agent never strands a daemon.
//!
//! The lease's existence and mtime are the entire protocol; the file is empty.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result};

use crate::persist::index_dir_for;

/// Default reader refresh cadence (overridable via `VELES_LEASE_REFRESH_SECS`).
pub const REFRESH_INTERVAL: Duration = Duration::from_secs(15);

/// Default staleness window — a lease older than this is a dead reader
/// (overridable via `VELES_LEASE_TTL_SECS`). Three refresh intervals tolerates
/// a missed beat or two without false eviction; keep TTL > refresh interval.
pub const FRESH_TTL: Duration = Duration::from_secs(45);

fn secs_env(var: &str, default: Duration) -> Duration {
    std::env::var(var)
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .map(Duration::from_secs)
        .unwrap_or(default)
}

/// Effective refresh cadence (honors `VELES_LEASE_REFRESH_SECS`).
pub fn refresh_interval() -> Duration {
    secs_env("VELES_LEASE_REFRESH_SECS", REFRESH_INTERVAL)
}

/// Effective staleness window (honors `VELES_LEASE_TTL_SECS`).
pub fn fresh_ttl() -> Duration {
    secs_env("VELES_LEASE_TTL_SECS", FRESH_TTL)
}

fn readers_dir(repo_root: &Path) -> PathBuf {
    index_dir_for(repo_root).join("readers")
}

/// A held reader lease. The file lives as long as this guard; `Drop` removes
/// it for a clean shutdown (a crash leaves it to age out instead).
pub struct ReaderLease {
    path: PathBuf,
}

impl ReaderLease {
    /// Create a fresh lease for `repo_root`. `id` should be unique per reader
    /// (e.g. a uuid or `pid`-derived token); two leases with the same id from
    /// the same reader are fine — the second just refreshes the first.
    pub fn acquire(repo_root: &Path, id: &str) -> Result<Self> {
        let dir = readers_dir(repo_root);
        fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
        let path = dir.join(id);
        let lease = Self { path };
        lease.refresh()?;
        Ok(lease)
    }

    /// Bump the lease's mtime to now. Readers call this on `REFRESH_INTERVAL`.
    pub fn refresh(&self) -> Result<()> {
        // Truncate-write touches both mtime and ensures the file exists even
        // if it was swept while we were briefly idle.
        fs::write(&self.path, b"").with_context(|| format!("refresh lease {}", self.path.display()))
    }

    /// Path to this lease file (diagnostics/tests).
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for ReaderLease {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

/// Count leases whose mtime is within `FRESH_TTL`, sweeping any that are
/// stale. Returns the number of live readers the coordinator should keep
/// serving. A missing/empty `readers/` directory means zero readers.
pub fn count_fresh(repo_root: &Path) -> usize {
    let dir = readers_dir(repo_root);
    let Ok(entries) = fs::read_dir(&dir) else {
        return 0;
    };
    let now = SystemTime::now();
    let ttl = fresh_ttl();
    let mut fresh = 0usize;
    for entry in entries.filter_map(Result::ok) {
        let path = entry.path();
        let stale = match entry.metadata().and_then(|m| m.modified()) {
            Ok(mtime) => now
                .duration_since(mtime)
                .map(|age| age > ttl)
                .unwrap_or(false), // mtime in the future: treat as fresh
            Err(_) => true,
        };
        if stale {
            let _ = fs::remove_file(&path); // sweep dead readers
        } else {
            fresh += 1;
        }
    }
    fresh
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lease_counts_then_drops_to_zero_on_release() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(count_fresh(dir.path()), 0);

        let lease = ReaderLease::acquire(dir.path(), "reader-a").unwrap();
        assert_eq!(count_fresh(dir.path()), 1);

        // A second reader.
        let lease2 = ReaderLease::acquire(dir.path(), "reader-b").unwrap();
        assert_eq!(count_fresh(dir.path()), 2);

        drop(lease);
        assert_eq!(count_fresh(dir.path()), 1);
        drop(lease2);
        assert_eq!(count_fresh(dir.path()), 0);
    }

    #[test]
    fn stale_lease_is_swept() {
        let dir = tempfile::tempdir().unwrap();
        let lease = ReaderLease::acquire(dir.path(), "old").unwrap();
        // Backdate the lease well past FRESH_TTL.
        let old = SystemTime::now() - (FRESH_TTL + Duration::from_secs(30));
        let times = fs::FileTimes::new().set_modified(old).set_accessed(old);
        let f = fs::File::options().write(true).open(lease.path()).unwrap();
        f.set_times(times).unwrap();

        assert_eq!(count_fresh(dir.path()), 0, "stale lease must not count");
        assert!(!lease.path().exists(), "stale lease must be swept");
    }
}

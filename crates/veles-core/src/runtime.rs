//! Coordinator runtime state — informational discovery, never authoritative.
//!
//! A running coordinator publishes `<repo>/.veles/runtime.json` describing
//! itself: pid, dashboard URL, current generation, state. It powers `status`
//! and "open the dashboard for repo X" without a fixed port — there is no
//! fixed port, so discovery resolves through this file instead.
//!
//! Ownership is decided by the writer `flock`, **not** by this file. Runtime
//! state may be stale (a crashed daemon leaves a file behind); readers and
//! tools must treat it as a hint, never as proof that a daemon is alive.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::persist::index_dir_for;

const RUNTIME_FILE: &str = "runtime.json";

/// Best-effort snapshot of a coordinator. All fields are advisory.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeState {
    /// OS process id of the coordinator daemon.
    pub pid: u32,
    /// Unix epoch seconds when the daemon started.
    pub started_at: i64,
    /// Dashboard URL, if the daemon is serving one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dashboard_url: Option<String>,
    /// Latest published generation id, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generation: Option<u64>,
    /// Coarse state label: "preparing", "watching", ...
    pub state: String,
}

fn runtime_path(repo_root: &Path) -> PathBuf {
    index_dir_for(repo_root).join(RUNTIME_FILE)
}

/// Write (or overwrite) the runtime file for `repo_root`.
pub fn write(repo_root: &Path, state: &RuntimeState) -> Result<()> {
    let dir = index_dir_for(repo_root);
    fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    let path = runtime_path(repo_root);
    let json = serde_json::to_string_pretty(state)?;
    fs::write(&path, json).with_context(|| format!("write {}", path.display()))
}

/// Read the runtime file, or `None` if absent/unparseable.
pub fn read(repo_root: &Path) -> Option<RuntimeState> {
    let data = fs::read_to_string(runtime_path(repo_root)).ok()?;
    serde_json::from_str(&data).ok()
}

/// Remove the runtime file (on clean coordinator shutdown).
pub fn remove(repo_root: &Path) {
    let _ = fs::remove_file(runtime_path(repo_root));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips() {
        let dir = tempfile::tempdir().unwrap();
        assert!(read(dir.path()).is_none());
        let state = RuntimeState {
            pid: 4321,
            started_at: 1_700_000_000,
            dashboard_url: Some("http://127.0.0.1:5051".into()),
            generation: Some(42),
            state: "watching".into(),
        };
        write(dir.path(), &state).unwrap();
        let got = read(dir.path()).expect("runtime present");
        assert_eq!(got.pid, 4321);
        assert_eq!(got.generation, Some(42));
        assert_eq!(got.dashboard_url.as_deref(), Some("http://127.0.0.1:5051"));
        remove(dir.path());
        assert!(read(dir.path()).is_none());
    }
}

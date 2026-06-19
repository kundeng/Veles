//! How a user-facing folder maps to the directory whose `.veles/` actually
//! holds its index.
//!
//! Most folders index *in place*: you point veles at a repo and its index
//! lives in `<repo>/.veles/`. But some folders are full of verbose JSON
//! (line-delimited records, agent transcripts, exports) that (a) index poorly
//! raw and (b) aren't ours to write into — e.g. `~/.claude/projects`. For
//! those, veles indexes a *distilled shadow* instead: it derives readable text
//! ([`crate::distill`]) into a veles-owned state directory and indexes that.
//! The source folder is never touched.
//!
//! Two entry points keep writers and readers in agreement:
//! - [`index_root`] (read-only) — *where is the index right now?* Used by every
//!   reader (lease, lock probe, generation read). Returns the folder itself
//!   unless a plan already says "distill".
//! - [`establish_plan`] (writer-only) — detect the mode (once), persist it, and
//!   return the full [`Plan`]. Only the coordinator calls this. Readers never
//!   write, so they never race to create a plan.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// How a folder is ingested.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IngestMode {
    /// Index the folder where it sits (`<folder>/.veles/`). Default.
    InPlace,
    /// Distill verbose JSON into a veles-owned shadow dir and index that.
    Distill,
}

/// The resolved ingest plan for a folder.
#[derive(Debug, Clone)]
pub struct Plan {
    pub mode: IngestMode,
    /// The folder the user actually added.
    pub source: PathBuf,
    /// For `Distill`, where derived `.md` are written and indexed. Equals
    /// `index_root`. `None` for `InPlace`.
    pub derived_dir: Option<PathBuf>,
    /// The directory whose `.veles/` holds the index (what persist/lock/lease
    /// operate on). `source` for `InPlace`, the shadow derived dir otherwise.
    pub index_root: PathBuf,
}

#[derive(Serialize, Deserialize)]
struct PersistedPlan {
    mode: IngestMode,
}

/// veles' per-user state directory (`$VELES_STATE_DIR`, else
/// `$XDG_STATE_HOME/veles`, else `~/.local/state/veles`).
pub fn state_dir() -> PathBuf {
    if let Some(d) = std::env::var_os("VELES_STATE_DIR") {
        return PathBuf::from(d);
    }
    if let Some(d) = std::env::var_os("XDG_STATE_HOME") {
        return PathBuf::from(d).join("veles");
    }
    if let Some(home) = std::env::var_os("HOME") {
        return PathBuf::from(home).join(".local/state/veles");
    }
    std::env::temp_dir().join("veles")
}

/// Stable shadow root for `folder`, keyed by a hash of its canonical path so
/// the same folder always maps to the same shadow regardless of caller cwd.
fn shadow_root(folder: &Path) -> PathBuf {
    let canonical = std::fs::canonicalize(folder).unwrap_or_else(|_| folder.to_path_buf());
    let key = blake3::hash(canonical.to_string_lossy().as_bytes()).to_hex();
    state_dir().join("folders").join(&key[..16])
}

fn derived_dir_of(folder: &Path) -> PathBuf {
    shadow_root(folder).join("derived")
}

fn plan_path(folder: &Path) -> PathBuf {
    shadow_root(folder).join("plan.json")
}

fn read_persisted(folder: &Path) -> Option<IngestMode> {
    let bytes = std::fs::read(plan_path(folder)).ok()?;
    serde_json::from_slice::<PersistedPlan>(&bytes)
        .ok()
        .map(|p| p.mode)
}

/// Read-only: the directory whose `.veles/` currently holds `folder`'s index.
///
/// Fast path: honor the persisted plan once the coordinator has written one.
/// Before that exists, fall back to read-only detection so readers and the
/// writer agree on the shadow location from the very first access — this is
/// what keeps a non-owned source folder (e.g. `~/.claude/projects`) from ever
/// getting a stray `.veles/`. Detection only walks a handful of files and
/// never writes, so it is safe to call on every request.
pub fn index_root(folder: &Path) -> PathBuf {
    match read_persisted(folder) {
        Some(IngestMode::Distill) => derived_dir_of(folder),
        Some(IngestMode::InPlace) => folder.to_path_buf(),
        None => {
            if crate::distill::looks_like_verbose_json(folder) {
                derived_dir_of(folder)
            } else {
                folder.to_path_buf()
            }
        }
    }
}

/// Writer-only: resolve and **persist** the ingest plan for `folder`.
///
/// If a plan already exists it is honored (stable across restarts). Otherwise
/// the mode is auto-detected ([`crate::distill::looks_like_verbose_json`]) and
/// persisted for *either* mode, so subsequent reader lookups are O(1) instead
/// of re-detecting. The coordinator calls this once when it starts owning a
/// folder. The persisted plan lives in veles' shadow state, never in `folder`.
pub fn establish_plan(folder: &Path) -> std::io::Result<Plan> {
    let mode = match read_persisted(folder) {
        Some(m) => m,
        None => {
            let mode = if crate::distill::looks_like_verbose_json(folder) {
                IngestMode::Distill
            } else {
                IngestMode::InPlace
            };
            let root = shadow_root(folder);
            std::fs::create_dir_all(&root)?;
            let bytes = serde_json::to_vec_pretty(&PersistedPlan { mode })?;
            std::fs::write(plan_path(folder), bytes)?;
            mode
        }
    };
    Ok(match mode {
        IngestMode::InPlace => Plan {
            mode,
            source: folder.to_path_buf(),
            derived_dir: None,
            index_root: folder.to_path_buf(),
        },
        IngestMode::Distill => {
            let derived = derived_dir_of(folder);
            std::fs::create_dir_all(&derived)?;
            Plan {
                mode,
                source: folder.to_path_buf(),
                derived_dir: Some(derived.clone()),
                index_root: derived,
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// Serializes the env-var mutation below so parallel tests don't clobber
    /// the process-global `VELES_STATE_DIR`.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Point state_dir() at a temp location for the duration of a test.
    struct StateGuard {
        _tmp: tempfile::TempDir,
        _lock: std::sync::MutexGuard<'static, ()>,
    }
    impl StateGuard {
        fn new() -> Self {
            let lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
            let tmp = tempfile::tempdir().unwrap();
            unsafe { std::env::set_var("VELES_STATE_DIR", tmp.path()) };
            StateGuard {
                _tmp: tmp,
                _lock: lock,
            }
        }
    }
    impl Drop for StateGuard {
        fn drop(&mut self) {
            unsafe { std::env::remove_var("VELES_STATE_DIR") };
        }
    }

    #[test]
    fn plain_folder_is_in_place_indexed_at_source() {
        let _g = StateGuard::new();
        let repo = tempfile::tempdir().unwrap();
        fs::write(repo.path().join("main.rs"), "fn main() {}").unwrap();

        let plan = establish_plan(repo.path()).unwrap();
        assert_eq!(plan.mode, IngestMode::InPlace);
        assert_eq!(plan.index_root, repo.path());
        assert!(plan.derived_dir.is_none());
        // read path agrees
        assert_eq!(index_root(repo.path()), repo.path());
        // the plan is persisted in veles' shadow state, never in the source
        assert!(plan_path(repo.path()).exists());
        assert!(!repo.path().join(".veles").exists());
    }

    #[test]
    fn read_path_detects_before_a_plan_is_written() {
        let _g = StateGuard::new();
        let src = tempfile::tempdir().unwrap();
        for i in 0..10 {
            fs::write(src.path().join(format!("s{i}.jsonl")), "{}\n").unwrap();
        }
        // No establish_plan() yet — index_root must still resolve to the shadow
        // via read-only detection (no source pollution, no chicken-and-egg).
        let resolved = index_root(src.path());
        assert!(!resolved.starts_with(src.path()));
        assert_eq!(resolved, derived_dir_of(src.path()));
        assert!(!plan_path(src.path()).exists());
    }

    #[test]
    fn verbose_json_folder_distills_to_shadow() {
        let _g = StateGuard::new();
        let src = tempfile::tempdir().unwrap();
        for i in 0..10 {
            fs::write(
                src.path().join(format!("s{i}.jsonl")),
                "{\"message\":{\"content\":\"hi\"}}\n",
            )
            .unwrap();
        }
        let plan = establish_plan(src.path()).unwrap();
        assert_eq!(plan.mode, IngestMode::Distill);
        let derived = plan.derived_dir.unwrap();
        assert_eq!(plan.index_root, derived);
        // shadow lives outside the source folder
        assert!(!derived.starts_with(src.path()));
        // read path now resolves to the same shadow dir (plan persisted)
        assert_eq!(index_root(src.path()), derived);
    }

    #[test]
    fn plan_is_stable_across_calls() {
        let _g = StateGuard::new();
        let src = tempfile::tempdir().unwrap();
        for i in 0..10 {
            fs::write(src.path().join(format!("s{i}.jsonl")), "{}\n").unwrap();
        }
        let a = establish_plan(src.path()).unwrap().index_root;
        let b = establish_plan(src.path()).unwrap().index_root;
        assert_eq!(a, b);
    }
}

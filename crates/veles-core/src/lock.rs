//! Single-writer guarantee for an index destination.
//!
//! A *writer* (the `veles watch` daemon or a one-shot `veles transform`) must
//! be the only process mutating a given `<dest>/.veles/`. Two writers racing
//! on the same index directory would interleave their `save()`s and double
//! their transforms. We enforce "at most one writer per destination" with an
//! exclusive advisory lock on `<dest>/.veles/writer.lock`.
//!
//! Why `flock(2)` and not a PID file: a PID file goes **stale** on `kill -9`
//! or a panic, and you're left guessing whether the recorded PID is still
//! alive. `flock` is released by the **kernel** the instant the holding
//! process dies — crash, `SIGKILL`, power-cycle-then-reboot all leave the next
//! writer free to acquire cleanly, with no stale-lock cleanup logic. The lock
//! is held for the writer's whole lifetime by simply keeping the file open.
//!
//! Readers (`serve-mcp`) never take this lock — many readers + one writer is
//! the whole point. The lock is keyed on the *destination index*, so two
//! writers driving **different** dests run concurrently without false
//! contention; only a genuine same-dest collision is refused.
//!
//! The lock file's *contents* (pid, start time, an optional label) are written
//! purely for human diagnostics — `who holds dest X?`. The guarantee is the
//! `flock`, never the file's existence.

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::persist::index_dir_for;

const LOCK_FILE: &str = "writer.lock";

/// Result of trying to become the sole writer for a destination.
pub enum LockOutcome {
    /// We hold the lock. Keep the [`WriterLock`] alive for as long as you
    /// intend to write; dropping it (or the process dying) releases it.
    Acquired(WriterLock),
    /// Another live writer already owns this dest. `holder` is a best-effort,
    /// human-readable description read from the lock file (may be empty if the
    /// holder hadn't written its identity yet).
    Held { holder: String },
}

/// An held exclusive writer lock. The lock lives as long as this value: the
/// underlying file stays open, so the `flock` is held. Drop releases it
/// (explicitly on unix, and implicitly via `close(2)` on every platform).
pub struct WriterLock {
    path: PathBuf,
    file: File,
}

impl WriterLock {
    /// Path to the lock file this guard holds.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for WriterLock {
    fn drop(&mut self) {
        // Best-effort explicit release. Closing the fd (which happens right
        // after) also drops the flock, so this is belt-and-suspenders.
        #[cfg(unix)]
        {
            use std::os::unix::io::AsRawFd;
            unsafe {
                libc::flock(self.file.as_raw_fd(), libc::LOCK_UN);
            }
        }
    }
}

/// Try to become the sole writer for `dest_root` (the directory whose
/// `.veles/` index will be written). Non-blocking: returns
/// [`LockOutcome::Held`] immediately if another writer owns it rather than
/// waiting. `label` is recorded in the lock file for diagnostics (e.g. the
/// pipeline name); pass `""` if none.
///
/// `now_epoch_secs` is the wall-clock timestamp to record (caller-supplied so
/// this stays trivially testable).
pub fn try_acquire(dest_root: &Path, label: &str, now_epoch_secs: i64) -> Result<LockOutcome> {
    let dir = index_dir_for(dest_root);
    fs::create_dir_all(&dir).with_context(|| format!("create index dir {}", dir.display()))?;
    let path = dir.join(LOCK_FILE);

    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)
        .with_context(|| format!("open lock file {}", path.display()))?;

    match raw_try_lock_exclusive(&file) {
        Ok(true) => {
            // We hold it. Record our identity for `who holds this?` queries.
            let pid = std::process::id();
            let id = if label.is_empty() {
                format!("pid={pid} started_at={now_epoch_secs}\n")
            } else {
                format!("pid={pid} started_at={now_epoch_secs} label={label}\n")
            };
            // Truncate any stale holder text and write ours.
            let mut f = &file;
            f.set_len(0).ok();
            use std::io::Seek;
            (&mut f).seek(std::io::SeekFrom::Start(0)).ok();
            f.write_all(id.as_bytes())
                .with_context(|| format!("write lock identity to {}", path.display()))?;
            f.flush().ok();
            Ok(LockOutcome::Acquired(WriterLock { path, file }))
        }
        Ok(false) => {
            // Someone else holds it. Read their identity, best-effort.
            let mut holder = String::new();
            let _ = File::open(&path).and_then(|mut h| h.read_to_string(&mut holder));
            Ok(LockOutcome::Held {
                holder: holder.trim().to_string(),
            })
        }
        Err(e) => Err(e).with_context(|| format!("flock {}", path.display())),
    }
}

/// Non-blocking exclusive lock attempt. `Ok(true)` = acquired, `Ok(false)` =
/// already held by another live writer, `Err` = a real syscall failure.
#[cfg(unix)]
fn raw_try_lock_exclusive(file: &File) -> Result<bool> {
    use std::os::unix::io::AsRawFd;
    let fd = file.as_raw_fd();
    // SAFETY: `fd` is a valid open descriptor owned by `file` for the duration
    // of the call. flock with LOCK_NB never blocks.
    let rc = unsafe { libc::flock(fd, libc::LOCK_EX | libc::LOCK_NB) };
    if rc == 0 {
        return Ok(true);
    }
    let err = std::io::Error::last_os_error();
    match err.raw_os_error() {
        // Lock is held by someone else — the expected contention signal.
        Some(code) if code == libc::EWOULDBLOCK => Ok(false),
        _ => Err(err.into()),
    }
}

/// Non-unix fallback. We don't have a crash-safe advisory lock here, so we do
/// not pretend to: we always "acquire". The set-and-forget writer daemon runs
/// under a unix supervisor (launchd/systemd); Windows is reader-only in
/// practice. Documented limitation rather than a silent false guarantee.
#[cfg(not(unix))]
fn raw_try_lock_exclusive(_file: &File) -> Result<bool> {
    Ok(true)
}

/// Non-retaining probe: is a live writer currently holding `dest_root`'s lock?
///
/// Unlike [`try_acquire`], this never *keeps* the lock — it momentarily tries
/// the exclusive lock and, if it succeeds, immediately releases it. A reader
/// (e.g. an MCP server) uses this to decide whether to spawn a coordinator
/// without ever becoming the writer itself. Concurrent probers serialize for
/// sub-millisecond windows; a returned `false` only means "no writer right
/// now", so callers must tolerate a racing spawn (the writer lock is the real
/// arbiter — duplicate coordinators self-resolve on it).
///
/// On non-unix (no real advisory lock) this conservatively returns `false`.
#[cfg(unix)]
pub fn is_writer_active(dest_root: &Path) -> bool {
    let path = index_dir_for(dest_root).join(LOCK_FILE);
    let Ok(file) = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)
    else {
        return false;
    };
    use std::os::unix::io::AsRawFd;
    let fd = file.as_raw_fd();
    // SAFETY: `fd` is a valid open descriptor owned by `file`. LOCK_NB never blocks.
    let rc = unsafe { libc::flock(fd, libc::LOCK_EX | libc::LOCK_NB) };
    if rc == 0 {
        // We just acquired it ⇒ nobody held it. Release immediately.
        unsafe {
            libc::flock(fd, libc::LOCK_UN);
        }
        false
    } else {
        // Held by another process (EWOULDBLOCK) ⇒ a writer is active.
        true
    }
}

#[cfg(not(unix))]
pub fn is_writer_active(_dest_root: &Path) -> bool {
    false
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn second_writer_on_same_dest_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path();

        let first = try_acquire(dest, "pipeA", 1000).unwrap();
        assert!(matches!(first, LockOutcome::Acquired(_)));

        // A second writer on the SAME dest must be refused while the first lives.
        match try_acquire(dest, "pipeB", 2000).unwrap() {
            LockOutcome::Held { holder } => {
                assert!(holder.contains("pid="), "holder diagnostics: {holder:?}");
                assert!(
                    holder.contains("label=pipeA"),
                    "should name first holder: {holder:?}"
                );
            }
            LockOutcome::Acquired(_) => panic!("two writers acquired the same dest"),
        }

        // Releasing the first frees the dest for the next writer.
        drop(first);
        let third = try_acquire(dest, "pipeC", 3000).unwrap();
        assert!(matches!(third, LockOutcome::Acquired(_)));
    }

    #[test]
    fn different_dests_do_not_contend() {
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        let la = try_acquire(a.path(), "", 1).unwrap();
        let lb = try_acquire(b.path(), "", 1).unwrap();
        assert!(matches!(la, LockOutcome::Acquired(_)));
        assert!(matches!(lb, LockOutcome::Acquired(_)));
    }
}

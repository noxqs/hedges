//! A single-instance lock, held for the life of the process.
//!
//! The D-Bus name GApplication owns is the *usual* single-instance guard, but
//! it only catches a second copy that registers under the same name — a copy
//! started with a different `HEDGE_APP_ID`, or on a session bus this process
//! can't see, sails straight past it and two daemons end up writing the same
//! `config.json`. This is the belt to that braces: an advisory `flock` on a
//! file named after the identity, which the kernel releases however the holder
//! dies, so there is no such thing as a stale lock to clean up.
//!
//! The lock is per-identity on purpose: `HEDGE_APP_ID=…` is how a development
//! build is meant to run alongside the installed one, and locking on the id
//! keeps that possible while still making two copies of the *same* identity
//! impossible.

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::io::AsRawFd;
use std::path::PathBuf;

/// A held lock. Dropping it (or the process dying) releases it.
pub struct Lock {
    _file: File,
}

/// Who is holding the lock, when we could not take it.
pub struct Holder {
    /// The pid the holder wrote, if it is still readable.
    pub pid: Option<i32>,
}

/// `$XDG_RUNTIME_DIR/<app-id>.lock`, falling back to the state directory when
/// there is no runtime dir (a bare `ssh` session, say).
fn runtime_dir() -> PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| crate::log::path().parent().unwrap_or(&PathBuf::from("/tmp")).to_path_buf())
}

fn lock_path(base: &std::path::Path, app_id: &str) -> PathBuf {
    let _ = std::fs::create_dir_all(base);
    base.join(format!("{app_id}.lock"))
}

/// Take the lock, or report who has it.
///
/// A failure to *open* the file is not treated as "someone else is running":
/// a read-only or missing runtime directory must not stop Hedge starting.
pub fn acquire(app_id: &str) -> Result<Lock, Holder> {
    acquire_in(&runtime_dir(), app_id)
}

/// `acquire`, against an explicit directory. The tests drive this one: taking
/// the directory as an argument keeps them from having to point
/// `XDG_RUNTIME_DIR` somewhere, and `std::env::set_var` in a test binary races
/// every other thread reading the environment.
fn acquire_in(base: &std::path::Path, app_id: &str) -> Result<Lock, Holder> {
    let path = lock_path(base, app_id);
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path);
    let mut file = match file {
        Ok(f) => f,
        Err(e) => {
            log!(WARN, "cannot open the lock file {}: {e}", path.display());
            // fabricate a held-by-nobody success: better to run than to refuse
            return Ok(Lock {
                _file: OpenOptions::new()
                    .read(true)
                    .write(true)
                    .create(true)
                    .truncate(false)
                    .open("/dev/null")
                    .expect("/dev/null"),
            });
        }
    };

    // SAFETY: a plain flock(2) on a descriptor we own for the call's duration.
    let taken = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0;
    if !taken {
        let mut text = String::new();
        let _ = file.read_to_string(&mut text);
        return Err(Holder {
            pid: text.trim().parse().ok(),
        });
    }

    // Leave our pid behind so the copy that loses the race can name us.
    let _ = file.set_len(0);
    let _ = file.seek(SeekFrom::Start(0));
    let _ = write!(file, "{}", std::process::id());
    let _ = file.flush();
    Ok(Lock { _file: file })
}

/// Take the lock, retrying for a few seconds.
///
/// For the copy that has just asked the previous instance to stand down and is
/// racing its exit: the old process still holds the lock for as long as it
/// takes to shut down, and refusing on the first attempt would turn every
/// take-over into a failure.
pub fn acquire_retrying(app_id: &str, attempts: u32) -> Result<Lock, Holder> {
    let mut result = acquire(app_id);
    for _ in 1..attempts.max(1) {
        if result.is_ok() {
            return result;
        }
        std::thread::sleep(std::time::Duration::from_millis(250));
        result = acquire(app_id);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_second_take_of_the_same_id_is_refused_and_names_the_holder() {
        let dir = std::env::temp_dir().join(format!("hedge-lock-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let id = "org.hedge.LockTest";

        let held = acquire_in(&dir, id).ok().expect("first take succeeds");
        // flock is per open file description, so a second open in this same
        // process is refused exactly as another process would be
        let second = acquire_in(&dir, id);
        let holder = second.err().expect("second take is refused");
        assert_eq!(holder.pid, Some(std::process::id() as i32));

        // a different identity is a different lock
        assert!(acquire_in(&dir, "org.hedge.LockTest.Other").is_ok());

        drop(held);
        // Retried rather than asserted once. A sibling test (`ping`'s live
        // round) forks `ping` children on background threads, and between a
        // fork and the exec that drops the descriptor the child holds a copy of
        // every fd this process has open — a held lock file's included. So a
        // lock nobody owns can refuse a taker for a few microseconds, which is
        // what `acquire_retrying` exists to ride out in the app. Retrying here
        // keeps the assertion about release semantics instead of about timing.
        let freed = (0..40).any(|attempt| {
            if attempt > 0 {
                std::thread::sleep(std::time::Duration::from_millis(25));
            }
            acquire_in(&dir, id).is_ok()
        });
        assert!(freed, "the lock is free once released");
        let _ = std::fs::remove_dir_all(&dir);
    }
}

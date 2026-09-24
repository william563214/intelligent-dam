//! Write-stability gate for Watcher ingest
//!
//! # Algorithm
//! 1. Poll file size every `poll_interval`
//! 2. Require `stable_checks` consecutive unchanged non-zero sizes
//! 3. Attempt an advisory exclusive lock (`fs4::FileExt::try_lock`)
//!    - Success: unlock and treat as stable (writer released the file)
//!    - WouldBlock: keep waiting (download still holds it)
//!    - Other OS errors: fall back to size-stable alone
//!
//! # Platform notes
//! - **Unix**: advisory `flock` (or equivalent). Downloaders that hold an
//!   exclusive lock delay ingest until they release it. Size-stable still
//!   catches writers that never lock.
//! - **Windows**: `LockFileEx` advisory lock when the OS supports it. Many
//!   browsers / copy tools do not lock the destination; size-stable is the
//!   reliable primary signal on Windows.
//!
//! Timeouts never hang forever. Callers must not hold the DB mutex across
//! this wait. Failures do not write `scrape_failed` (that status is reserved
//! for scrape pipeline outcomes per the status machine).

use fs4::{FileExt, TryLockError};
use std::fs::{self, File};
use std::io;
use std::path::Path;
use std::thread;
use std::time::{Duration, Instant};
use thiserror::Error;

#[allow(non_upper_case_globals)]
const default_poll_interval: Duration = Duration::from_millis(500);

#[allow(non_upper_case_globals)]
const default_stable_checks: u32 = 3;

#[allow(non_upper_case_globals)]
const default_timeout: Duration = Duration::from_secs(120);

#[allow(non_upper_case_globals)]
const default_missing_grace: Duration = Duration::from_secs(5);

/// Tunables for [`wait_until_stable`].
#[derive(Debug, Clone)]
pub struct StabilityOptions {
    pub poll_interval: Duration,
    pub stable_checks: u32,
    pub timeout: Duration,
    /// How long to tolerate a path that does not exist yet (rename races)
    pub missing_grace: Duration,
}

impl Default for StabilityOptions {
    fn default() -> Self {
        Self {
            poll_interval: default_poll_interval,
            stable_checks: default_stable_checks,
            timeout: default_timeout,
            missing_grace: default_missing_grace,
        }
    }
}

impl StabilityOptions {
    /// Faster timings for unit tests
    pub fn for_tests() -> Self {
        Self {
            poll_interval: Duration::from_millis(40),
            stable_checks: 3,
            timeout: Duration::from_millis(800),
            missing_grace: Duration::from_millis(200),
        }
    }
}

#[derive(Debug, Error)]
pub enum StabilityError {
    #[error("timed out waiting for file to become stable: {path}")]
    TimedOut { path: String },
    #[error("aborted while waiting for file stability: {path}")]
    Aborted { path: String },
    #[error("file disappeared while waiting for stability: {path}")]
    Missing { path: String },
    #[error("io error while checking stability of {path}: {source}")]
    Io {
        path: String,
        #[source]
        source: io::Error,
    },
}

/// Wait until `path` looks fully written then return its final size in bytes.
///
/// `should_abort` is polled each loop so Watcher stop can cancel waits.
pub fn wait_until_stable<F>(
    path: &Path,
    options: &StabilityOptions,
    mut should_abort: F,
) -> Result<u64, StabilityError>
where
    F: FnMut() -> bool,
{
    let path_label = path.display().to_string();
    let started = Instant::now();
    let mut last_size: Option<u64> = None;
    let mut stable_count: u32 = 0;
    let mut first_seen_missing = true;
    let mut missing_since: Option<Instant> = None;

    let required = options.stable_checks.max(1);

    loop {
        if should_abort() {
            return Err(StabilityError::Aborted {
                path: path_label,
            });
        }

        if started.elapsed() > options.timeout {
            return Err(StabilityError::TimedOut {
                path: path_label,
            });
        }

        match sample_size(path) {
            Ok(None) => {
                if first_seen_missing {
                    missing_since = Some(Instant::now());
                    first_seen_missing = false;
                }
                if let Some(since) = missing_since {
                    if since.elapsed() > options.missing_grace {
                        return Err(StabilityError::Missing {
                            path: path_label,
                        });
                    }
                }
                last_size = None;
                stable_count = 0;
            }
            Ok(Some(size)) => {
                first_seen_missing = true;
                missing_since = None;

                if size == 0 {
                    last_size = Some(0);
                    stable_count = 0;
                } else if last_size == Some(size) {
                    stable_count = stable_count.saturating_add(1);
                } else {
                    last_size = Some(size);
                    stable_count = 1;
                }

                if stable_count >= required {
                    match try_confirm_exclusive(path) {
                        LockProbe::Free => return Ok(size),
                        LockProbe::Held => {
                            // Writer still holds the file; keep polling
                            stable_count = required.saturating_sub(1);
                        }
                        LockProbe::Unavailable => return Ok(size),
                    }
                }
            }
            Err(source) => {
                return Err(StabilityError::Io {
                    path: path_label,
                    source,
                });
            }
        }

        thread::sleep(options.poll_interval);
    }
}

enum LockProbe {
    Free,
    Held,
    /// OS / filesystem does not support advisory locks; size-stable is enough
    Unavailable,
}

fn sample_size(path: &Path) -> io::Result<Option<u64>> {
    match fs::metadata(path) {
        Ok(meta) => {
            if meta.is_file() {
                Ok(Some(meta.len()))
            } else {
                Ok(None)
            }
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

fn try_confirm_exclusive(path: &Path) -> LockProbe {
    let file = match File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return LockProbe::Held,
        Err(_) => return LockProbe::Unavailable,
    };

    match file.try_lock() {
        Ok(()) => {
            let _ = file.unlock();
            LockProbe::Free
        }
        Err(TryLockError::WouldBlock) => LockProbe::Held,
        Err(TryLockError::Error(_)) => LockProbe::Unavailable,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::sync::{Arc, Mutex};
    use std::thread;

    fn unique_temp_file(label: &str) -> std::path::PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "idam_stability_{}_{}_{}",
            label,
            std::process::id(),
            nanos
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir.join("clip.mp4")
    }

    #[test]
    fn stable_file_returns_size() {
        let path = unique_temp_file("stable");
        fs::write(&path, b"hello-video-bytes").unwrap();

        let size = wait_until_stable(&path, &StabilityOptions::for_tests(), || false)
            .expect("stable file");
        assert_eq!(size, 17);

        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn growing_file_waits_until_size_stops() {
        let path = unique_temp_file("growing");
        let path_for_writer = path.clone();
        let done = Arc::new(Mutex::new(false));
        let done_flag = Arc::clone(&done);

        let writer = thread::spawn(move || {
            let mut file = File::create(&path_for_writer).unwrap();
            for chunk in 0..8u8 {
                file.write_all(&[chunk; 64]).unwrap();
                file.flush().unwrap();
                thread::sleep(Duration::from_millis(50));
            }
            *done_flag.lock().unwrap() = true;
        });

        let opts = StabilityOptions {
            poll_interval: Duration::from_millis(30),
            stable_checks: 3,
            timeout: Duration::from_secs(5),
            missing_grace: Duration::from_secs(1),
        };

        let size = wait_until_stable(&path, &opts, || false).expect("eventually stable");
        writer.join().unwrap();
        assert!(*done.lock().unwrap());
        assert_eq!(size, 8 * 64);

        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn timeout_on_continuously_growing_file() {
        let path = unique_temp_file("timeout");
        let path_for_writer = path.clone();
        let stop = Arc::new(Mutex::new(false));
        let stop_flag = Arc::clone(&stop);

        let writer = thread::spawn(move || {
            let mut file = File::create(&path_for_writer).unwrap();
            let mut n = 0u8;
            while !*stop_flag.lock().unwrap() {
                file.write_all(&[n; 32]).unwrap();
                file.flush().unwrap();
                n = n.wrapping_add(1);
                thread::sleep(Duration::from_millis(25));
            }
        });

        let opts = StabilityOptions {
            poll_interval: Duration::from_millis(30),
            stable_checks: 5,
            timeout: Duration::from_millis(350),
            missing_grace: Duration::from_millis(100),
        };

        let err = wait_until_stable(&path, &opts, || false).expect_err("should time out");
        assert!(matches!(err, StabilityError::TimedOut { .. }));

        *stop.lock().unwrap() = true;
        writer.join().unwrap();
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn abort_flag_stops_wait() {
        let path = unique_temp_file("abort");
        fs::write(&path, b"x").unwrap();

        let opts = StabilityOptions {
            poll_interval: Duration::from_millis(40),
            stable_checks: 50,
            timeout: Duration::from_secs(10),
            missing_grace: Duration::from_secs(1),
        };

        let started = Instant::now();
        let err = wait_until_stable(&path, &opts, || started.elapsed() > Duration::from_millis(80))
            .expect_err("abort");
        assert!(matches!(err, StabilityError::Aborted { .. }));

        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn exclusive_lock_held_delays_until_unlock() {
        let path = unique_temp_file("lock");
        fs::write(&path, b"locked-content-bytes").unwrap();

        let holder = File::open(&path).unwrap();
        holder.try_lock().expect("take exclusive lock");

        let path_for_waiter = path.clone();
        let unlock_at = Instant::now() + Duration::from_millis(200);
        let unlocker = thread::spawn(move || {
            while Instant::now() < unlock_at {
                thread::sleep(Duration::from_millis(20));
            }
            let _ = holder.unlock();
            drop(holder);
        });

        let opts = StabilityOptions {
            poll_interval: Duration::from_millis(40),
            stable_checks: 2,
            timeout: Duration::from_secs(3),
            missing_grace: Duration::from_millis(200),
        };

        let size = wait_until_stable(&path_for_waiter, &opts, || false).expect("after unlock");
        unlocker.join().unwrap();
        assert_eq!(size, 20);

        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn missing_path_errors_after_grace() {
        let path = unique_temp_file("missing");
        // parent exists but file does not
        let opts = StabilityOptions {
            poll_interval: Duration::from_millis(20),
            stable_checks: 2,
            timeout: Duration::from_secs(2),
            missing_grace: Duration::from_millis(80),
        };
        let err = wait_until_stable(&path, &opts, || false).expect_err("missing");
        assert!(matches!(err, StabilityError::Missing { .. }));
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }
}

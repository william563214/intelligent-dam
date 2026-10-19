//! Write-stability gate for Watcher ingest
//!
//! # Algorithm
//! 1. Poll file size and mtime every `poll_interval`
//! 2. Require `stable_checks` consecutive unchanged non-zero (size, mtime) samples
//! 3. Confirm the file is no longer held by a writer
//!    - **All platforms**: advisory exclusive lock (`fs4::FileExt::try_lock`)
//!      - Success: unlock and treat as free
//!      - WouldBlock: keep waiting
//!      - Other OS errors: fall back (size+mtime may be enough)
//!    - **Windows**: additionally open with `share_mode(0)` (exclusive share)
//!      - Sharing violation: treat as held and keep waiting
//!      - Other errors: fall back like the advisory lock path
//!
//! # Platform notes
//! - **Unix**: advisory `flock` (or equivalent). Downloaders that hold an
//!   exclusive lock delay ingest until they release it. Size+mtime still
//!   catches writers that never lock.
//! - **Windows**: many copy tools do not take an advisory lock on the
//!   destination; size+mtime plus exclusive-share open are the reliable
//!   signals. A mid-copy pause that freezes size/mtime still fails the
//!   share-mode probe while the writer keeps the handle open.
//!
//! Optional container tail checks (e.g. ffprobe) are intentionally omitted:
//! they add process spawn cost and false negatives on partial-but-readable
//! media without a clear win over size+mtime+share.
//!
//! Timeouts never hang forever. Callers must not hold the DB mutex across
//! this wait. Failures do not write `scrape_failed` (that status is reserved
//! for scrape pipeline outcomes per the status machine).

use fs4::{FileExt, TryLockError};
use std::fs::{self, File};
use std::io;
use std::path::Path;
use std::thread;
use std::time::{Duration, Instant, SystemTime};
use thiserror::Error;

#[allow(non_upper_case_globals)]
const default_poll_interval: Duration = Duration::from_millis(500);

#[allow(non_upper_case_globals)]
const default_stable_checks: u32 = 3;

#[allow(non_upper_case_globals)]
const default_timeout: Duration = Duration::from_secs(120);

#[allow(non_upper_case_globals)]
const default_missing_grace: Duration = Duration::from_secs(5);

#[allow(non_upper_case_globals)]
const env_poll_ms: &str = "IDAM_STABILITY_POLL_MS";

#[allow(non_upper_case_globals)]
const env_stable_checks: &str = "IDAM_STABILITY_CHECKS";

#[allow(non_upper_case_globals)]
const env_timeout_secs: &str = "IDAM_STABILITY_TIMEOUT_SECS";

#[allow(non_upper_case_globals)]
const env_missing_grace_ms: &str = "IDAM_STABILITY_MISSING_GRACE_MS";

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
    /// Built-in defaults with optional env overrides for production tuning
    pub fn from_env() -> Self {
        let mut opts = Self::default();
        if let Some(ms) = env_u64(env_poll_ms) {
            opts.poll_interval = Duration::from_millis(ms.max(1));
        }
        if let Some(n) = env_u64(env_stable_checks) {
            opts.stable_checks = n.clamp(1, u64::from(u32::MAX)) as u32;
        }
        if let Some(secs) = env_u64(env_timeout_secs) {
            opts.timeout = Duration::from_secs(secs.max(1));
        }
        if let Some(ms) = env_u64(env_missing_grace_ms) {
            opts.missing_grace = Duration::from_millis(ms.max(1));
        }
        opts
    }

    /// Faster timings for unit and integration tests
    #[cfg(any(test, feature = "test-support"))]
    pub fn for_tests() -> Self {
        Self {
            poll_interval: Duration::from_millis(40),
            stable_checks: 3,
            timeout: Duration::from_millis(800),
            missing_grace: Duration::from_millis(200),
        }
    }

    /// Watcher e2e: 100ms x5 (~500ms unchanged) so chunked writes with ~25ms
    /// pauses cannot pass the gate mid-growth (clear margin vs growth cadence)
    #[cfg(feature = "test-support")]
    pub fn for_watcher_e2e() -> Self {
        Self {
            poll_interval: Duration::from_millis(100),
            stable_checks: 5,
            timeout: Duration::from_secs(15),
            missing_grace: Duration::from_secs(2),
        }
    }
}

fn env_u64(name: &str) -> Option<u64> {
    std::env::var(name).ok()?.trim().parse().ok()
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FileSample {
    size: u64,
    /// None when the filesystem does not expose mtime
    mtime: Option<SystemTime>,
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
    let mut last_sample: Option<FileSample> = None;
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

        match sample_file(path) {
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
                last_sample = None;
                stable_count = 0;
            }
            Ok(Some(sample)) => {
                first_seen_missing = true;
                missing_since = None;

                if sample.size == 0 {
                    last_sample = Some(sample);
                    stable_count = 0;
                } else if last_sample == Some(sample) {
                    stable_count = stable_count.saturating_add(1);
                } else {
                    last_sample = Some(sample);
                    stable_count = 1;
                }

                if stable_count >= required {
                    match try_confirm_ready(path) {
                        LockProbe::Free => return Ok(sample.size),
                        LockProbe::Held => {
                            // Writer still holds the file; keep polling
                            stable_count = required.saturating_sub(1);
                        }
                        LockProbe::Unavailable => return Ok(sample.size),
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
    /// OS / filesystem does not support the probe; size+mtime may be enough
    Unavailable,
}

fn sample_file(path: &Path) -> io::Result<Option<FileSample>> {
    match fs::metadata(path) {
        Ok(meta) => {
            if meta.is_file() {
                let mtime = meta.modified().ok();
                Ok(Some(FileSample {
                    size: meta.len(),
                    mtime,
                }))
            } else {
                Ok(None)
            }
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

fn try_confirm_ready(path: &Path) -> LockProbe {
    // Windows exclusive-share first so we do not hold another handle that
    // would itself cause a false sharing violation.
    #[cfg(windows)]
    {
        match try_windows_exclusive_share(path) {
            LockProbe::Held => return LockProbe::Held,
            LockProbe::Free | LockProbe::Unavailable => {}
        }
    }

    try_confirm_exclusive(path)
}

fn try_confirm_exclusive(path: &Path) -> LockProbe {
    let file = match File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return LockProbe::Held,
        Err(_) => return LockProbe::Unavailable,
    };

    // 明確走 fs4::FileExt 避免與 std::fs::File::try_lock (1.89+) 固有方法衝突
    match FileExt::try_lock(&file) {
        Ok(()) => {
            let _ = FileExt::unlock(&file);
            LockProbe::Free
        }
        Err(TryLockError::WouldBlock) => LockProbe::Held,
        Err(TryLockError::Error(_)) => LockProbe::Unavailable,
    }
}

/// Open with no share rights; sharing violation means a writer still holds the file.
#[cfg(windows)]
fn try_windows_exclusive_share(path: &Path) -> LockProbe {
    use std::fs::OpenOptions;
    use std::os::windows::fs::OpenOptionsExt;

    // ERROR_SHARING_VIOLATION
    #[allow(non_upper_case_globals)]
    const sharing_violation: i32 = 32;

    match OpenOptions::new().read(true).share_mode(0).open(path) {
        Ok(_file) => LockProbe::Free,
        Err(e) if e.kind() == io::ErrorKind::NotFound => LockProbe::Held,
        Err(e) if e.raw_os_error() == Some(sharing_violation) => LockProbe::Held,
        Err(_) => LockProbe::Unavailable,
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
    fn mtime_change_resets_stability_even_when_size_unchanged() {
        let path = unique_temp_file("mtime");
        fs::write(&path, b"same-size-payload!!").unwrap();

        let path_for_bumper = path.clone();
        let stop = Arc::new(Mutex::new(false));
        let stop_flag = Arc::clone(&stop);

        let bumper = thread::spawn(move || {
            // Keep size fixed but bump mtime so the gate must not declare stable
            let mut tick = 0u64;
            while !*stop_flag.lock().unwrap() {
                tick += 1;
                let file = File::options()
                    .write(true)
                    .open(&path_for_bumper)
                    .expect("open for mtime bump");
                let base = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000 + tick);
                file.set_modified(base).expect("set_modified");
                drop(file);
                thread::sleep(Duration::from_millis(35));
            }
        });

        let opts = StabilityOptions {
            poll_interval: Duration::from_millis(25),
            stable_checks: 3,
            timeout: Duration::from_millis(400),
            missing_grace: Duration::from_millis(100),
        };

        let err = wait_until_stable(&path, &opts, || false).expect_err("mtime churn");
        assert!(matches!(err, StabilityError::TimedOut { .. }));

        *stop.lock().unwrap() = true;
        bumper.join().unwrap();

        // After mtime settles the same bytes should pass
        let settle = StabilityOptions {
            poll_interval: Duration::from_millis(30),
            stable_checks: 3,
            timeout: Duration::from_secs(3),
            missing_grace: Duration::from_millis(200),
        };
        let size = wait_until_stable(&path, &settle, || false).expect("after settle");
        assert_eq!(size, b"same-size-payload!!".len() as u64);

        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn file_sample_equality_requires_matching_mtime() {
        let a = FileSample {
            size: 10,
            mtime: Some(SystemTime::UNIX_EPOCH + Duration::from_secs(1)),
        };
        let b = FileSample {
            size: 10,
            mtime: Some(SystemTime::UNIX_EPOCH + Duration::from_secs(2)),
        };
        let c = FileSample {
            size: 10,
            mtime: a.mtime,
        };
        assert_ne!(a, b);
        assert_eq!(a, c);
        assert_ne!(
            a,
            FileSample {
                size: 11,
                mtime: a.mtime,
            }
        );
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

    #[test]
    fn from_env_leaves_defaults_without_vars() {
        // Do not call env::set_var; only assert the pure-default path shape
        let opts = StabilityOptions::default();
        assert_eq!(opts.poll_interval, default_poll_interval);
        assert_eq!(opts.stable_checks, default_stable_checks);
        assert_eq!(opts.timeout, default_timeout);
        assert_eq!(opts.missing_grace, default_missing_grace);
    }
}

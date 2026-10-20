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
//!    - **Windows**: open with `share_mode(FILE_SHARE_READ)` so read-only
//!      holders (Explorer preview, players, seeders) do not block ingest,
//!      while a write handle that denies shared read still yields Held
//!    - If size+mtime stay stable for `held_degrade_after` while the probe
//!      remains Held, degrade to stable with a warn (avoid 120s TimedOut /
//!      missed ingest). Zero disables degrade.
//!
//! # Platform / filesystem notes
//! - **Unix**: advisory `flock` (or equivalent). Downloaders that hold an
//!   exclusive lock delay ingest until they release it. Size+mtime still
//!   catches writers that never lock.
//! - **Windows**: many copy tools do not take an advisory lock; size+mtime
//!   plus the FILE_SHARE_READ open probe are the primary signals.
//! - **mtime granularity**: FAT and some SMB mounts update mtime in 2s (or
//!   coarser) steps. Brief true growth can look mtime-stable; size still
//!   changes, and the writer share probe covers mid-copy pauses.
//! - **mtime unavailable**: if `metadata.modified()` fails, `mtime` is
//!   `None` and only size participates in equality (both sides `None`).
//! - **delete / rename mid-wait**: a missing path (or probe `Gone`) resets
//!   the stable streak and uses `missing_grace` before `Missing`.
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

/// Size+mtime stable this long while probe stays Held → degrade (warn)
#[allow(non_upper_case_globals)]
const default_held_degrade_after: Duration = Duration::from_secs(30);

#[allow(non_upper_case_globals)]
const floor_poll_interval: Duration = Duration::from_millis(50);

#[allow(non_upper_case_globals)]
const floor_stable_checks: u32 = 2;

#[allow(non_upper_case_globals)]
const floor_timeout: Duration = Duration::from_secs(5);

#[allow(non_upper_case_globals)]
const env_poll_ms: &str = "IDAM_STABILITY_POLL_MS";

#[allow(non_upper_case_globals)]
const env_stable_checks: &str = "IDAM_STABILITY_CHECKS";

#[allow(non_upper_case_globals)]
const env_timeout_secs: &str = "IDAM_STABILITY_TIMEOUT_SECS";

#[allow(non_upper_case_globals)]
const env_missing_grace_ms: &str = "IDAM_STABILITY_MISSING_GRACE_MS";

#[allow(non_upper_case_globals)]
const env_held_degrade_ms: &str = "IDAM_STABILITY_HELD_DEGRADE_MS";

/// Tunables for [`wait_until_stable`].
#[derive(Debug, Clone)]
pub struct StabilityOptions {
    pub poll_interval: Duration,
    pub stable_checks: u32,
    pub timeout: Duration,
    /// How long to tolerate a path that does not exist yet (rename races)
    pub missing_grace: Duration,
    /// After size+mtime stay stable this long while the writer probe remains
    /// Held, accept the file with a warn. Zero disables degrade (wait until
    /// timeout / unlock).
    pub held_degrade_after: Duration,
}

impl Default for StabilityOptions {
    fn default() -> Self {
        Self {
            poll_interval: default_poll_interval,
            stable_checks: default_stable_checks,
            timeout: default_timeout,
            missing_grace: default_missing_grace,
            held_degrade_after: default_held_degrade_after,
        }
    }
}

impl StabilityOptions {
    /// Built-in defaults with optional env overrides for production tuning
    pub fn from_env() -> Self {
        Self::from_lookup(|name| std::env::var(name).ok())
    }

    /// Same as [`from_env`] but reads via `get` (tests inject values; no `set_var`)
    pub fn from_lookup(get: impl Fn(&str) -> Option<String>) -> Self {
        let mut opts = Self::default();

        if let Some(raw) = get(env_poll_ms) {
            if let Some(ms) = parse_u64_env(env_poll_ms, &raw) {
                let clamped = ms.max(floor_poll_interval.as_millis() as u64);
                if clamped != ms {
                    eprintln!(
                        "warn: {env_poll_ms}={ms} clamped to {} ms (floor)",
                        floor_poll_interval.as_millis()
                    );
                }
                opts.poll_interval = Duration::from_millis(clamped);
            }
        }

        if let Some(raw) = get(env_stable_checks) {
            if let Some(n) = parse_u64_env(env_stable_checks, &raw) {
                let capped = n.min(u64::from(u32::MAX));
                let clamped = capped.max(u64::from(floor_stable_checks));
                if clamped != n {
                    eprintln!(
                        "warn: {env_stable_checks}={n} clamped to {clamped} (floor {floor_stable_checks})"
                    );
                }
                opts.stable_checks = clamped as u32;
            }
        }

        if let Some(raw) = get(env_timeout_secs) {
            if let Some(secs) = parse_u64_env(env_timeout_secs, &raw) {
                let floor_secs = floor_timeout.as_secs();
                let clamped = secs.max(floor_secs);
                if clamped != secs {
                    eprintln!(
                        "warn: {env_timeout_secs}={secs} clamped to {clamped} s (floor)"
                    );
                }
                opts.timeout = Duration::from_secs(clamped);
            }
        }

        if let Some(raw) = get(env_missing_grace_ms) {
            if let Some(ms) = parse_u64_env(env_missing_grace_ms, &raw) {
                opts.missing_grace = Duration::from_millis(ms.max(1));
            }
        }

        if let Some(raw) = get(env_held_degrade_ms) {
            if let Some(ms) = parse_u64_env(env_held_degrade_ms, &raw) {
                // 0 disables degrade; otherwise keep as requested
                opts.held_degrade_after = Duration::from_millis(ms);
            }
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
            // Long enough that unlock-delay tests are not short-circuited
            held_degrade_after: Duration::from_secs(5),
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
            // Pause tests sleep ~1.8s; degrade must stay well above that
            held_degrade_after: Duration::from_secs(10),
        }
    }
}

fn parse_u64_env(name: &str, raw: &str) -> Option<u64> {
    match raw.trim().parse::<u64>() {
        Ok(v) => Some(v),
        Err(_) => {
            eprintln!("warn: ignoring invalid {name}={raw:?} (expected u64)");
            None
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
    let mut probe_held_since: Option<Instant> = None;

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
                probe_held_since = None;
            }
            Ok(Some(sample)) => {
                first_seen_missing = true;
                missing_since = None;

                if sample.size == 0 {
                    last_sample = Some(sample);
                    stable_count = 0;
                    probe_held_since = None;
                } else if last_sample == Some(sample) {
                    stable_count = stable_count.saturating_add(1);
                } else {
                    last_sample = Some(sample);
                    stable_count = 1;
                    probe_held_since = None;
                }

                if stable_count >= required {
                    match try_confirm_ready(path) {
                        LockProbe::Free => return Ok(sample.size),
                        LockProbe::Held => {
                            let now = Instant::now();
                            let since = probe_held_since.get_or_insert(now);
                            if !options.held_degrade_after.is_zero()
                                && now.duration_since(*since) >= options.held_degrade_after
                            {
                                eprintln!(
                                    "warn: stability: writer probe still Held after {:?} of size+mtime-stable for {}; degrading to stable",
                                    options.held_degrade_after, path_label
                                );
                                return Ok(sample.size);
                            }
                            // Keep near the threshold so the next matching sample re-probes
                            stable_count = required.saturating_sub(1);
                        }
                        LockProbe::Gone => {
                            // Vanished between sample and open; reuse missing_grace
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
                            probe_held_since = None;
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
    /// Path disappeared between metadata sample and open (rename/delete).
    /// Mapped like a missing sample so `missing_grace` still applies.
    Gone,
    /// OS / filesystem does not support the probe; size+mtime may be enough
    Unavailable,
}

fn sample_file(path: &Path) -> io::Result<Option<FileSample>> {
    match fs::metadata(path) {
        Ok(meta) => {
            if meta.is_file() {
                // modified() can fail on exotic FS; treat as size-only equality
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
    // Windows share probe first so we do not hold another handle that would
    // itself interact with sharing checks.
    #[cfg(windows)]
    {
        match try_windows_share_read(path) {
            LockProbe::Held => return LockProbe::Held,
            LockProbe::Gone => return LockProbe::Gone,
            LockProbe::Free | LockProbe::Unavailable => {}
        }
    }

    try_confirm_exclusive(path)
}

fn try_confirm_exclusive(path: &Path) -> LockProbe {
    let file = match File::open(path) {
        Ok(f) => f,
        // NotFound here is Gone (not Held): avoid treating delete as a held lock
        Err(e) if e.kind() == io::ErrorKind::NotFound => return LockProbe::Gone,
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

/// Open for read while allowing other readers (`FILE_SHARE_READ`).
/// A write handle that does not share read yields sharing/lock violation → Held.
/// Read-only holders (preview, player, seeder) typically pass → Free.
#[cfg(windows)]
fn try_windows_share_read(path: &Path) -> LockProbe {
    use std::fs::OpenOptions;
    use std::os::windows::fs::OpenOptionsExt;

    // Win32: ERROR_SHARING_VIOLATION / ERROR_LOCK_VIOLATION (no windows-sys dep)
    const ERROR_SHARING_VIOLATION: i32 = 32;
    const ERROR_LOCK_VIOLATION: i32 = 33;
    // FILE_SHARE_READ
    const FILE_SHARE_READ: u32 = 0x0000_0001;

    match OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ)
        .open(path)
    {
        Ok(_file) => LockProbe::Free,
        // Gone: delete/rename between sample and probe; missing_grace still applies
        Err(e) if e.kind() == io::ErrorKind::NotFound => LockProbe::Gone,
        Err(e) if e.raw_os_error() == Some(ERROR_SHARING_VIOLATION) => LockProbe::Held,
        Err(e) if e.raw_os_error() == Some(ERROR_LOCK_VIOLATION) => LockProbe::Held,
        Err(_) => LockProbe::Unavailable,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
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
                // sync_all so size is visible to pollers (macOS CI was racing on flush)
                file.sync_all().unwrap();
                // Grow faster than the stable window (4 * 40ms) so mid-write
                // samples cannot look stable under scheduler jitter
                thread::sleep(Duration::from_millis(15));
            }
            *done_flag.lock().unwrap() = true;
        });

        let opts = StabilityOptions {
            poll_interval: Duration::from_millis(40),
            stable_checks: 4,
            timeout: Duration::from_secs(8),
            missing_grace: Duration::from_secs(1),
            held_degrade_after: Duration::from_secs(30),
        };

        let size = wait_until_stable(&path, &opts, || false).expect("eventually stable");
        writer.join().unwrap();
        assert!(*done.lock().unwrap());
        assert_eq!(size, 8 * 64);
        assert_eq!(fs::metadata(&path).unwrap().len(), 8 * 64);

        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn mtime_change_resets_stability_even_when_size_unchanged() {
        let path = unique_temp_file("mtime");
        let payload = b"same-size-payload!!";
        fs::write(&path, payload).unwrap();

        let path_for_bumper = path.clone();
        let stop = Arc::new(Mutex::new(false));
        let stop_flag = Arc::clone(&stop);

        let bumper = thread::spawn(move || {
            // Keep size fixed but bump mtime so the gate must not declare stable.
            // Rewrite same bytes (more reliable than set_modified on macOS APFS).
            while !*stop_flag.lock().unwrap() {
                fs::write(&path_for_bumper, payload).unwrap();
                thread::sleep(Duration::from_millis(20));
            }
        });

        let opts = StabilityOptions {
            poll_interval: Duration::from_millis(25),
            stable_checks: 3,
            timeout: Duration::from_millis(600),
            missing_grace: Duration::from_millis(100),
            held_degrade_after: Duration::from_secs(30),
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
            held_degrade_after: Duration::from_secs(30),
        };
        let size = wait_until_stable(&path, &settle, || false).expect("after settle");
        assert_eq!(size, payload.len() as u64);

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
            held_degrade_after: Duration::ZERO,
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
            held_degrade_after: Duration::from_secs(30),
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
            held_degrade_after: Duration::from_secs(10),
        };

        let size = wait_until_stable(&path_for_waiter, &opts, || false).expect("after unlock");
        unlocker.join().unwrap();
        assert_eq!(size, 20);

        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn held_degrade_accepts_after_threshold_while_probe_held() {
        let path = unique_temp_file("degrade");
        fs::write(&path, b"degrade-payload-bytes").unwrap();

        let holder = File::open(&path).unwrap();
        holder.try_lock().expect("hold lock for degrade test");

        let opts = StabilityOptions {
            poll_interval: Duration::from_millis(30),
            stable_checks: 2,
            timeout: Duration::from_secs(5),
            missing_grace: Duration::from_millis(200),
            held_degrade_after: Duration::from_millis(250),
        };

        let size = wait_until_stable(&path, &opts, || false).expect("degrade to stable");
        assert_eq!(size, b"degrade-payload-bytes".len() as u64);
        drop(holder);

        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn missing_path_errors_after_grace() {
        let path = unique_temp_file("missing");
        let opts = StabilityOptions {
            poll_interval: Duration::from_millis(20),
            stable_checks: 2,
            timeout: Duration::from_secs(2),
            missing_grace: Duration::from_millis(80),
            held_degrade_after: Duration::from_secs(30),
        };
        let err = wait_until_stable(&path, &opts, || false).expect_err("missing");
        assert!(matches!(err, StabilityError::Missing { .. }));
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn from_lookup_none_keeps_defaults() {
        let opts = StabilityOptions::from_lookup(|_| None);
        assert_eq!(opts.poll_interval, default_poll_interval);
        assert_eq!(opts.stable_checks, default_stable_checks);
        assert_eq!(opts.timeout, default_timeout);
        assert_eq!(opts.missing_grace, default_missing_grace);
        assert_eq!(opts.held_degrade_after, default_held_degrade_after);
    }

    #[test]
    fn from_lookup_applies_floors() {
        let map: HashMap<&str, &str> = HashMap::from([
            (env_poll_ms, "1"),
            (env_stable_checks, "1"),
            (env_timeout_secs, "1"),
            (env_held_degrade_ms, "0"),
        ]);
        let opts = StabilityOptions::from_lookup(|k| map.get(k).map(|s| (*s).to_string()));
        assert_eq!(opts.poll_interval, floor_poll_interval);
        assert_eq!(opts.stable_checks, floor_stable_checks);
        assert_eq!(opts.timeout, floor_timeout);
        assert_eq!(opts.held_degrade_after, Duration::ZERO);
    }

    #[test]
    fn from_lookup_ignores_invalid_parse() {
        let map: HashMap<&str, &str> = HashMap::from([(env_poll_ms, "not-a-number")]);
        let opts = StabilityOptions::from_lookup(|k| map.get(k).map(|s| (*s).to_string()));
        assert_eq!(opts.poll_interval, default_poll_interval);
    }

    #[cfg(windows)]
    #[test]
    fn windows_share_probe_write_held_then_free_read_free() {
        let path = unique_temp_file("win_share");
        let mut writer = File::create(&path).expect("create");
        writer.write_all(b"windows-share-probe").unwrap();
        writer.flush().unwrap();

        assert!(
            matches!(try_windows_share_read(&path), LockProbe::Held),
            "open write handle must yield Held"
        );

        drop(writer);
        assert!(
            matches!(try_windows_share_read(&path), LockProbe::Free),
            "after drop must be Free"
        );

        let reader = File::open(&path).expect("read-only open");
        assert!(
            matches!(try_windows_share_read(&path), LockProbe::Free),
            "read-only holder must still be Free (FILE_SHARE_READ)"
        );
        drop(reader);

        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    #[cfg(windows)]
    #[test]
    fn windows_readonly_holder_still_reaches_stable() {
        let path = unique_temp_file("win_ro");
        fs::write(&path, b"readonly-holder-payload").unwrap();
        let reader = File::open(&path).expect("read-only");

        let opts = StabilityOptions {
            poll_interval: Duration::from_millis(40),
            stable_checks: 2,
            timeout: Duration::from_secs(3),
            missing_grace: Duration::from_millis(200),
            held_degrade_after: Duration::from_secs(10),
        };
        let size = wait_until_stable(&path, &opts, || false).expect("ingest despite reader");
        assert_eq!(size, b"readonly-holder-payload".len() as u64);
        drop(reader);

        let _ = fs::remove_dir_all(path.parent().unwrap());
    }
}

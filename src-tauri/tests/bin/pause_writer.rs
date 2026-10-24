//! Cross-process pause/resume writer for watcher_e2e (#93).
//!
//! macOS BSD flock is process-scoped: a second fd in the *same* process can
//! take an exclusive lock while the writer still holds one. Spawning this
//! helper keeps the advisory lock in another process so the stability probe
//! sees Held for the whole mid-write pause (Linux/Windows behave the same).

use fs4::FileExt;
use std::env;
use std::fs::File;
use std::io::Write;
use std::path::PathBuf;
use std::thread;
use std::time::Duration;

fn main() {
    let mut args = env::args().skip(1);
    let dest = PathBuf::from(args.next().expect("usage: pause_writer <path> <half> <pause_ms>"));
    let half: usize = args
        .next()
        .expect("half bytes")
        .parse()
        .expect("half usize");
    let pause_ms: u64 = args
        .next()
        .expect("pause_ms")
        .parse()
        .expect("pause_ms u64");

    let mut file = File::create(&dest).expect("create");
    file.write_all(&vec![0x11u8; half]).expect("first half");
    file.flush().expect("flush half");
    // Hold exclusive for the whole pause so Unix advisory probe stays Held;
    // on Windows the open write handle alone fails FILE_SHARE_READ open.
    FileExt::try_lock(&file).expect("hold exclusive during pause");

    let mid = dest.with_extension("mid");
    std::fs::write(&mid, b"1").expect("mid signal");

    thread::sleep(Duration::from_millis(pause_ms));

    file.write_all(&vec![0x22u8; half]).expect("second half");
    file.flush().expect("flush full");
    let _ = FileExt::unlock(&file);
    drop(file);
    let _ = std::fs::remove_file(&mid);
}

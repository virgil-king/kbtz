use std::io::Write;
use std::sync::OnceLock;
use std::time::Instant;

static START: OnceLock<Instant> = OnceLock::new();

/// Append a timestamped line to the file specified by `KBTZ_DEBUG`.
/// No-op if the env var is unset.
pub fn log(msg: &str) {
    let Ok(path) = std::env::var("KBTZ_DEBUG") else {
        return;
    };
    let elapsed = START.get_or_init(Instant::now).elapsed();
    let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    else {
        return;
    };
    let _ = writeln!(f, "[{:>8.3}s] {}", elapsed.as_secs_f64(), msg);
}

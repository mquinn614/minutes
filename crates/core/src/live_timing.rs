//! Opt-in performance logger for diagnosing live-transcript latency on a target
//! machine — especially CPU-only Windows hosts, where the synthetic
//! `whisper_rtf` benchmark doesn't capture the real per-utterance cost (state
//! creation, VAD, queue pressure). Writes timing lines to
//! `~/.minutes/live-timing.log` ONLY when the env var `MINUTES_LIVE_TIMING` is
//! set to a non-empty, non-"0" value — so it is a complete no-op for normal
//! builds and changes no behavior.
//!
//! TEMPORARY DIAGNOSTIC. Remove (and the call sites in `streaming.rs` /
//! `streaming_whisper.rs`) once the live-scoring perf issue is resolved.

use std::io::Write;
use std::sync::atomic::{AtomicU64, Ordering};

/// Audio chunks dropped because the live processing queue was full (whisper not
/// keeping up). A growing value means real audio loss → dropped buzzwords.
static DROPPED_CHUNKS: AtomicU64 = AtomicU64::new(0);

/// Record one dropped audio chunk (called from the capture path on a full queue).
pub fn record_dropped_chunk() {
    DROPPED_CHUNKS.fetch_add(1, Ordering::Relaxed);
}

/// Cumulative dropped-chunk count this process.
pub fn dropped_chunks() -> u64 {
    DROPPED_CHUNKS.load(Ordering::Relaxed)
}

/// True when `MINUTES_LIVE_TIMING` is set to a non-empty, non-"0" value.
pub fn enabled() -> bool {
    std::env::var("MINUTES_LIVE_TIMING")
        .map(|v| !v.is_empty() && v != "0")
        .unwrap_or(false)
}

/// Append a timestamped line to `~/.minutes/live-timing.log`. No-op unless
/// `enabled()`. Best-effort: any I/O error is silently ignored.
pub fn log(line: &str) {
    if !enabled() {
        return;
    }
    let path = crate::pid::live_transcript_jsonl_path().with_file_name("live-timing.log");
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        let ts = chrono::Local::now().format("%H:%M:%S%.3f");
        let _ = writeln!(f, "[{}] {}", ts, line);
    }
}

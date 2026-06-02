use crate::transcribe::streaming_whisper_params;
use whisper_rs::WhisperContext;

// ──────────────────────────────────────────────────────────────
// Streaming whisper transcription — progressive text output.
//
// Instead of batch (accumulate all audio → transcribe once),
// this transcribes in rolling windows while the user speaks:
//
//   Audio chunks accumulate:
//     [0s──────2s]                         → whisper → "Switch to monthly"
//     [0s──────────────4s]                 → whisper → "Switch to monthly billing for"
//     [0s──────────────────────6s]         → whisper → "Switch to monthly billing for consultants"
//     [0s──────────────────────────────8s] → (silence) → FINAL
//
// Key design decisions:
//   - Full re-transcription on each pass (not incremental). Whisper
//     is fast enough on the accumulated buffer because we're using
//     the small/base model and utterances are short (<2 min).
//   - No segment stitching needed — we always transcribe from t=0
//     so whisper sees full context. Each pass replaces the previous.
//   - Partial results are emitted via callback; the final result on
//     silence replaces all partials.
//   - Uses the same WhisperContext (preloaded model) as batch mode.
//
// Why full re-transcription instead of incremental:
//   Incremental (transcribe only the new 2s chunk) produces worse
//   quality because whisper loses context from earlier speech.
//   Full re-transcription from t=0 gives consistent output at the
//   cost of increasing latency as the utterance grows. For typical
//   dictation utterances (<30s), re-transcription takes <500ms on
//   Apple Silicon with the base model. Acceptable.
//
// Performance budget:
//   - base model: ~200ms for 10s audio on M-series
//   - small model: ~500ms for 10s audio on M-series
//   - Transcription runs on a background thread; audio capture
//     continues uninterrupted on the main thread.
// ──────────────────────────────────────────────────────────────

/// How often to run partial transcription (in audio samples at 16kHz).
const PARTIAL_INTERVAL_SAMPLES: usize = 16000 * 2; // Every 2 seconds

/// Minimum audio length to attempt transcription (avoid noise-only runs).
const MIN_TRANSCRIBE_SAMPLES: usize = 16000; // 1 second

/// Default cap for partial transcription cost. See `StreamingWhisper::new` for
/// the full reasoning — past this many seconds of accumulated audio, partial
/// passes are skipped (the utterance still finalizes correctly).
pub const DEFAULT_PARTIAL_MAX_SECS: u32 = 30;

/// Result from a streaming transcription pass.
#[derive(Debug, Clone)]
pub struct StreamingResult {
    /// The transcribed text (replaces any previous partial).
    pub text: String,
    /// Whether this is a final result (silence detected) or partial (still speaking).
    pub is_final: bool,
    /// Duration of audio transcribed in seconds.
    pub duration_secs: f64,
}

/// Streaming whisper transcriber. Holds the accumulated audio buffer
/// and runs partial transcriptions at intervals.
pub struct StreamingWhisper {
    /// All audio samples accumulated so far (16kHz mono f32).
    audio_buffer: Vec<f32>,
    /// Samples since last partial transcription.
    samples_since_partial: usize,
    /// The last partial text emitted (for dedup).
    last_partial: String,
    /// Number of CPU threads for whisper.
    n_threads: i32,
    /// Language hint (None = auto-detect).
    language: Option<String>,
    /// Whether we've created a state before (suppress init noise on subsequent calls).
    has_created_state: bool,
    /// Cap on partial-transcription buffer length, in samples at 16kHz. Past
    /// this length, partials are skipped (final still runs at end of utterance).
    partial_max_samples: usize,
    /// How often to run a partial pass, in samples at 16kHz. Defaults to
    /// `PARTIAL_INTERVAL_SAMPLES` (2s); a caller (Minutes Madness) can shorten
    /// it for snappier live scoring via `set_partial_interval_secs`.
    partial_interval_samples: usize,
}

impl StreamingWhisper {
    /// Create a new streaming transcriber with the default partial cap (30s).
    pub fn new(language: Option<String>) -> Self {
        Self::with_partial_max_secs(language, DEFAULT_PARTIAL_MAX_SECS)
    }

    /// Create a new streaming transcriber with a custom partial-cap limit
    /// (in seconds). Past this many seconds of accumulated audio the partial
    /// `state.full(...)` pass is skipped on each `feed()` call. The utterance
    /// still finalizes correctly via `finalize()` when the caller (typically
    /// VAD/silence detection in `live_transcript.rs`) decides the utterance
    /// is over.
    ///
    /// Why this matters: partial cost is O(buffer_len). At ~200ms per 10s of
    /// audio on Apple Silicon with the base model, a 60s buffer takes ~1.2s
    /// per partial — slower than the 2s partial interval, so partials queue
    /// up and fall further behind. 30s keeps each partial well under the
    /// interval and stops the runaway.
    pub fn with_partial_max_secs(language: Option<String>, partial_max_secs: u32) -> Self {
        let partial_max_samples = (partial_max_secs as usize).saturating_mul(16000);
        Self {
            audio_buffer: Vec::with_capacity(16000 * 30), // pre-alloc 30s
            samples_since_partial: 0,
            last_partial: String::new(),
            n_threads: live_n_threads(),
            language,
            has_created_state: false,
            partial_max_samples,
            partial_interval_samples: PARTIAL_INTERVAL_SAMPLES,
        }
    }

    /// Override how often partial passes run (in seconds), for callers that
    /// want snappier live updates (e.g. Minutes Madness). Floored at 0.5s so a
    /// pass can't be requested faster than it can plausibly complete. The
    /// default (2s) is used when this is never called, so core behavior is
    /// unchanged.
    pub fn set_partial_interval_secs(&mut self, secs: f32) {
        let samples = (secs * 16000.0).max(8000.0) as usize;
        self.partial_interval_samples = samples;
    }

    /// Feed audio samples. Returns a partial result if enough audio has
    /// accumulated since the last transcription.
    ///
    /// Once `audio_buffer` exceeds `partial_max_samples`, partials are skipped
    /// to avoid CPU runaway (cost grows with buffer length). The utterance
    /// still terminates correctly via `finalize()` when the caller detects
    /// silence or hits its own utterance cap. From the user's perspective,
    /// the live transcript stops refreshing during very long uninterrupted
    /// speech, then catches up at finalize.
    pub fn feed(&mut self, samples: &[f32], ctx: &WhisperContext) -> Option<StreamingResult> {
        self.audio_buffer.extend_from_slice(samples);
        self.samples_since_partial += samples.len();

        // Skip partial passes once the buffer is long enough that
        // `state.full()` would dominate the partial interval.
        if self.partial_max_samples > 0 && self.audio_buffer.len() > self.partial_max_samples {
            // Reset the counter so we don't fire a partial the instant we drop
            // back under the cap (which we won't until reset()).
            self.samples_since_partial = 0;
            return None;
        }

        // Only transcribe if enough new audio AND enough total audio
        if self.samples_since_partial >= self.partial_interval_samples
            && self.audio_buffer.len() >= MIN_TRANSCRIBE_SAMPLES
        {
            self.samples_since_partial = 0;
            return self.transcribe(ctx, false);
        }

        None
    }

    /// Finalize: run one last transcription and return the final result.
    /// Call this when silence is detected or the user stops.
    pub fn finalize(&mut self, ctx: &WhisperContext) -> Option<StreamingResult> {
        if self.audio_buffer.len() < MIN_TRANSCRIBE_SAMPLES {
            return None;
        }
        self.transcribe(ctx, true)
    }

    /// Reset the buffer for the next utterance (keeps the model loaded).
    pub fn reset(&mut self) {
        self.audio_buffer.clear();
        self.samples_since_partial = 0;
        self.last_partial.clear();
    }

    /// Total audio duration accumulated so far.
    pub fn duration_secs(&self) -> f64 {
        self.audio_buffer.len() as f64 / 16000.0
    }

    /// Run whisper on the full accumulated buffer.
    fn transcribe(&mut self, ctx: &WhisperContext, is_final: bool) -> Option<StreamingResult> {
        // Total per-call time including create_state (the synthetic benchmark
        // omits state creation — this captures the real in-situ cost).
        let call_start = std::time::Instant::now();
        // Suppress whisper's noisy C-level stderr output on subsequent state creations.
        // The first call prints GPU/backend info (useful); subsequent calls repeat it (noise).
        let mut state = if self.has_created_state {
            // Redirect stderr to /dev/null during state creation
            let state = suppress_stderr(|| ctx.create_state().ok());
            state?
        } else {
            self.has_created_state = true;
            ctx.create_state().ok()?
        };

        let mut params = streaming_whisper_params();
        params.set_n_threads(self.n_threads);
        params.set_language(self.language.as_deref());

        // Size the encoder context to the actual buffered audio. Without this,
        // whisper.cpp encodes a fixed 1500-mel-frame (30s) window on EVERY
        // call — short audio is zero-padded to 30s — so a 1.3s live buffer
        // pays the same ~6s encode as 30s. That flat per-call cost is the
        // root of live-scoring lag on CPU (measured: 6.2s/call regardless of
        // buffer length on an i7-14700KF). Sizing audio_ctx to the buffer
        // collapses it for short streaming utterances; whisper.cpp #1855
        // measured ~3.4x faster on short base.en clips with WER unchanged.
        // Long buffers near the cap resolve to ~1500 (the full window), which
        // is correct. Streaming-only: the batch path keeps the default 1500
        // because it processes 30s chunks of real audio.
        let audio_ctx = audio_ctx_for_samples(self.audio_buffer.len());
        params.set_audio_ctx(audio_ctx);

        let start = std::time::Instant::now();

        if let Err(e) = state.full(params, &self.audio_buffer) {
            tracing::warn!("streaming whisper failed: {}", e);
            return None;
        }

        let elapsed_ms = start.elapsed().as_millis();
        let duration_secs = self.audio_buffer.len() as f64 / 16000.0;

        // Opt-in live-perf diagnostics (env MINUTES_LIVE_TIMING; no-op otherwise).
        // Logs the REAL per-call cost — incl. create_state — vs buffer length,
        // plus cumulative dropped chunks, so we can see where live lag/drops
        // actually come from on the target hardware.
        let total_ms = call_start.elapsed().as_millis();
        crate::live_timing::log(&format!(
            "whisper {} buf={:.1}s actx={} threads={} full={}ms total(+state)={}ms rtf={:.2} dropped_chunks={}",
            if is_final { "FINAL  " } else { "partial" },
            duration_secs,
            audio_ctx,
            self.n_threads,
            elapsed_ms,
            total_ms,
            total_ms as f64 / 1000.0 / duration_secs.max(0.001),
            crate::live_timing::dropped_chunks()
        ));

        // Extract per-segment text, then run whisper-guard's segment cleaning
        // BEFORE joining. Whisper's decoder can loop, emitting the same phrase
        // across many consecutive segments (observed: a single spoken "move the
        // needle" transcribed as 38 repeated segments) — especially on short
        // buffers with a small audio_ctx. The BATCH transcription path runs
        // this cleaning; the streaming path historically did not, so raw
        // repetition reached the live transcript and inflated Madness buzzword
        // counts (one mention scored as 38). clean_segments collapses runs of
        // 3+ similar consecutive segments to the first, matching batch's
        // anti-hallucination behavior. (This is a core streaming/batch parity
        // gap, not Madness-specific.)
        let num_segments = state.full_n_segments();
        let mut segs: Vec<String> = Vec::with_capacity(num_segments.max(0) as usize);
        for i in 0..num_segments {
            if let Some(seg) = state.get_segment(i) {
                if let Ok(t) = seg.to_str_lossy() {
                    let t = t.trim();
                    if !t.is_empty() {
                        segs.push(t.to_string());
                    }
                }
            }
        }
        let (segs, _clean_stats) = whisper_guard::segments::clean_segments(&segs);
        // clean_segments collapses repetition across SEPARATE segments, but
        // whisper can also loop WITHIN a single segment
        // ("Double-click, double-click, double-click, ..." x55 from one
        // spoken phrase). Segment-level dedup sees that as one segment and
        // passes it through, so collapse intra-segment clause repetition too.
        let text = collapse_repeated_clauses(&segs.join(" "))
            .trim()
            .to_string();

        // Skip if empty or identical to last partial (no new info)
        if text.is_empty() {
            return None;
        }
        if !is_final && text == self.last_partial {
            return None;
        }

        tracing::debug!(
            partial = !is_final,
            words = text.split_whitespace().count(),
            audio_secs = format!("{:.1}", duration_secs),
            whisper_ms = elapsed_ms,
            "streaming transcription"
        );

        self.last_partial = text.clone();

        Some(StreamingResult {
            text,
            is_final,
            duration_secs,
        })
    }
}

/// Temporarily suppress stderr (whisper C code prints noisy init logs).
fn suppress_stderr<T>(f: impl FnOnce() -> T) -> T {
    #[cfg(unix)]
    {
        use std::os::unix::io::AsRawFd;
        let stderr_fd = std::io::stderr().as_raw_fd();
        let saved = unsafe { libc::dup(stderr_fd) };
        if saved >= 0 {
            let devnull = std::fs::OpenOptions::new()
                .write(true)
                .open("/dev/null")
                .ok();
            if let Some(ref dn) = devnull {
                unsafe { libc::dup2(dn.as_raw_fd(), stderr_fd) };
            }
            let result = f();
            unsafe { libc::dup2(saved, stderr_fd) };
            unsafe { libc::close(saved) };
            return result;
        }
    }
    f()
}

/// Thread count for the live streaming path.
///
/// Defaults to [`live_default_threads`] (available parallelism capped at 16),
/// overridable via the `MINUTES_LIVE_THREADS` env var for empirical tuning on
/// a target host. A non-positive or unparseable value falls back to the
/// default.
fn live_n_threads() -> i32 {
    if let Ok(v) = std::env::var("MINUTES_LIVE_THREADS") {
        if let Ok(n) = v.trim().parse::<i32>() {
            if n > 0 {
                return n;
            }
        }
    }
    live_default_threads()
}

/// Default live-path thread count: available parallelism capped at 16.
///
/// The shared `whisper_guard::num_cpus()` caps at 8, which a hardware sweep on
/// an i7-14700KF showed leaves ~30% on the table for the live path
/// (8 threads ≈ 3.4s/call, 16 ≈ 2.4s/call). Returns past 16 flatten hard
/// (20 ≈ 2.3s, ~5% over 16) and risk E-core drag plus starving the WebView2
/// UI / scoring loop of cores, so 16 is the sweet spot. The cap is a no-op on
/// smaller fleet machines (a 4- or 8-core laptop uses all its cores). Batch
/// transcription still uses the shared 8-cap — this only widens the live path.
fn live_default_threads() -> i32 {
    std::thread::available_parallelism()
        .map(|p| (p.get() as i32).min(16))
        .unwrap_or(8)
}

/// Compute whisper's encoder context size (`audio_ctx`) for a buffer of
/// `n_samples` 16kHz mono samples.
///
/// whisper.cpp's encoder runs over a fixed 1500-mel-frame window (≈30s) by
/// default, padding shorter audio — so per-call encode cost is constant
/// regardless of how much audio is actually buffered. For live streaming of
/// short utterances that's mostly wasted compute. Sizing audio_ctx to the
/// real buffer length (whisper.cpp #1855) recovers a meaningful speedup on
/// short clips with no measured accuracy loss.
///
/// Formula: `(secs/30)*1500 + 128` padding, rounded up to a multiple of 64
/// (whisper.cpp kernels prefer 64-aligned context), clamped to `[768, 1500]`.
/// A buffer at/over the 30s window resolves to the full 1500 (no reduction).
/// The 768 floor (not the audio's bare frame count) is deliberate: smaller
/// contexts starve the decoder and trigger repetition loops on short live
/// utterances. See the body for the full rationale.
/// Collapse whisper repetition loops that occur WITHIN a single segment,
/// e.g. `"Double-click, double-click, double-click, ..."` (×55) from one
/// spoken phrase. `whisper_guard::clean_segments` handles repetition spread
/// across separate segments, but not loops packed into one segment's text,
/// which is what reaches the live transcript on short utterances and inflates
/// Madness buzzword counts.
///
/// Splits on clause delimiters (`.`, `,`, `;`), and collapses any run of 3+
/// consecutive clauses that normalize to the same text down to a single copy.
/// The 3+ threshold matches `dedup_segments`, so genuine short repetition
/// ("very, very") is preserved. If no run is collapsed the original text is
/// returned verbatim, so non-looping transcripts are never reformatted.
fn collapse_repeated_clauses(text: &str) -> String {
    let normalize = |s: &str| {
        s.trim()
            .trim_end_matches(['.', ',', ';', '!', '?', ' '])
            .to_lowercase()
    };
    let clauses: Vec<&str> = text
        .split(['.', ',', ';'])
        .map(str::trim)
        .filter(|c| !c.is_empty())
        .collect();
    if clauses.len() < 3 {
        return text.to_string();
    }
    let mut out: Vec<&str> = Vec::with_capacity(clauses.len());
    let mut collapsed = false;
    let mut i = 0;
    while i < clauses.len() {
        let norm = normalize(clauses[i]);
        let mut j = i + 1;
        while j < clauses.len() && normalize(clauses[j]) == norm {
            j += 1;
        }
        if j - i >= 3 {
            out.push(clauses[i]); // collapse the run to one copy
            collapsed = true;
        } else {
            out.extend_from_slice(&clauses[i..j]);
        }
        i = j;
    }
    if collapsed {
        out.join(", ")
    } else {
        text.to_string()
    }
}

fn audio_ctx_for_samples(n_samples: usize) -> i32 {
    let audio_secs = n_samples as f32 / 16_000.0;
    let raw = (audio_secs / 30.0) * 1500.0 + 128.0;
    let rounded = ((raw / 64.0).ceil() as i32) * 64;
    // 1500 is the model's fixed n_audio_ctx (the full 30s window) and is NOT
    // 64-aligned — it's the hard ceiling. Anything that rounds to >= 1500 just
    // uses the full window (equivalent to the default). Reduced values stay
    // 64-aligned for kernel efficiency.
    //
    // Floor at 768, not 128: very small audio_ctx (we observed 192–256 on
    // 1–2s buffers) starves the decoder of context and triggers repetition
    // loops (one spoken phrase transcribed dozens of times). whisper.cpp
    // guidance flags sub-768 as loop-prone. 768 is loop-resistant and, on the
    // GPU backends Madness actually ships on (Vulkan/Metal), essentially free
    // — encode cost is dominated by the GPU, not the context size. The
    // aggressive sub-768 reduction only ever benefited rare CPU-only hosts,
    // and the streaming dedup passes catch any residual loops regardless.
    if rounded >= 1500 {
        1500
    } else {
        rounded.max(768)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_streaming_whisper_has_empty_buffer() {
        let sw = StreamingWhisper::new(None);
        assert_eq!(sw.duration_secs(), 0.0);
        assert!(sw.audio_buffer.is_empty());
    }

    #[test]
    fn collapse_repeated_clauses_kills_intra_segment_loop() {
        // Regression: whisper looped one spoken "double-click" into 55 comma-
        // separated repeats inside a SINGLE segment, which scored 55. Clause
        // collapse must reduce it to one mention.
        let looped = std::iter::repeat("double-click")
            .take(55)
            .collect::<Vec<_>>()
            .join(", ");
        let out = collapse_repeated_clauses(&looped);
        assert_eq!(
            out.to_lowercase().matches("double-click").count(),
            1,
            "expected one mention after collapse, got: {out}"
        );
        // Non-looping text is returned unchanged (no reformatting).
        let normal = "Let's circle back on synergy and bandwidth.";
        assert_eq!(collapse_repeated_clauses(normal), normal);
        // Genuine short repetition (< 3) is preserved.
        let short = "very, very good";
        assert_eq!(collapse_repeated_clauses(short), short);
    }

    #[test]
    fn clean_segments_collapses_whisper_repetition_loop() {
        // Regression guard: whisper's decoder can loop, emitting one spoken
        // phrase as many repeated segments (observed: a single "move the
        // needle" transcribed as 38 repeats, which scored 38 in Madness).
        // The streaming path runs clean_segments before joining; this asserts
        // that pass collapses the loop so live buzzword counts stay accurate.
        let looped: Vec<String> = std::iter::repeat("Move the needle.".to_string())
            .take(38)
            .collect();
        let (cleaned, _) = whisper_guard::segments::clean_segments(&looped);
        assert!(
            cleaned.len() < 4,
            "expected repetition loop collapsed, got {} segments",
            cleaned.len()
        );
    }

    #[test]
    fn audio_ctx_scales_with_buffer_and_clamps() {
        // Results are in [768, 1500]; reduced values are 64-aligned, and the
        // 1500 ceiling (model's full n_audio_ctx) is the one allowed exception.
        for &samples in &[0usize, 16_000, 80_000, 240_000, 480_000, 960_000] {
            let ctx = audio_ctx_for_samples(samples);
            assert!((768..=1500).contains(&ctx), "ctx {ctx} out of range");
            assert!(ctx == 1500 || ctx % 64 == 0, "ctx {ctx} not 64-aligned");
        }
        // Short buffers floor at 768 (loop-resistant; smaller contexts induced
        // whisper repetition loops on 1-2s utterances).
        assert_eq!(audio_ctx_for_samples(0), 768, "empty floors at 768");
        assert_eq!(audio_ctx_for_samples(16_000), 768, "1s floors at 768");
        assert_eq!(audio_ctx_for_samples(80_000), 768, "5s floors at 768");
        // Monotonic: more audio → larger (or equal) context.
        assert!(audio_ctx_for_samples(16_000) <= audio_ctx_for_samples(240_000));
        assert!(audio_ctx_for_samples(240_000) <= audio_ctx_for_samples(480_000));
        // A full 30s+ window resolves to the max (no reduction).
        assert_eq!(audio_ctx_for_samples(16_000 * 30), 1500);
        assert_eq!(audio_ctx_for_samples(16_000 * 60), 1500);
    }

    #[test]
    fn feed_below_interval_returns_none() {
        let mut sw = StreamingWhisper::new(None);
        // Feed 1 second of silence (below 2s interval)
        let silence = vec![0.0f32; 16000];
        // We can't test with a real WhisperContext without a model,
        // but we can verify the buffer grows correctly
        sw.audio_buffer.extend_from_slice(&silence);
        sw.samples_since_partial += silence.len();
        assert_eq!(sw.duration_secs(), 1.0);
        assert_eq!(sw.samples_since_partial, 16000);
    }

    #[test]
    fn reset_clears_state() {
        let mut sw = StreamingWhisper::new(Some("en".into()));
        sw.audio_buffer.extend_from_slice(&[0.0; 16000]);
        sw.samples_since_partial = 16000;
        sw.last_partial = "hello".into();

        sw.reset();

        assert!(sw.audio_buffer.is_empty());
        assert_eq!(sw.samples_since_partial, 0);
        assert!(sw.last_partial.is_empty());
        assert_eq!(sw.duration_secs(), 0.0);
    }

    #[test]
    fn partial_max_samples_is_set_from_secs() {
        let sw = StreamingWhisper::with_partial_max_secs(None, 45);
        assert_eq!(sw.partial_max_samples, 45 * 16000);

        let sw_default = StreamingWhisper::new(None);
        assert_eq!(
            sw_default.partial_max_samples,
            DEFAULT_PARTIAL_MAX_SECS as usize * 16000
        );
    }

    #[test]
    fn zero_partial_max_disables_cap() {
        let sw = StreamingWhisper::with_partial_max_secs(None, 0);
        assert_eq!(sw.partial_max_samples, 0);
        // The feed() check `partial_max_samples > 0` short-circuits the cap.
    }
}

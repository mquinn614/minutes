//! Runtime micro-benchmark of whisper transcription cost on the live model.
//!
//! Live buzzword scoring (Minutes Madness) must pick a partial cadence + cap
//! whose sustained whisper work stays under real time on the host. The pure
//! cost-model math lives in [`crate::live_autotune`]; this module times the
//! data points that math is fitted to, sharing one ladder definition with the
//! `whisper_rtf` benchmark example so the projection coming out of the runtime
//! probe matches the table the benchmark prints. The reference audio is
//! bundled at compile time (the same ~10.6s clip the benchmark uses), so the
//! probe is self-contained on every host.
//!
//! Behind the `whisper` feature: needs `whisper_rs` to actually run inference.
//! The pure math in [`crate::live_autotune`] stays dep-free and works
//! everywhere.
//!
//! End-to-end shape: caller picks a model file + use_gpu, [`probe_cost`] times
//! N short transcriptions and fits a [`CostModel`], then
//! [`crate::live_autotune::choose`] over [`default_ladder`] returns the
//! snappiest config the host can sustain.

use std::io::Cursor;
use std::path::Path;
use std::sync::OnceLock;
use std::time::Instant;

use whisper_rs::{FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters};

use crate::live_autotune::{CostModel, LiveCandidate, RtfPoint};

/// Bundled demo clip (16 kHz mono i16 WAV, ~10.6s) the benchmark and probe share.
const DEMO_WAV: &[u8] = include_bytes!("../../assets/demo.wav");

/// Decode the embedded WAV to f32 samples in [-1, 1], cached for the process.
///
/// Asserts the bundled file is 16 kHz mono — the only format both the live
/// pipeline and whisper.cpp consume directly. A panic here is a build-time
/// asset regression, not a runtime user condition.
fn demo_samples() -> &'static [f32] {
    static SAMPLES: OnceLock<Vec<f32>> = OnceLock::new();
    SAMPLES.get_or_init(|| {
        let mut reader = hound::WavReader::new(Cursor::new(DEMO_WAV))
            .expect("bundled demo.wav must be a valid WAV");
        let spec = reader.spec();
        assert_eq!(spec.sample_rate, 16000, "bundled demo.wav must be 16 kHz");
        assert_eq!(spec.channels, 1, "bundled demo.wav must be mono");
        reader
            .samples::<i16>()
            .map(|s| s.expect("read sample") as f32 / 32768.0)
            .collect()
    })
}

/// Total duration of the bundled clip in seconds.
pub fn demo_duration_secs() -> f64 {
    demo_samples().len() as f64 / 16000.0
}

/// Buffer lengths (seconds) used by the runtime probe. Mirrors the first five
/// points of the `whisper_rtf` benchmark — enough to fit the linear cost model
/// (`CostModel::fit` uses the first and last) and short enough to keep the
/// probe under ~10s on a typical CPU.
pub const PROBE_LENGTHS: &[f64] = &[1.0, 2.0, 3.0, 5.0, 8.0];

/// The candidate ladder, snappiest → safest. Identical to the table the
/// `whisper_rtf` example prints so a host that picks "partials 2.5s + 5s cap"
/// in the bench picks the same in the probe.
pub fn default_ladder() -> Vec<LiveCandidate> {
    vec![
        LiveCandidate::partials(1.5, 10.0),
        LiveCandidate::partials(2.5, 5.0),
        LiveCandidate::partials(3.0, 6.0),
        LiveCandidate::partials_off(3.0),
        LiveCandidate::partials_off(5.0),
    ]
}

/// Transcribe `samples` once with whisper-cli-compatible params and return
/// `(elapsed_secs, transcript)`. Mirrors `whisper_rtf::transcribe_time` so the
/// probe and the benchmark fit the same cost model.
fn transcribe_time(ctx: &WhisperContext, samples: &[f32], threads: i32) -> (f64, String) {
    let mut state = ctx.create_state().expect("create state");
    let mut params = FullParams::new(SamplingStrategy::Greedy { best_of: 1 });
    params.set_n_threads(threads);
    params.set_language(Some("en"));
    params.set_print_special(false);
    params.set_print_progress(false);
    params.set_print_realtime(false);
    params.set_print_timestamps(false);

    let t = Instant::now();
    state.full(params, samples).expect("whisper full");
    let elapsed = t.elapsed().as_secs_f64();

    let mut text = String::new();
    for i in 0..state.full_n_segments() {
        if let Some(seg) = state.get_segment(i) {
            if let Ok(s) = seg.to_str_lossy() {
                text.push_str(&s);
            }
        }
    }
    (elapsed, text.trim().to_string())
}

/// One warm-up transcription is discarded before the timed passes so the
/// returned points reflect steady-state whisper.cpp behaviour, not first-call
/// overhead. Returns the fitted [`CostModel`] plus the raw points (handy for
/// logging or surfacing in the panel).
///
/// `model_path` must point at a ggml whisper model file on disk. `use_gpu`
/// toggles the underlying GPU backend if the build was compiled with one;
/// without a GPU-enabled feature it is silently ignored by whisper.cpp.
pub fn probe_cost(
    model_path: &Path,
    lengths: &[f64],
    threads: i32,
    use_gpu: bool,
) -> Result<(CostModel, Vec<RtfPoint>), String> {
    if lengths.len() < 2 {
        return Err("probe_cost needs at least 2 buffer lengths to fit".into());
    }
    let samples = demo_samples();
    let total_secs = samples.len() as f64 / 16000.0;
    if lengths.iter().any(|&l| l > total_secs + 0.01) {
        return Err(format!(
            "probe length exceeds bundled clip duration ({:.1}s)",
            total_secs
        ));
    }

    let model_str = model_path
        .to_str()
        .ok_or_else(|| format!("model_path is not valid UTF-8: {}", model_path.display()))?;
    let ctx = WhisperContext::new_with_params(
        model_str,
        WhisperContextParameters {
            use_gpu,
            ..Default::default()
        },
    )
    .map_err(|e| format!("load model {}: {}", model_path.display(), e))?;

    // Discard the first call; it pays one-time backend warm-up and would
    // poison the linear fit. Use the shortest length for warm-up to keep the
    // probe fast.
    let warm_n = ((16000.0 * lengths[0]) as usize).min(samples.len());
    let _ = transcribe_time(&ctx, &samples[..warm_n], threads);

    let mut pts = Vec::with_capacity(lengths.len());
    for &l in lengths {
        let n = ((16000.0 * l) as usize).min(samples.len());
        let (elapsed, _text) = transcribe_time(&ctx, &samples[..n], threads);
        pts.push(RtfPoint::new(l, elapsed));
    }
    let cost = CostModel::fit(&pts).ok_or("cost-model fit failed (need >=2 distinct points)")?;
    Ok((cost, pts))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn demo_clip_is_16k_mono_and_long_enough() {
        // Confirms the embedded asset shape the probe relies on, without
        // running whisper. If the clip is ever rebundled with different
        // characteristics this test catches it immediately.
        assert!(demo_duration_secs() >= 8.0);
    }

    #[test]
    fn default_ladder_is_snappiest_first() {
        // `choose()` returns the first candidate that clears OK_THRESHOLD,
        // which only means "snappiest" if the ladder is ordered
        // snappiest → safest. Verify monotone non-increasing load across a
        // representative cost model (any cost with positive overhead +
        // marginal exhibits this for the configured ladder).
        let ladder = default_ladder();
        let cost = CostModel {
            overhead_secs: 1.0,
            marginal_rtf: 0.5,
        };
        let loads: Vec<f64> = ladder.iter().map(|c| c.projected_load(&cost)).collect();
        for w in loads.windows(2) {
            assert!(
                w[1] <= w[0] + 1e-9,
                "ladder not ordered snappiest→safest: {:?}",
                loads
            );
        }
        assert_eq!(ladder[0], LiveCandidate::partials(1.5, 10.0));
        assert_eq!(
            ladder.last().copied(),
            Some(LiveCandidate::partials_off(5.0))
        );
    }

    #[test]
    fn probe_lengths_fit_in_demo_clip() {
        // Guard against shrinking the clip or expanding PROBE_LENGTHS in a
        // way that would make `probe_cost` reject all calls at runtime.
        let total = demo_duration_secs();
        for &l in PROBE_LENGTHS {
            assert!(l <= total + 0.01, "{} > {}", l, total);
        }
    }
}

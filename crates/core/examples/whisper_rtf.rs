//! Whisper real-time-factor (RTF) benchmark for live-scoring tuning.
//!
//! Measures per-buffer transcription cost — including the fixed per-call
//! overhead that hurts short partial buffers — for each installed whisper model
//! on THIS machine, then projects whether candidate live configs (partial
//! interval + utterance cap) can keep up in real time. This is the number that
//! decides the Minutes Madness Windows live-scoring fix; run it on the target
//! hardware (especially the CPU-only Windows hosts).
//!
//! Run:
//!   cargo run --release --example whisper_rtf --features whisper            # CPU baseline
//!   cargo run --release --example whisper_rtf --features whisper,vulkan     # GPU (broad: NVIDIA/AMD/Intel)
//!   cargo run --release --example whisper_rtf --features whisper,cuda       # GPU (NVIDIA only)
//!   cargo run --release --example whisper_rtf --features whisper,metal      # macOS GPU
//! Optionally pass a 16kHz mono WAV path; defaults to the bundled demo clip.
//!
//! Reads models from ~/.minutes/models/ggml-{tiny,base,small}.bin (whichever exist).

use minutes_core::live_autotune::{verdict, CostModel, LiveCandidate, RtfPoint, Verdict};
use std::time::Instant;
use whisper_rs::{FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters};

fn backend() -> &'static str {
    if cfg!(feature = "metal") {
        "metal (GPU)"
    } else if cfg!(feature = "cuda") {
        "cuda (GPU)"
    } else if cfg!(feature = "vulkan") {
        "vulkan (GPU)"
    } else if cfg!(feature = "coreml") {
        "coreml"
    } else if cfg!(feature = "hipblas") {
        "hipblas (GPU)"
    } else {
        "cpu"
    }
}

fn gpu_compiled() -> bool {
    cfg!(any(
        feature = "metal",
        feature = "cuda",
        feature = "vulkan",
        feature = "coreml",
        feature = "hipblas"
    ))
}

fn models_dir() -> std::path::PathBuf {
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .unwrap_or_default();
    std::path::Path::new(&home).join(".minutes").join("models")
}

fn load_wav_16k_mono(path: &std::path::Path) -> Vec<f32> {
    let mut reader = hound::WavReader::open(path)
        .unwrap_or_else(|e| panic!("open wav {}: {}", path.display(), e));
    let spec = reader.spec();
    assert_eq!(
        spec.sample_rate, 16000,
        "need a 16kHz wav (got {})",
        spec.sample_rate
    );
    assert_eq!(
        spec.channels, 1,
        "need a mono wav (got {} ch)",
        spec.channels
    );
    reader
        .samples::<i16>()
        .map(|s| s.expect("read sample") as f32 / 32768.0)
        .collect()
}

/// Transcribe `samples` once and return (elapsed_secs, transcript).
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

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let wav_path = args
        .get(1)
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../assets/demo.wav")
        });
    let threads = std::thread::available_parallelism()
        .map(|n| n.get() as i32)
        .unwrap_or(4);

    println!("whisper RTF benchmark");
    println!(
        "  backend (compiled): {}   use_gpu={}",
        backend(),
        gpu_compiled()
    );
    println!("  threads: {}", threads);
    println!("  wav: {}", wav_path.display());

    let samples = load_wav_16k_mono(&wav_path);
    let total_secs = samples.len() as f64 / 16000.0;
    println!("  audio: {:.1}s\n", total_secs);

    let lengths: Vec<f64> = [1.0, 2.0, 3.0, 5.0, 8.0, 10.0]
        .into_iter()
        .filter(|&l| l <= total_secs + 0.01)
        .collect();
    if lengths.len() < 2 {
        eprintln!("audio too short ({:.1}s); need >=2s for a fit", total_secs);
        std::process::exit(1);
    }

    let mdir = models_dir();
    let models: Vec<(&str, std::path::PathBuf)> = ["tiny", "base", "small"]
        .into_iter()
        .filter_map(|n| {
            let p = mdir.join(format!("ggml-{}.bin", n));
            p.exists().then_some((n, p))
        })
        .collect();
    if models.is_empty() {
        eprintln!("no ggml-*.bin models in {}", mdir.display());
        std::process::exit(1);
    }

    for (name, path) in &models {
        println!("=== model: {} ===", name);
        let ctx = WhisperContext::new_with_params(path.to_str().unwrap(), {
            let mut p = WhisperContextParameters::default();
            p.use_gpu = gpu_compiled();
            p
        })
        .expect("load model");

        // Warm-up: the first call pays one-time backend/model warm-up; discard it.
        let warm = ((16000.0 * lengths[0]) as usize).min(samples.len());
        let _ = transcribe_time(&ctx, &samples[..warm], threads);

        let mut pts: Vec<RtfPoint> = Vec::new();
        let mut sanity = String::new();
        for &l in &lengths {
            let n = ((16000.0 * l) as usize).min(samples.len());
            let (elapsed, text) = transcribe_time(&ctx, &samples[..n], threads);
            let rtf = elapsed / l;
            println!(
                "  {:>4.1}s buffer -> {:>6.0} ms  (RTF {:.2}{})",
                l,
                elapsed * 1000.0,
                rtf,
                if rtf > 1.0 {
                    "  << slower than real time"
                } else {
                    ""
                }
            );
            pts.push(RtfPoint::new(l, elapsed));
            sanity = text;
        }

        // Shared cost model + projection (see crate::live_autotune): the runtime
        // auto-tuner uses the exact same math so this table predicts what it picks.
        let cost = CostModel::fit(&pts).expect("need >=2 timing points");
        println!(
            "  fit: per-call overhead ~{:.0} ms, marginal ~{:.2}x real time",
            cost.overhead_secs * 1000.0,
            cost.marginal_rtf
        );

        // Project sustained live load for candidate configs. work/realtime must
        // stay below ~1.0 (with margin) or whisper backs up and drops audio.
        let configs: &[(LiveCandidate, &str)] = &[
            (
                LiveCandidate::partials(1.5, 10.0),
                "partials 1.5s + 10s cap (original, failed)",
            ),
            (
                LiveCandidate::partials(2.5, 5.0),
                "partials 2.5s + 5s cap (old Windows)",
            ),
            (LiveCandidate::partials(3.0, 6.0), "partials 3.0s + 6s cap"),
            (
                LiveCandidate::partials_off(3.0),
                "no partials + 3s cap (current Windows)",
            ),
            (LiveCandidate::partials_off(5.0), "no partials + 5s cap"),
        ];
        println!(
            "  projected live load (work/realtime; <0.85 OK, <1.0 tight, >=1.0 falls behind):"
        );
        for (cand, label) in configs {
            let ratio = cand.projected_load(&cost);
            let verdict_str = match verdict(ratio) {
                Verdict::Ok => "OK",
                Verdict::Tight => "TIGHT",
                Verdict::FallsBehind => "FALLS BEHIND",
            };
            println!("    {:<40} {:.2}  {}", label, ratio, verdict_str);
        }
        let preview: String = sanity.chars().take(110).collect();
        println!("  sanity transcript: {}\n", preview);
    }

    if !gpu_compiled() {
        println!("NOTE: CPU-only build. Re-run with `--features whisper,vulkan` (broad) or");
        println!("      `--features whisper,cuda` (NVIDIA) to measure the GPU ceiling.");
    }
    println!(
        "NOTE: VAD is off here; the live path uses Silero VAD which skips silence,\n\
         so real-world load on speech-with-pauses is at or below these numbers."
    );
}

# Minutes Madness — Windows live-scoring perf handoff

Context handoff for a Claude Code session running **on the Windows PC** where
live scoring is laggy. (The prior work happened on a Mac, which can't reproduce
this CPU-bound bug.) Everything below is on the `experiments` branch of the fork
`mquinn614/minutes`. Work here, commit, and push to `experiments` to stay in
sync. Do NOT PR upstream (`silverstein/minutes`).

## The product
**Minutes Madness** = March-Madness-style buzzword bracket for all-hands calls,
built on top of the local live-transcript pipeline. It's a pure ADD-ON: must not
change core Minutes behavior, deps, config, or release surface. Panel:
`tauri/src/madness.html`. Engine: `crates/core/src/madness.rs`. Tauri cmds:
`tauri/src-tauri/src/commands.rs` (`cmd_madness_*`, `cmd_start_live_transcript`).
The next company All-Hands is ~1 week out; Windows colleagues host live.

## The remaining bug (this is the whole job)
On the **CPU-only Windows build** (no Metal; whisper.cpp AVX2), live buzzword
scoring is **extremely laggy** — words light up tens of seconds late and, when
whisper falls far enough behind, the bounded audio queue overflows and spoken
buzzwords are **dropped entirely**. macOS is fine (Metal GPU).

Already FIXED and verified on Windows (don't re-investigate these):
- Frozen 0:00 timer + false "no audio" nudge → was `session_status()` gating
  stats on a PID file held under an exclusive `fs2`/`LockFileEx` lock (read
  fails on Windows). Panel now uses wall-clock timer + `audioLevel`. Upstream
  issue filed: silverstein/minutes#258.
- Missing default aliases, players-not-shown-pre-scoring, fanfare acronym font,
  "Reset to defaults" link — all shipped.

The lag itself is the open item. Mechanism: with partials on, every partial
re-transcribes the *entire growing utterance buffer*; on this CPU whisper can't
do that in real time, so it backs up and drops audio. Current Windows config
(panel `startRecording`): `{ cap: 5, partials: true, partialSecs: 2.5 }`. macOS:
`{ cap: 10, partials: true, partialSecs: 1.5 }`. **User reports 2.5s/5s is still
extremely laggy** — so this CPU is slower than the ~0.43x-realtime estimate from
batch logs, likely due to per-whisper-call overhead on short partial buffers.

## DISTRIBUTION TARGET (read this — it shapes the whole fix)
This will spread to MULTIPLE non-technical hosts on VARIED, often weak hardware
(work laptops, integrated GPUs), each running their own events. They get ONE
installer; they will not install CUDA toolkits or tune configs. Implications:
- The shipped baseline must run on essentially ANY Windows x64 → that means
  **CPU**, and it must stay acceptable on a *mid/low* machine, not just a fast one.
- **GPU is opportunistic, not the deliverable.** For breadth prefer **Vulkan**
  (uses a GPU when present, falls back to CPU, works NVIDIA/AMD/Intel) over CUDA
  (NVIDIA-only, heavy runtime dep, won't launch without it). A CUDA build is only
  for a *known* dedicated NVIDIA host.
- **Best multi-host answer = runtime auto-adaptation:** at session start,
  micro-benchmark whisper on a short clip to get the machine's real-time factor
  (or detect an active GPU backend), then auto-pick model + partial interval +
  cap. Fast machines get snappy; weak ones get conservative-but-working, with no
  user config. This is the robust target — design toward it.
- So: prove a solid CPU baseline FIRST (works on a weak machine). Treat GPU
  (Vulkan) as a possible single broad artifact only if it falls back cleanly and
  is stable across drivers. The on-PC GPU check is a data point (ceiling + is
  Vulkan-as-baseline viable?), not the goal.

## DO THIS FIRST: measure, don't guess
A ready-made benchmark exists — `crates/core/examples/whisper_rtf.rs`. It times
per-buffer transcription (incl. per-call overhead on short partial buffers) for
each installed model and PROJECTS whether each candidate live config keeps up.
Run it on this PC (needs the Rust toolchain — see build section):

```
cargo run --release --example whisper_rtf --features whisper           # CPU baseline
cargo run --release --example whisper_rtf --features whisper,vulkan     # GPU (broad)
cargo run --release --example whisper_rtf --features whisper,cuda       # GPU (NVIDIA)
```
Optionally pass a 16kHz mono WAV; defaults to the bundled `crates/assets/demo.wav`.
Run it 2-3x (per-run noise) and read the "projected live load" table — a config
< ~0.85 keeps up, >= 1.0 falls behind. On the Mac (M-series CPU) even `small`
projected < 1.0; the user's PC fails at 1.5s/10s, so expect much higher numbers
there — the table tells you exactly which model + interval + cap crosses back
under 1.0. That picks the CPU baseline; the GPU runs show the ceiling.

Also cross-check live: watch `~/.minutes/events.jsonl` `live.utterance.final`
`offset_ms` vs wall-clock during a real Madness recording to see backlog grow.

## PRIMARY GOAL: a CPU baseline that works on a typical/weak host
This must work for everyone (see Distribution target). Decide from the measured
real-time factor on this PC (and remember other hosts may be slower):
- If base is mildly over real time: partials OFF + short cap (cap:3) → one
  transcription per utterance, ~3-4s latency, no backlog. Panel-only, universal.
- If base still can't keep up finalize-only: live model `tiny` on CPU
  (`config.live_transcript.model`). Earlier validation: tiny 12/16 vs base 14/16
  and garbled multi-word terms — mitigate with wider aliases, or accept for speed.
- Strongly consider RUNTIME AUTO-ADAPT (the multi-host win): micro-benchmark at
  session start, pick model/interval/cap from the measured RTF so every host
  self-tunes. Worth the effort given this spreads to varied machines.

## GPU as an OPPORTUNISTIC enhancement (data point on this PC; not the baseline)
The cuda/vulkan feature flags are ALREADY wired (`crates/core/Cargo.toml`:
`cuda`/`vulkan`/`metal` → `whisper-rs/*`, plus cli + tauri). The shipped Windows
CI build is CPU-only by choice (`--features parakeet`).
1. Detect the GPU: `nvidia-smi` or `wmic path win32_VideoController get name`.
2. Test build with `--features parakeet,vulkan` (broad; needs Vulkan SDK at build,
   a Vulkan driver at run; SHOULD fall back to CPU if no device — verify that).
   `--features parakeet,cuda` only for a known NVIDIA host (CUDA Toolkit + runtime).
3. Goal of the test: (a) does GPU make base real-time here? (ceiling check), and
   (b) is a Vulkan build a viable SINGLE broad artifact (fast w/ GPU, clean CPU
   fallback, stable across drivers)? If yes, it could ship to all hosts; if the
   fallback/driver story is shaky, ship CPU-adaptive and reserve GPU builds for
   power hosts.
4. A CUDA build is NOT a multi-host deliverable (won't launch without NVIDIA).
- If base is only mildly over real-time: drop partials entirely on Windows
  (`partials:false`) + short cap (`cap:3`) → one transcription per utterance,
  ~3–4s latency, no backlog. Rock-solid. Panel-only.
- If base can't even keep up finalize-only: switch the LIVE model to `tiny` on
  Windows (live model = `config.live_transcript.model`, empty → dictation
  `base`). NOTE: validated tiny earlier — it captured 12/16 buzzwords vs base
  14/16 and garbled multi-word terms ("value ad", fused "move the needle/boil
  the ocean"). Mitigation worth trying: widen aliases (add "value ad", etc.) or
  accept lower accuracy for speed. Measure tiny's real RTF here first.
- Hybrid: tiny for live partials (speed) + base re-score of finalized
  utterances (accuracy). More work; only if needed.
- The partial-interval knob is already plumbed: `partial_interval_secs:
  Option<f64>` through `cmd_start_live_transcript` → `run_live_session` →
  `live_transcript::run`/`run_inner` (core/CLI pass `None` → 1.5s default).
  Panel sends camelCase `partialSecs`.

## Build/iterate on Windows
- Toolchain: `rustup` (repo pins via `rust-toolchain.toml`), **Visual Studio
  Build Tools (MSVC + C++)**, and **CMake** — whisper-rs-sys/knf-rs-sys build
  native code via CMake. The CI (`.github/workflows/release-windows-desktop.yml`)
  shows the exact build: `cargo tauri build --features parakeet --ci --bundles
  nsis --no-sign` with `GGML_AVX2=ON GGML_AVX512=OFF` etc.
- Fast loop: `cargo tauri dev` (from `tauri/src-tauri`) serves the frontend live
  and rebuilds Rust incrementally — far faster than the CI round-trip.
- If local native build is too painful, fall back to CI:
  `gh workflow run "Release Windows Desktop" -R mquinn614/minutes --ref experiments`
  then download artifact `minutes-desktop-windows-x64` (~16 min).
- Live transcript needs the `base` whisper model present (it is, on this PC).

## Guardrails
- Add-on isolation: keep changes in `madness.html` + the already-plumbed opt-in
  args. The `partial_interval_secs`/`cap`/`emit_partials` params default to
  existing behavior so Mat's CLI/Live mode are untouched.
- `~/.minutes/` (incl. `madness/*.json`) persists across reinstall; use the
  panel's "Modify Buzzwords → Reset to defaults" or delete the json to refresh
  a stale bracket.
- Verify by speaking real bracket buzzwords (synergy, AI powered, bandwidth,
  pivot, headwinds, paradigm shift) and watching flash latency + that ALL land.

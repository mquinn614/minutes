# Minutes Madness — Final Push Checklist (All-Hands Ready)

End-of-night state (2026-05-28 ~02:30 ET) → All-Hands ~1 week out. This is the
ordered work plan to ship a calibrated, multi-host-safe build with confidence,
**plus the lessons from the autotuner-shipped-before-calibration miss** so they
don't happen again. Cross off items as you go; work top-to-bottom, with the
parallel-OK tracks marked.

## Where we are

- **CPU safety-floor build dispatched** (run `26558590076`) — forces
  `{cap:5, partials:false}` on cpu backend; should finish overnight and become
  the shippable fleet installer.
- **Vulkan workflow failed** — whisper.cpp's GGML-Vulkan backend hits MSBuild
  parallel-dependency ordering errors on Windows runners with the default VS
  generator. Known fix: force Ninja. Not yet applied.
- **`MINUTES_LIVE_TIMING` instrumentation shipped** but never used to capture
  real-world data — that's the missing calibration loop (see "What we missed").
- Mac dev app rebuilt + relaunched with all today's fixes.

## What we missed (so we don't miss it again)

The proper order for shipping the autotuner was **1 → 5**, not **4 → 1**:

1. Build probe + cost model + ladder ✅
2. Build `MINUTES_LIVE_TIMING` instrumentation ✅
3. **Capture real per-call live-path cost** on the target hardware ⛔ skipped
4. **Compare real vs. projected; calibrate** (multiplier / threshold / ladder) ⛔ skipped
5. Ship the calibrated autotuner ✅ (but on un-calibrated numbers)

Result: probe projected `0.37 / Ok` for a config that drops half the audio in
practice. The architecture is fine; it shipped without its empirical input.

**Rule for next architectural piece:** if a feature's correctness depends on
empirical data from instrumentation, the instrumentation must run on the target
hardware *before* the feature ships downstream. Calibration is part of the
deliverable, not a follow-up.

---

## Phase 0 — Verify overnight state (~10 min, do first)

- [ ] **0.1** Check CPU safety-floor build status:
  `gh run list -R mquinn614/minutes --workflow "Release Windows Desktop" -L 1`
  - If success → download artifact, refresh `~/Desktop/Minutes-Madness-Prototype/minutes-desktop-windows-x64-setup.exe`.
  - If failed → diagnose; this is the must-ship fleet artifact, fix takes priority.
- [ ] **0.2** Confirm Mac dev app still runs cleanly (relaunched last night).
- [ ] **0.3** Sync both machines: `git pull origin experiments` on PC + Mac.

## Phase 1 — Close the calibration loop (the missing 3+4) ~1–2 hr

This is the most important phase. Goal: probe's projected load = real load
within ~20%, so the autotuner can pick configs that actually work.

- [ ] **1.1** Install the fresh CPU safety-floor build on the PC. Confirm
  live recording works end-to-end: read all 16 buzzwords with natural pauses,
  expect every one to register within ~5s, **zero drops**. If this fails,
  even the safety floor isn't safe — fall back further (cap:6 or tiny model).
- [ ] **1.2** **Capture real live-path cost** on the PC:
  ```
  set MINUTES_LIVE_TIMING=1
  # launch the installed app (or cargo tauri dev --features parakeet)
  ```
  Open Madness, start a live recording, speak buzzwords for ~30s, stop.
  `%USERPROFILE%\.minutes\live-timing.log` will have per-call elapsed/buffer/RTF.
- [ ] **1.3** **Capture probe projection** for the same configs:
  delete `%USERPROFILE%\.minutes\madness\probe-base-cpu.json` and re-launch —
  next session will re-probe. Note the projected load it reports.
- [ ] **1.4** **Compute the calibration factor.** Compare `total(+state) ms`
  per call from the timing log vs the probe's per-call cost (from the cost
  model fit). Most likely real_load = projected_load × multiplier where
  multiplier ≈ 1.5–2.5 on CPU. Confirm by computing actual load over the run.
- [ ] **1.5** **Apply the calibration** in `crates/core/src/live_autotune.rs`:
  options in increasing invasiveness — pick the one supported by the data:
  - (a) Lower `OK_THRESHOLD` for CPU only (e.g., 0.85 → 0.45).
  - (b) Multiply projected load by the calibration factor for CPU backends in
    `project_load`/`verdict` (cleanest if the factor is roughly constant).
  - (c) Restrict the CPU candidate ladder in `live_probe::default_ladder()` to
    finalize-only configs (most aggressive; equivalent to permanent safety floor).
- [ ] **1.6** **Verify the calibrated tuner picks a config that actually
  works.** Delete the cache, re-probe, run a real live session, confirm:
  picked verdict ≠ "Ok" turned into actual-Falls-Behind; picked verdict ==
  "Ok" actually keeps up; no audio dropped.
- [ ] **1.7** **Remove the panel-side CPU safety floor in `madness.html`**
  once the tuner is calibrated and verified. Or keep it as a belt-and-suspenders
  with a comment noting it should be removable once data confirms reliability.
- [ ] **1.8** Commit + push: `madness: calibrate live autotune against
  real-path MINUTES_LIVE_TIMING data on i7 CPU`.
- [ ] **1.9** **Dispatch a new CPU Windows build** with the calibrated tuner.
  Install on PC, verify once more.

## Phase 2 — Vulkan power-user track (~1–2 hr, can run in parallel with Phase 1)

Power-user installer for hosts with a Vulkan-capable GPU. Not the fleet
default. **Run this in parallel with Phase 1 on a different terminal if
possible** — Vulkan iteration is mostly CI cycle time.

- [ ] **2.1** Fix `.github/workflows/windows-vulkan-experiment.yml` to use the
  Ninja CMake generator:
  ```yaml
  env:
    CMAKE_GENERATOR: Ninja
    CMAKE_BUILD_PARALLEL_LEVEL: "4"
  ```
  And ensure Ninja is installed on the runner (likely already there via VS
  Build Tools; if not, add a `choco install ninja` step).
- [ ] **2.2** Commit + push to both `experiments` and `main` (the workflow file
  must be on default branch for `workflow_dispatch`).
- [ ] **2.3** Re-dispatch. If it fails again, the next-likeliest issues are:
  Vulkan SDK env vars missing (`VULKAN_SDK` should be set by the install
  step), or whisper-rs-sys's build.rs forcing the VS generator. Iterate.
- [ ] **2.4** When green, place `minutes-desktop-windows-x64-vulkan-setup.exe`
  in the prototype folder.
- [ ] **2.5** Install on PC, run benchmark + a live recording:
  expect probe to detect `backend = vulkan`, pick a snappy config (likely
  partials 1.5s + 10s cap or 2.5s + 5s cap), Mac-like flash latency, zero
  drops. Capture `MINUTES_LIVE_TIMING` for Vulkan too — useful future data.

## Phase 3 — Full smoke test on both platforms (~30 min)

- [ ] **3.1** **PC, CPU build**: full live recording flow, all 16 buzzwords
  land. Auto-tuner panel info shows expected pick. Timer increments. No
  drops. Stop → fanfare. Champion correct.
- [ ] **3.2** **PC, Vulkan build** (if green): same test; expect significantly
  snappier flashes.
- [ ] **3.3** **PC, Transcript Mode**: paste the sample-all-hands transcript.
  Expect round-by-round reveal + fanfare.
- [ ] **3.4** **PC, multiplayer flow**: generate bracket code, open
  `picks.html` in a second browser/window, paste code, click winners, copy
  picks code, paste back into Players modal → "Added X" + standings show.
- [ ] **3.5** **PC, Modify Buzzwords**: drag from grip works, drag from
  textbox works (no highlight trail), click textbox positions cursor, ✕
  clears, Reset to defaults works.
- [ ] **3.6** **Mac dev app**: same flow as 3.1, expect snappy partials
  (Metal). Confirm no regressions from the safety-floor + Vulkan workflow
  changes (Mac shouldn't be affected, but verify).
- [ ] **3.7** **Edge cases worth a single sweep**: open Madness while the CLI
  has a recording running (attach behavior); start a fresh bracket via
  Modify Buzzwords → save with empty title (auto-name); RTO + AI alias
  matching (say "return to the office" — confirm RTO seed scores).

## Phase 4 — Package + colleague-ready handoff (~30 min)

- [ ] **4.1** Update `~/Desktop/Minutes-Madness-Prototype/README.md`:
  - One-line description of what's new since they last got the build.
  - First-run: download model in-panel, allow mic, "calibrating" appears
    briefly, then ready.
  - Note that flash latency on CPU hosts is ~5s; Vulkan installer (optional)
    is snappier for hosts with a capable GPU.
  - Tell them to delete `%USERPROFILE%\.minutes\madness\probe-*.json` if
    behavior seems off (forces re-probe).
- [ ] **4.2** Confirm the prototype folder has, with current timestamps:
  - `Minutes-Madness-Mac.zip`
  - `minutes-desktop-windows-x64-setup.exe` (CPU, calibrated)
  - `minutes-desktop-windows-x64-vulkan-setup.exe` (optional Vulkan)
  - `picks.html`, `sample-all-hands-transcript.txt`, `README.md`
- [ ] **4.3** Send to 1–2 close colleagues for a smoke run on *their*
  hardware. Their feedback before the All-Hands is the last calibration
  data point we'll get on machines we don't own.
- [ ] **4.4** Upload `picks.html` to your Bluehost domain so the URL is ready
  for the All-Hands invite.

## Phase 5 — Pre-All-Hands rehearsal (later in week, not tomorrow)

- [ ] **5.1** Dry-run with at least 2 colleagues' brackets imported via picks.
- [ ] **5.2** Confirm the live scoring keeps up through ~30 min of meeting-
  paced speech (not just 30s of rapid buzzwords).
- [ ] **5.3** Test recovery: stop + restart recording mid-meeting; verify
  scoring continues from the existing JSONL.

---

## Risk register (what could still go wrong tomorrow)

| Risk | If it happens | Mitigation |
|---|---|---|
| CPU safety-floor build failed overnight | Phase 1 blocked | Re-dispatch; if persistent, revert madness.html to last-known-good commit and rebuild |
| Even cap:5 + finalize-only too slow on PC | Calibration insufficient | Drop live model to `tiny` (escalate path is already wired); accept lower transcription accuracy |
| Vulkan workflow keeps failing after Ninja fix | No GPU artifact | Ship CPU-only for fleet; build Vulkan locally on PC via `cargo tauri build --features parakeet,vulkan` for the host machine specifically |
| Colleague machine drops audio despite calibration | Their CPU even slower than yours | Have them set `MINUTES_LIVE_TIMING=1`, send log; tune the calibration multiplier upward |
| MINUTES_LIVE_TIMING data hard to interpret | Slow phase 1 | Start with the simplest comparison: per-call total_ms × utterance_count vs. session duration |

## Architectural lessons (for the file, for next time)

1. **Calibration is a deliverable, not a follow-up.** When a feature's
   correctness depends on empirical inputs, those inputs must exist *before*
   the feature ships downstream. The `MINUTES_LIVE_TIMING` → tuner data flow
   should have been the last commit on the PR, not a tomorrow item.

2. **Synthetic measurements have an honesty gap.** Anything that runs in
   isolation underestimates the cost when the real production path is
   concurrent. Probe-in-isolation vs. live-path-with-VAD-and-IO is the same
   pattern as benchmark-in-CI vs. behavior-under-load.

3. **The fast host hides the bug.** The PC session correctly flagged "this
   i7+4070 doesn't reproduce the lag" — that warning should have gated
   shipping, not just been noted. The slower colleague machines (the actual
   fleet) were never measured directly.

4. **GPU vs CPU is a per-call-overhead problem, not a throughput problem.**
   On CPU, every whisper invocation pays a ~1.8s fixed cost on this i7;
   marginal RTF is ~0.04×. Vulkan/Metal collapse that overhead to
   microseconds. So on CPU, *call frequency*, not buffer length, is the
   cost lever — exactly why partials hurt and finalize-only wins on
   non-GPU hosts.

5. **Two-track distribution is real.** Universal CPU-only artifact (works
   everywhere, conservative defaults) + opportunistic GPU artifact (snappy
   on capable hosts) is the practical answer. Trying to find a single
   "Vulkan-with-clean-fallback" artifact is a future investigation, not
   a fleet requirement.

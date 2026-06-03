# Minutes Madness

**A March-Madness-style buzzword bracket for your all-hands.**

Seed a bracket with corporate buzzwords, have everyone predict which ones get said the most, then let Minutes Madness *listen* to the meeting and score the bracket live as the jargon flies. First player to call the champion buzzword wins.

It transcribes entirely on your machine — no cloud, no API keys, no audio ever leaving your computer.

> Built on top of [**Minutes**](https://github.com/silverstein/minutes), the open-source, local-first conversation-memory engine. Minutes Madness reuses its local audio capture + whisper.cpp transcription and adds the bracket game on top. ↗

---

## What it does

- **Draft a bracket.** 16 buzzwords seeded into a tournament — the default set runs `game changer`, `AI`, `circle back`, `drill down`, `low-hanging fruit`, `move the needle`, `RTO`, `paradigm shift`, and more. Edit your own via **Modify Buzzwords** (each seed can carry alias spellings).
- **Players make picks.** Everyone predicts the winner of each matchup and the champion buzzword, via a share code or the standalone picks page.
- **Listen + score live.** During the meeting, Minutes Madness transcribes in real time and tallies every buzzword mention. Brackets score as it happens, the leaderboard reshuffles, and a champion fanfare fires when the dust settles.
- **Or score a transcript.** No mic? Paste a transcript (or point at a meeting's `.jsonl`) in **Transcript Mode** and score the bracket after the fact.

Everything runs locally with [whisper.cpp](https://github.com/ggerganov/whisper.cpp). Your meeting audio never leaves the machine.

---

## Install & run

### macOS (Apple Silicon)

1. Download `Minutes-Madness-macOS-arm64-<version>.zip` and unzip it.
2. The build is ad-hoc signed, so the first launch needs **right-click → Open** (then confirm) to clear Gatekeeper. Drag it to `/Applications` if you like.
3. Grant **Microphone** when prompted. To score a *remote* meeting (Zoom/Meet on headphones), also grant **Audio Recording** for system-audio capture — Minutes Madness uses the native macOS tap, so no extra software (BlackHole etc.) is needed.

### Windows (10/11, x64)

Download and run one of:
- `minutes-desktop-windows-x64-vulkan-setup.exe` — GPU-accelerated (Vulkan; works on NVIDIA / AMD / Intel) with automatic CPU fallback. Recommended.
- `minutes-desktop-windows-x64-setup.exe` — CPU-only.

The installer is unsigned, so SmartScreen warns on first run (**More info → Run anyway**).

> First launch downloads a small local speech model (one time). On Windows, Minutes Madness scores from the **microphone**; native system-audio capture is macOS-only today (on Windows, route system audio through a loopback input if you need it).

---

## How to play

1. **Open Minutes Madness** and create a bracket (or keep the default buzzwords).
2. **Collect picks.** Use **Players → Invite** to share a code, or send people the picks page. Each player ranks the bracket; paste their code back via **Import picks**.
3. **Pick your audio source** with the **🎙 / 🔊 Audio** button: *Microphone* for an in-person all-hands, *System audio* for a remote one. Watch the input meter to confirm it's hearing sound before you start.
4. **Start recording** when the meeting begins. Buzzwords light up and score in real time; click a player to overlay their bracket on yours.
5. **No recording?** Use **Transcript Mode** to paste or load a transcript and score instantly.

---

## Privacy

- 100% local transcription (whisper.cpp). No cloud calls, no API keys, no telemetry.
- Audio is captured only while you are recording, and only to produce the transcript that scores the bracket.

---

## Build from source

Requires [Rust](https://rustup.rs) (via rustup), `cmake`, and `ffmpeg`. macOS builds whisper.cpp from source, so you also need the Xcode Command Line Tools.

```bash
git clone https://github.com/mquinn614/minutes.git
cd minutes
git checkout experiments

# macOS (Apple Silicon, Metal GPU):
# Pin SDKROOT to an installed SDK older than macOS 26 — the macOS 26 SDK
# breaks the whisper.cpp C++ build. CXXFLAGS points libc++ at the default SDK.
export SDKROOT="$(xcrun --sdk macosx15.5 --show-sdk-path)"
export CXXFLAGS="-I$(xcrun --show-sdk-path)/usr/include/c++/v1"

./scripts/build.sh                  # → target/release/Minutes-Madness-macOS-arm64-<version>.zip
# …or, for a stable-TCC dev identity while iterating:
./scripts/install-dev-app.sh        # → ~/Applications/Minutes Madness Dev.app
```

GPU acceleration maps straight through to whisper.cpp via cargo features: `metal` (macOS), `vulkan` (Windows/Linux, any GPU), `cuda` (NVIDIA). The macOS desktop build defaults to `parakeet,metal`.

Windows builds run on CI: the **Windows Vulkan Experiment** workflow builds `--features parakeet,vulkan`, and **Release Windows Desktop** builds the CPU installer.

---

## Status

Minutes Madness is an experimental fork of [Minutes](https://github.com/silverstein/minutes), living on the `experiments` branch. It pares Minutes down to exactly what the game needs — local recording (microphone + system audio), real-time transcription, and the bracket UI — and drops the rest of the meeting-memory product surface (dictation, command palette, calendar, the AI assistant, MCP, vault sync, and so on).

Licensed under the MIT License, the same as upstream Minutes — see [LICENSE](LICENSE).

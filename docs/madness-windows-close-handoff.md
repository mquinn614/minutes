# Windows shutdown hang — Codex handoff

Context: All-Hands ~6 days out. The Madness add-on can't ship while
tray Quit on Windows produces a stuck process. Three iterations of
community-converged workarounds for the Tauri-on-Windows shutdown
deadlock have changed the symptom but never fixed it. Asking for a
fresh pair of eyes.

## Repository state

- Fork: `mquinn614/minutes`, branch: `experiments`
- Latest commit: `248f9ad` — uses `kernel32::TerminateProcess` on
  Windows for the terminal exit
- All work on top of upstream `silverstein/minutes` v0.18.3 (merged
  in `8bee434`)
- macOS path is verified working; never been the suspect
- Mac dev app + Windows installer artifact build cleanly via
  `gh workflow run "Release Windows Desktop"`

## The bug, exactly

**Test:** Install on Windows 11. Launch from Start Menu. Wait for the
main window to appear. Right-click the Minutes icon in the system
tray (notification area, bottom-right) → Quit Minutes.

**Expected:** App process terminates within ~2 seconds.

**Actual on latest build (`248f9ad`, with TerminateProcess):** Tested
5×, same failure every time. Window hides successfully on Quit click,
but the process keeps running. Tray icon stays visible, right-click
menu remains responsive, "Open Minutes" reopens the main window — but
clicking on the reopened window causes it to grey out with the
hourglass cursor (full Not Responding hang).

**Critically:** the same failure reproduces on **stock upstream
v0.18.0, v0.18.2, v0.18.3** binaries (pulled from upstream Releases —
no Madness, no our code). The user could close the app fine on
v0.18.2 for several days before yesterday; the inflection point was
yesterday's session of force-killing the app via Task Manager. After
that, every version started exhibiting the hang.

WebView2 Runtime version: 148.0.3967.83 (installed 5/23). User-data
folder at `%LOCALAPPDATA%\com.useminutes.desktop\EBWebView` was
fully deleted between tests — does NOT change behavior.

## Symptom progression across iterations

Each fix made the symptom less bad without ever curing it. The
progression itself is data — something downstream of each layer is
still wrong.

1. **Baseline (any v0.18.x stock):** click [×] on main window → window
   hides but process keeps running (intentional behavior). Click tray
   Quit → full white-grey "Not Responding" hang, must Task-Manager-kill.
2. **`60a313c` removed liveTick auto-end branch:** unrelated to close,
   fixes false fanfare. Madness panel only.
3. **`5bfc98d` widened audio channel 64 → 512:** unrelated to close.
4. **`a345be3` close-window-exits via request_clean_exit (Win/Lin):**
   click [×] on main now causes hang too (same Not Responding hang).
   **Reverted in `4aca998`.**
5. **`7d6af92` defer terminal shutdown to worker thread:** alone didn't
   fix tray Quit hang.
6. **`8502611` predrain webview windows + 200 ms pump sleep:** symptom
   changed. Now window hides on Quit, app stays responsive (right-click
   tray, Open Minutes all still work), but Quit becomes a no-op. Process
   never terminates. (User confirmed this is NOT a hang — event loop is
   alive.)
7. **`4158997` switch from `std::process::exit` to `libc::_exit` on
   Windows:** symptom shifted to the current one. Show Minutes opens
   the main window, but clicking the window hangs it.
8. **`248f9ad` `kernel32::TerminateProcess(GetCurrentProcess(), 0)` on
   Windows:** same behavior as `4158997`. **No change.**

The fact that TerminateProcess didn't kill the process is the
genuinely surprising data point. That's the syscall Task Manager
itself uses. If it's not terminating, either:
- It's not being reached (worker thread blocked before it)
- It's being reached but Windows is doing something prior to actual
  termination that hangs (unlikely — TerminateProcess is documented
  as immediate and irreversible)
- The fix isn't actually compiled in (build/dispatch sanity check?)

## Code surface to look at

- `tauri/src-tauri/src/main.rs:67` — `request_clean_exit` (entry
  point, gated by `CLEAN_EXIT_STARTED` atomic)
- `tauri/src-tauri/src/main.rs:50` — `exit_process_without_destructors`
  (the terminal exit call; now wraps TerminateProcess on Windows)
- `tauri/src-tauri/src/main.rs:41` — `cleanup_before_process_exit`
  (kills PTY sessions, shuts down parakeet sidecar)
- `tauri/src-tauri/src/main.rs:1854` + `2146` — tray menu Quit item
  registration + event handler (calls `request_clean_exit(app, 0)`)
- `tauri/src-tauri/src/commands.rs:3236` — `recording_active` (what
  request_clean_exit branches on)

## What's been ruled out

- **Our Madness work** — reproduces on stock upstream v0.18.0
- **WebView2 user data corruption** — deleting the EBWebView folder
  doesn't change behavior
- **Stale `~/.minutes/` state** — PID files are absent; `jobs/` is
  clean (just an `archive` folder)
- **WebView2 runtime regression** — same 148.0.3967.83 was present
  while v0.18.2 was working for the user; not an auto-update window
- **Atexit chain** (per `libc::_exit` test)
- **C library termination** (per TerminateProcess test, which should
  have skipped this)

## What I have NOT done

- Added any diagnostic tracing inside the shutdown path. This is a
  gap. We've been inferring where the worker thread is by external
  symptom, never by direct evidence. Adding `tracing::info!` calls at
  each step in `request_clean_exit` and `finish_clean_exit` would tell
  us in 30 seconds whether TerminateProcess is being reached or not.
  Logs land at `~/.minutes/logs/minutes.log` on Windows
  (`%USERPROFILE%\.minutes\logs\minutes.log`).
- Tested whether the bug repros in a freshly-built `cargo tauri dev`
  session (no installer wrapping), which would isolate any NSIS/CI
  packaging weirdness.
- Looked at whether something in our additions to `commands.rs` (the
  Madness command set, `cmd_probe_live_config`, etc.) is spawning a
  background thread on app startup that's somehow blocking the
  shutdown path even when the user never touches Madness.
- Compared our `Cargo.lock` against Mat's `silverstein/minutes` v0.18.3
  lockfile for any drift in Tauri / tao / wry / WebView2 binding
  versions that could matter.

## Relevant upstream issues found earlier

- `tauri-apps/tauri#14088` — open, `status: upstream`. Closest match.
- `tauri-apps/tauri#5611` + discussion `#4662` — `app.exit()` doesn't
  drive event-loop shutdown.
- `MicrosoftEdge/WebView2Feedback#318` — canonical WebView2 shutdown
  deadlock (closed, no public 148-era fix).
- `tauri-apps/tauri#7358` — Drop not called on Windows shutdown.

## What I'd try next if I were Codex

1. **Add the tracing first.** Adding `tracing::info!` lines through
   `request_clean_exit`, `finish_clean_exit`, and
   `cleanup_before_process_exit`, then having the user run one test
   and paste `~/.minutes/logs/minutes.log`, tells us in one
   round-trip what's blocking. We've been speculating; let's measure.
2. **Audit background threads spawned during startup.** Specifically
   anything we added in `commands.rs` (the device monitor, the
   probe-cache reader, anything that touches the live-transcript PID
   path on startup). A background thread holding a non-`Send` resource
   could block `TerminateProcess`'s parent-thread step.
3. **Check whether Mat actually closes cleanly.** The user said
   v0.18.2 worked for "several days" before yesterday. We assumed
   that's true at face value; could be that Mat's CI builds produce
   bit-different output from a local `cargo tauri build` (e.g.,
   different MSVC flags), and the local-on-Mat build is the only
   working artifact.

Sorry for the runaround. Logs would have closed this in one cycle.

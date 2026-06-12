# PC session handoff: upstream PR for issue #312 (Win11 transparent menu bar)

**Mission:** reproduce, fix, and validate
[silverstein/minutes#312](https://github.com/silverstein/minutes/issues/312)
on this Windows 11 machine, then submit the fix as a PR to upstream. Mat
explicitly invited this PR ("I'd happily review a PR") and hypothesized the
menu strip "is inheriting the window's transparency rather than painting a
background." **He is right** — see Root cause. This is a gift PR from the fork;
keep it minimal and surgical.

## Branch hygiene (read first)

- The PR branch MUST be cut from `upstream/main`, never from `experiments`
  (experiments is the heavily-diverged Minutes Madness fork).
- This handoff doc lives on `experiments` only. It must NOT appear in the PR.

```bash
git fetch upstream
git checkout -b fix/312-win11-menubar-transparency upstream/main
```

(If the `upstream` remote is missing on this machine:
`git remote add upstream https://github.com/silverstein/minutes.git`)

## Root cause (verified from source @ upstream 37669b9)

`tauri/src-tauri/src/main.rs` `show_main_window()` (~L252):

```rust
let mut builder = WebviewWindowBuilder::new(app, "main", WebviewUrl::App("index.html".into()))
    .title("")
    .inner_size(560.0, 700.0)
    .min_inner_size(460.0, 520.0)
    .transparent(true)            // <-- the bug on Windows
    ...

#[cfg(target_os = "macos")]
{
    builder = builder.title_bar_style(...)...   // overlay chrome
}

if let Ok(win) = builder.build() {
    #[cfg(target_os = "macos")]
    {
        use window_vibrancy::{apply_vibrancy, NSVisualEffectMaterial};
        apply_vibrancy(&win, NSVisualEffectMaterial::Sidebar, None, None).ok();
    }
    ...
}
```

The transparency exists solely to serve **macOS vibrancy** (the
`apply_vibrancy` call is cfg-gated to macOS; `index.html` sets
`background: transparent` on html/body so the material shows through). On
Windows there is no vibrancy, so `.transparent(true)` buys nothing — and the
native Win32 menu bar (the app menu from `build_app_menu`, ~L715, attached
app-wide at ~L1314) renders its strip with no background. White menu text over
alpha-0 = invisible over light content. The dictation overlay and palette
windows are also transparent but have `decorations(false)` and no menu bar —
they are fine and out of scope.

## Repro plan

**Phase 0 — setup**
1. `git pull` the fork on this machine, fetch upstream, cut the branch (above).
2. Build env: same as the May perf-debug sessions (`cargo tauri build` worked
   here before; `cargo tauri dev` also works — frontendDist is plain HTML, no
   node dev server).

**Phase 1 — reproduce on unmodified upstream/main**
1. Windows 11, system **dark** theme. Launch the app, open the main window.
2. Confirm: File/Edit/Window/Help strip is transparent (drag over Notepad or a
   browser showing a white page → labels invisible). Screenshot = PR "before".
3. Data point: flip to **light** theme, relaunch. Is the strip still
   transparent? (Expected yes — it's alpha, not theming. Record either way;
   it distinguishes this bug from muda#97's wrong-theme issue.)

**Phase 2 — apply fix #1 and verify**
1. Move `.transparent(true)` out of the shared chain and into the existing
   `#[cfg(target_os = "macos")]` builder block:
   ```rust
   // shared chain: no .transparent(true)
   #[cfg(target_os = "macos")]
   {
       builder = builder
           .transparent(true)   // vibrancy (below) needs an alpha window
           .title_bar_style(tauri::TitleBarStyle::Overlay)
           ...
   }
   ```
2. Rebuild, relaunch. Expect: menu strip now paints (likely the classic
   light Win32 menubar with dark text — readable; perfect theming is
   muda#97's gap and explicitly out of scope here).
3. **Watch for the one plausible regression:** with an opaque window, the
   webview's own background shows wherever the page CSS is transparent
   (html/body are). If the app background looks wrong (white flash at launch,
   white behind the noise texture), apply the contingency below. If the app
   looks identical to before except the menubar now paints → done.

**Contingency (only if Phase 2.3 regresses):** keep the window opaque and give
the page an explicit base coat on Windows — either set the window background
color at build time (Tauri 2 `WebviewWindowBuilder::background_color`, if
present in the locked tauri version) or have index.html paint
`background: var(--bg)` on `html` when `navigator.userAgent` says Windows
(macOS must keep `transparent` for vibrancy). Prefer the Rust-side option;
keep whichever addition is smallest.

**Fallback fixes (only if fix #1 is unworkable), in order:**
2. Paint the menubar explicitly: `SetMenuInfo(GetMenu(hwnd), MIM_BACKGROUND |
   MIM_APPLYTOSUBMENUS, dark brush)` via the `windows` crate. Keeps the
   transparent window. Costs a new dependency in the tauri app (it has none
   today) and ~30 unsafe lines — noticeably less attractive as a drive-by PR.
3. `#[cfg(not(target_os = "macos"))] win.remove_menu()` (what the fork's
   Madness window does, commit 044cd7b). Sidesteps rendering entirely but
   removes real upstream UX (File→open/note, Help links) — Mat's call, not
   ours; propose in the PR discussion rather than shipping unilaterally.

## Validation checklist (all on this PC unless noted)

- [ ] Menubar readable in dark theme: over a white window, over a dark window,
      after move/resize/maximize/restore
- [ ] Menubar readable in light theme
- [ ] Runtime theme flip while the app is open (Settings → Personalization):
      no repaint artifacts, still readable
- [ ] Every top-level menu opens; spot-check items fire (Help → website opens
      browser; File items behave)
- [ ] App visuals unchanged: dark background + noise texture intact, no white
      flash at launch, window shadow/corners look normal
- [ ] Smoke test: record a short mic clip end-to-end (unrelated to the fix,
      cheap insurance)
- [ ] `cargo fmt --all -- --check` and
      `cargo clippy --all --no-default-features -- -D warnings` pass
- [ ] **macOS regression check (on the Mac, same branch):** vibrancy still
      applies (translucent sidebar material), overlay title bar + traffic
      lights unchanged. The flag only moved *into* the macOS-only block, so
      this should be a visual no-op — verify anyway before submitting.
- [ ] "After" screenshots captured (same two compositions as the issue:
      over wallpaper, over a white window)

## PR submission

- Commit style (his conventions):
  `fix(app): gate main-window transparency to macOS; Win32 menu bar was compositing transparent (#312)`
- Push the branch to the fork, PR against upstream:
  ```bash
  git push origin fix/312-win11-menubar-transparency
  gh pr create -R silverstein/minutes \
    --head mquinn614:fix/312-win11-menubar-transparency \
    --title "fix(app): gate main-window transparency to macOS — Win32 menu bar was compositing transparent" \
    --body-file <draft below>
  ```
- PR body skeleton:
  - **Fixes #312.**
  - Diagnosis: `.transparent(true)` on the main window exists for macOS
    vibrancy only; on Windows it has no consumer and the native menu strip
    inherits the alpha-0 background (exactly the "inheriting the window's
    transparency" you suspected).
  - The change: one flag moved into the existing macOS-only builder block.
    macOS behavior identical; Windows window becomes opaque (its UI already
    paints its own background). Likely fixes the same strip on Linux
    (untested — note it).
  - Before/after screenshots.
  - Validation: summarize the checklist results + "menubar now paints the
    stock Win32 strip; full dark theming of the menubar is upstream
    muda#97 and out of scope."
- Do NOT include this handoff doc, any Madness code, or unrelated fork changes
  in the PR. The diff should be a handful of lines in `main.rs` (plus the
  contingency CSS/builder line if needed).

#!/bin/bash
# Build everything: CLI, Tauri app, and optional production-style install (macOS only)
set -euo pipefail

if [[ "$(uname -s)" != "Darwin" ]]; then
    echo "Error: build.sh is macOS-only (requires xcrun, swiftc, codesign)."
    echo "For cross-platform CLI builds: cargo build --release -p minutes-cli"
    exit 1
fi

export CXXFLAGS="-I$(xcrun --show-sdk-path)/usr/include/c++/v1"
export MACOSX_DEPLOYMENT_TARGET="${MACOSX_DEPLOYMENT_TARGET:-11.0}"
MINUTES_BUILD_FEATURES="${MINUTES_BUILD_FEATURES:-parakeet,metal}"

# Ensure cargo runs through rustup so rust-toolchain.toml is honored.
# Without this, a system Homebrew rustc (e.g. /opt/homebrew/bin/cargo)
# silently ignores the pin and produces local-vs-CI drift on clippy lints
# that fire only on the pinned version. PR #206's CI failure was exactly
# this: Rust 1.95 lint fired on CI, Homebrew's 1.94 ignored it locally.
#
# Detection uses `rustup which cargo` rather than a hardcoded path so
# CARGO_HOME / non-default rustup install locations work too.
RUSTUP_CARGO=""
if command -v rustup >/dev/null 2>&1; then
    RUSTUP_CARGO="$(rustup which cargo 2>/dev/null || true)"
fi
if [[ -n "$RUSTUP_CARGO" ]]; then
    RUSTUP_CARGO_DIR="$(dirname "$RUSTUP_CARGO")"
    export PATH="$RUSTUP_CARGO_DIR:$PATH"
fi
ACTIVE_CARGO="$(command -v cargo || true)"
if [[ -z "$ACTIVE_CARGO" ]]; then
    echo "Error: no cargo on PATH. Install rustup from https://rustup.rs and re-run."
    exit 1
fi
if [[ -n "$RUSTUP_CARGO" && "$ACTIVE_CARGO" != "$RUSTUP_CARGO" ]]; then
    echo "Warning: cargo at $ACTIVE_CARGO is not the rustup-managed cargo ($RUSTUP_CARGO)."
    echo "         rust-toolchain.toml may be silently ignored, causing local-vs-CI clippy drift."
    echo "         Fix: prepend rustup's bin dir to PATH in your shell, or 'brew uninstall rust' if installed via Homebrew."
fi
if [[ -z "$RUSTUP_CARGO" ]]; then
    echo "Note: rustup not found; using cargo at $ACTIVE_CARGO directly."
    echo "      rust-toolchain.toml requires rustup to be honored — install from https://rustup.rs for full reproducibility."
fi

# Code signing + notarization are optional for local source builds.
# Maintainers can export APPLE_SIGNING_IDENTITY / APPLE_API_* when they want
# cargo-tauri to produce a signed + notarized bundle.

echo "=== Building CLI (release) ==="
_build_tmp=$(mktemp)
if ! cargo build --release -p minutes-cli --features "$MINUTES_BUILD_FEATURES" 2>&1 | tee "$_build_tmp"; then
    if grep -q "library 'clang_rt\." "$_build_tmp"; then
        echo ""
        echo "  Stale ort-sys clang runtime path (Xcode/CLT upgrade detected)."
        echo "  Cleaning stale build cache and retrying..."
        rm -rf target/*/build/ort-sys-*
        cargo build --release -p minutes-cli --features "$MINUTES_BUILD_FEATURES"
    else
        rm -f "$_build_tmp"
        exit 1
    fi
fi
rm -f "$_build_tmp"

echo "=== Staging CLI as Tauri sidecar ==="
# v1: aarch64-only sidecars. x86_64 cross-compile is a v2 follow-up. The Tauri
# sidecar convention requires an arch-suffixed filename.
HOST_TARGET="$(rustc -Vv | awk '/host:/ {print $2}')"
mkdir -p tauri/src-tauri/bin
cp -f target/release/minutes "tauri/src-tauri/bin/minutes-${HOST_TARGET}"

echo "=== Building Tauri app ==="
# Production identity comes from tauri.conf.json: productName "Minutes Madness",
# bundle id com.useminutes.madness. (The dev variant — "Minutes Madness Dev" /
# com.useminutes.madness.dev — is built by scripts/install-dev-app.sh.)
# The calendar-events Swift helper is compiled and staged into
# tauri/src-tauri/resources/ by tauri/src-tauri/build.rs, and Tauri bundles it
# into the .app/Contents/Resources/ automatically via tauri.conf.json.
APP_NAME="Minutes Madness"
APP_BUNDLE="target/release/bundle/macos/${APP_NAME}.app"
TAURI_BUILD_ARGS=(cargo tauri build --features "$MINUTES_BUILD_FEATURES" --bundles app)
if [[ -z "${TAURI_SIGNING_PRIVATE_KEY:-}" ]]; then
    echo "  No TAURI_SIGNING_PRIVATE_KEY configured; building updater artifacts with --no-sign."
    TAURI_BUILD_ARGS+=(--no-sign)
fi
"${TAURI_BUILD_ARGS[@]}"

# Sign the .app. With a real Apple identity (APPLE_SIGNING_IDENTITY /
# MINUTES_DEV_SIGNING_IDENTITY) use hardened runtime + entitlements; otherwise
# fall back to ad-hoc so the bundle still launches for local/field testing.
# Mirrors scripts/install-dev-app.sh so both paths sign identically.
SIGN_ID="${APPLE_SIGNING_IDENTITY:-${MINUTES_DEV_SIGNING_IDENTITY:--}}"
echo "=== Signing ${APP_NAME}.app (identity: ${SIGN_ID}) ==="
if [[ "$SIGN_ID" == "-" ]]; then
    codesign --force --deep --sign - "$APP_BUNDLE"
else
    codesign --force --deep --options runtime --timestamp \
        --entitlements tauri/src-tauri/entitlements.plist \
        --sign "$SIGN_ID" \
        "$APP_BUNDLE"
fi

echo "=== Re-signing bundled CLI sidecar with its own entitlements ==="
# The CLI sidecar needs `com.apple.security.device.audio-input` so `minutes record`
# from a terminal hits the macOS TCC mic prompt instead of silently failing. The
# outer `--deep` sign above clobbers nested entitlements, so re-sign the sidecar
# afterwards. Tauri's bundler strips the target-triple from externalBin names, so
# the on-disk filename is plain `minutes`. Ad-hoc-signed sidecars won't get
# entitlements honored without a Team ID — that's expected for OSS/field builds.
SIDECAR="${APP_BUNDLE}/Contents/MacOS/minutes"
if [[ -f "$SIDECAR" ]]; then
    if [[ "$SIGN_ID" == "-" ]]; then
        codesign --force --options runtime \
            --entitlements tauri/src-tauri/minutes-cli.entitlements \
            --sign - "$SIDECAR"
    else
        codesign --force --options runtime --timestamp \
            --entitlements tauri/src-tauri/minutes-cli.entitlements \
            --sign "$SIGN_ID" "$SIDECAR"
    fi
    echo "  Signed sidecar with identity: $SIGN_ID"
else
    echo "  WARNING: expected sidecar not found at $SIDECAR — skipping re-sign."
fi

# CRITICAL: re-seal the OUTER bundle now that the sidecar is in its final state.
# The `--deep` sign above sealed the bundle over the sidecar's *pre-re-sign*
# hash; re-signing the sidecar afterward invalidated that seal, so a quarantined
# (downloaded) copy fails Gatekeeper with "nested code is modified or invalid"
# → the user sees "'Minutes Madness' is damaged and can't be opened." Re-signing
# the outer bundle WITHOUT --deep re-records the final nested hashes (and leaves
# the sidecar's own entitlements signature intact), so the seal validates.
echo "=== Re-sealing ${APP_NAME}.app outer bundle (over the final sidecar) ==="
if [[ "$SIGN_ID" == "-" ]]; then
    codesign --force --sign - "$APP_BUNDLE"
else
    codesign --force --options runtime --timestamp \
        --entitlements tauri/src-tauri/entitlements.plist \
        --sign "$SIGN_ID" "$APP_BUNDLE"
fi
codesign --verify --deep --strict "$APP_BUNDLE" \
    && echo "  Signature valid (codesign --verify passed)" \
    || echo "  WARNING: codesign --verify FAILED — the bundle seal is broken."

APP_VERSION="$(python3 - <<'PY'
import json
from pathlib import Path
print(json.loads(Path("tauri/src-tauri/tauri.conf.json").read_text())["version"])
PY
)"

# Package a shareable zip (ditto preserves macOS metadata + the codesignature).
APP_ZIP="target/release/${APP_NAME// /-}-macOS-arm64-${APP_VERSION}.zip"
echo "=== Packaging ${APP_ZIP} ==="
rm -f "$APP_ZIP"
ditto -c -k --keepParent "$APP_BUNDLE" "$APP_ZIP"

# Optional branded DMG (opt-in: ./scripts/build.sh --dmg). NOTE:
# scripts/create-branded-dmg.sh still hardcodes the "Minutes.app" / "Minutes"
# volume branding and needs a fork-branding pass before it emits a correct
# "Minutes Madness" DMG; the zip above is the primary distributable.
if [[ " $* " == *" --dmg "* ]]; then
    ./scripts/create-branded-dmg.sh \
        --app "$APP_BUNDLE" \
        --version "$APP_VERSION" \
        --output "target/release/bundle/dmg/${APP_NAME// /-}_${APP_VERSION}_aarch64.dmg" \
        || echo "  DMG step failed (non-fatal); the zip is the primary artifact."
fi

echo "=== Signing + Installing CLI ==="
mkdir -p ~/.local/bin
codesign -s - -f target/release/minutes 2>/dev/null || true
cp -f target/release/minutes ~/.local/bin/minutes && echo "  Installed to ~/.local/bin/"

echo ""

# Install to /Applications if --install flag is passed
if [[ " $* " == *" --install "* ]]; then
    echo "=== Installing app to /Applications ==="
    rm -rf "/Applications/${APP_NAME}.app"
    cp -rf "$APP_BUNDLE" /Applications/
    echo "  Installed to /Applications/${APP_NAME}.app"
fi

echo "=== Done ==="
echo "  Build features: $MINUTES_BUILD_FEATURES"
RESOLVED="$(which minutes 2>/dev/null || true)"
if [ -n "$RESOLVED" ]; then
    echo "  CLI:  $RESOLVED — $("$RESOLVED" --version 2>&1)"
else
    echo "  CLI:  ~/.local/bin/minutes (not in PATH) — $(~/.local/bin/minutes --version 2>&1 || echo 'unknown')"
fi
if [ -n "$RESOLVED" ]; then
    RESOLVED_REAL="$(readlink -f "$RESOLVED" 2>/dev/null || echo "$RESOLVED")"
    EXPECTED_REAL="$(readlink -f "$HOME/.local/bin/minutes" 2>/dev/null || echo "$HOME/.local/bin/minutes")"
fi
if [ -n "$RESOLVED" ] && [ "$RESOLVED_REAL" != "$EXPECTED_REAL" ]; then
    echo ""
    echo "  ⚠  PATH shadowing: 'minutes' resolves to $RESOLVED"
    echo "     The build installed to ~/.local/bin/minutes but a stale binary takes priority."
    if [[ "$RESOLVED" == */homebrew/* ]] || [[ "$RESOLVED" == */Cellar/* ]]; then
        echo "     Fix: brew unlink minutes"
    elif [[ "$RESOLVED" == */.cargo/bin/* ]]; then
        echo "     Fix: cargo uninstall minutes"
    else
        echo "     Fix: rm '$RESOLVED'"
    fi
fi
echo "  App:  $APP_BUNDLE"
echo "  Zip:  $APP_ZIP"
echo ""
if [ -d "/Applications/${APP_NAME}.app" ]; then
    echo "  Relaunch: open \"/Applications/${APP_NAME}.app\""
else
    echo "  Launch: open \"$APP_BUNDLE\""
    echo "  Install: ./scripts/build.sh --install"
fi
echo "  Dev app (stable TCC identity): ./scripts/install-dev-app.sh"

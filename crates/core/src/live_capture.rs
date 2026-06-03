//! Unified live audio capture source for the Minutes Madness live path.
//!
//! The live-transcript / Madness pipeline historically captured from a single
//! cpal microphone device (`AudioStream`). Minutes Madness also needs to score
//! buzzwords from a **remote** all-hands where the meeting audio plays through
//! the host's speakers/headphones — i.e. *system audio*. On macOS we capture
//! that natively via the CoreAudio process tap (`CoreAudioTapSystemAudioBackend`),
//! with no third-party loopback driver (BlackHole etc.) required.
//!
//! Both sources produce the exact same `AudioChunk` stream (16kHz mono f32 with
//! per-chunk RMS), so [`LiveCapture`] presents one `Receiver<AudioChunk>`
//! regardless of which source is active. [`LevelMeter`] builds on that to drive
//! a pre-record input meter so the host can confirm the selected source is
//! actually picking up sound before the game starts.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use crossbeam_channel::Receiver;

use crate::error::CaptureError;
use crate::streaming::{AudioChunk, AudioStream};

/// Which input the live path should capture from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LiveAudioSource {
    /// Microphone capture via a cpal input device. `None` uses the system
    /// default input device; `Some(name)` pins a specific device.
    Microphone(Option<String>),
    /// System audio (whatever is playing out of the computer) captured natively
    /// via the macOS CoreAudio process tap. No loopback driver required.
    SystemAudio,
}

impl LiveAudioSource {
    /// Parse a frontend/source string (`"mic"`, `"microphone"`, `"system"`,
    /// `"system-audio"`) into a [`LiveAudioSource`].
    ///
    /// `device` is only consulted for the microphone variant. Unknown strings
    /// fall back to the microphone (the always-available default).
    pub fn parse(source: &str, device: Option<String>) -> Self {
        match source.trim().to_ascii_lowercase().as_str() {
            "system" | "system-audio" | "system_audio" | "tap" | "call" => {
                LiveAudioSource::SystemAudio
            }
            _ => LiveAudioSource::Microphone(device),
        }
    }

    /// Stable identifier for this source (`"microphone"` / `"system-audio"`),
    /// useful for logging and round-tripping the selection to the frontend.
    pub fn as_str(&self) -> &'static str {
        match self {
            LiveAudioSource::Microphone(_) => "microphone",
            LiveAudioSource::SystemAudio => "system-audio",
        }
    }
}

/// A live capture stream abstracted over microphone vs. system-audio sources.
///
/// Holds the underlying backend alive for its lifetime; dropping it stops
/// capture. Chunks are read from [`LiveCapture::receiver`].
pub struct LiveCapture {
    receiver: Receiver<AudioChunk>,
    device_label: String,
    backend: CaptureBackend,
}

/// Backing capture implementation, kept alive for the life of the [`LiveCapture`].
enum CaptureBackend {
    /// cpal microphone (or loopback) input device.
    Mic(AudioStream),
    /// macOS CoreAudio process tap handle.
    #[cfg(target_os = "macos")]
    System(crate::system_audio_backend::StreamHandle),
}

impl LiveCapture {
    /// Start capturing from `source`.
    ///
    /// For [`LiveAudioSource::Microphone`] this opens the named (or default)
    /// cpal device. For [`LiveAudioSource::SystemAudio`] this starts the native
    /// macOS CoreAudio tap on the default system output. Returns a
    /// [`CaptureError`] if the device/tap cannot be opened (e.g. missing
    /// permission, or system audio requested on a non-macOS platform).
    pub fn start(source: &LiveAudioSource) -> Result<Self, CaptureError> {
        match source {
            LiveAudioSource::Microphone(device) => {
                let stream = AudioStream::start(device.as_deref())?;
                let receiver = stream.receiver.clone();
                let device_label = stream.device_name.clone();
                Ok(Self {
                    receiver,
                    device_label,
                    backend: CaptureBackend::Mic(stream),
                })
            }
            LiveAudioSource::SystemAudio => Self::start_system(),
        }
    }

    /// Start the native macOS system-audio tap.
    #[cfg(target_os = "macos")]
    fn start_system() -> Result<Self, CaptureError> {
        use crate::system_audio_backend::{CoreAudioTapSystemAudioBackend, SystemAudioBackend};

        // 512 chunks ≈ 51s of headroom, matching AudioStream's buffer so the
        // meter / live consumer never silently drops system-audio chunks.
        let (tx, rx) = crossbeam_channel::bounded(512);
        let mut backend = CoreAudioTapSystemAudioBackend::new();
        let handle = backend.start(tx)?;
        let device_label = handle
            .route()
            .device_name
            .unwrap_or_else(|| "System audio".to_string());
        Ok(Self {
            receiver: rx,
            device_label,
            backend: CaptureBackend::System(handle),
        })
    }

    /// System audio is macOS-only; other platforms have no native tap.
    #[cfg(not(target_os = "macos"))]
    fn start_system() -> Result<Self, CaptureError> {
        Err(CaptureError::Io(std::io::Error::other(
            "System audio capture is only available on macOS",
        )))
    }

    /// Receiver delivering 16kHz mono f32 chunks from the active source.
    pub fn receiver(&self) -> &Receiver<AudioChunk> {
        &self.receiver
    }

    /// Human-readable label for the active device/route (for UI + logs).
    pub fn device_label(&self) -> &str {
        &self.device_label
    }

    /// True if the underlying capture backend has reported an error.
    pub fn has_error(&self) -> bool {
        match &self.backend {
            CaptureBackend::Mic(stream) => stream.has_error(),
            #[cfg(target_os = "macos")]
            CaptureBackend::System(handle) => handle.has_error(),
        }
    }
}

/// Normalize a chunk RMS (0.0–1.0 scale) to a 0–100 UI level.
///
/// Matches `AudioStream`'s in-callback scaling so the pre-record meter and the
/// during-recording level read identically.
pub fn rms_to_level(rms: f32) -> u32 {
    (rms * 2000.0).min(100.0).max(0.0) as u32
}

/// A pre-record input level meter over a [`LiveCapture`] source.
///
/// The capture backend (which owns a `!Send` cpal stream on macOS) lives
/// entirely on a dedicated meter thread; this handle holds only `Send` control
/// state (atomics + the join handle), so it can be parked in the Tauri
/// `AppState` and polled from command handlers. The meter publishes the current
/// input level (0–100) so the Madness host can confirm the selected source is
/// picking up sound before the game's recording starts. Dropping the meter
/// stops capture and joins the thread.
pub struct LevelMeter {
    level: Arc<AtomicU32>,
    stop: Arc<AtomicBool>,
    error: Arc<AtomicBool>,
    device_label: String,
    thread: Option<JoinHandle<()>>,
}

impl LevelMeter {
    /// Start metering `source`. Blocks only until the capture backend has been
    /// opened on the meter thread (so device/permission failures surface
    /// synchronously as `Err`), then returns a running meter; poll
    /// [`LevelMeter::level`] for the current input level.
    pub fn start(source: &LiveAudioSource) -> Result<Self, CaptureError> {
        let level = Arc::new(AtomicU32::new(0));
        let stop = Arc::new(AtomicBool::new(false));
        let error = Arc::new(AtomicBool::new(false));

        // The cpal stream is `!Send`, so the capture MUST be created on (and
        // never leave) the meter thread. A one-shot channel reports the open
        // result back so the caller gets synchronous error propagation.
        let (ready_tx, ready_rx) = crossbeam_channel::bounded::<Result<String, CaptureError>>(1);
        let source = source.clone();
        let level_thread = Arc::clone(&level);
        let stop_thread = Arc::clone(&stop);
        let error_thread = Arc::clone(&error);

        let thread = std::thread::spawn(move || {
            let capture = match LiveCapture::start(&source) {
                Ok(capture) => {
                    let _ = ready_tx.send(Ok(capture.device_label().to_string()));
                    capture
                }
                Err(err) => {
                    let _ = ready_tx.send(Err(err));
                    return;
                }
            };

            while !stop_thread.load(Ordering::Relaxed) {
                match capture.receiver().recv_timeout(Duration::from_millis(200)) {
                    Ok(chunk) => {
                        level_thread.store(rms_to_level(chunk.rms), Ordering::Relaxed);
                    }
                    Err(crossbeam_channel::RecvTimeoutError::Timeout) => {
                        // Stream stalled (no chunk in 200ms, though even silence
                        // produces ~10 chunks/sec). Relax the meter to zero so it
                        // doesn't stick at the last value.
                        level_thread.store(0, Ordering::Relaxed);
                    }
                    Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
                }
                if capture.has_error() {
                    error_thread.store(true, Ordering::Relaxed);
                    break;
                }
            }
            // `capture` drops here on the meter thread, stopping the stream.
        });

        match ready_rx.recv() {
            Ok(Ok(device_label)) => Ok(Self {
                level,
                stop,
                error,
                device_label,
                thread: Some(thread),
            }),
            Ok(Err(err)) => Err(err),
            Err(_) => Err(CaptureError::Io(std::io::Error::other(
                "audio meter thread exited before reporting capture status",
            ))),
        }
    }

    /// Current input level, 0–100.
    pub fn level(&self) -> u32 {
        self.level.load(Ordering::Relaxed)
    }

    /// True if the underlying capture backend has reported an error.
    pub fn has_error(&self) -> bool {
        self.error.load(Ordering::Relaxed)
    }

    /// Human-readable label for the active device/route.
    pub fn device_label(&self) -> &str {
        &self.device_label
    }

    /// Signal the meter thread to stop (idempotent; also runs on drop).
    pub fn stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

impl Drop for LevelMeter {
    fn drop(&mut self) {
        self.stop();
        // Join the meter thread so the capture backend (and its stream) is fully
        // torn down before this handle goes away.
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_maps_system_aliases() {
        assert_eq!(
            LiveAudioSource::parse("system", None),
            LiveAudioSource::SystemAudio
        );
        assert_eq!(
            LiveAudioSource::parse("System-Audio", None),
            LiveAudioSource::SystemAudio
        );
        assert_eq!(
            LiveAudioSource::parse("tap", None),
            LiveAudioSource::SystemAudio
        );
    }

    #[test]
    fn parse_defaults_to_microphone_with_device() {
        assert_eq!(
            LiveAudioSource::parse("mic", Some("USB Mic".into())),
            LiveAudioSource::Microphone(Some("USB Mic".into()))
        );
        assert_eq!(
            LiveAudioSource::parse("unknown", None),
            LiveAudioSource::Microphone(None)
        );
    }

    #[test]
    fn as_str_round_trips() {
        assert_eq!(LiveAudioSource::Microphone(None).as_str(), "microphone");
        assert_eq!(LiveAudioSource::SystemAudio.as_str(), "system-audio");
    }

    #[test]
    fn level_meter_is_send() {
        // LevelMeter is parked in the Tauri AppState (Send + Sync), so it must
        // be Send even though the underlying cpal stream is not. This is a
        // compile-time guard against accidentally holding a !Send field.
        fn assert_send<T: Send>() {}
        assert_send::<LevelMeter>();
    }

    #[test]
    fn rms_to_level_clamps_and_scales() {
        assert_eq!(rms_to_level(0.0), 0);
        assert_eq!(rms_to_level(0.01), 20);
        assert_eq!(rms_to_level(1.0), 100); // clamped
        assert_eq!(rms_to_level(-0.5), 0); // clamped low
    }
}

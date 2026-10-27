# Device enumeration and settings persistence run off the UI thread

Two main-thread blockers surfaced as visible window-drag lag on macOS 26
(Tahoe): the Frame handler ran a synchronous `cpal` device enumeration once
per second (a cross-process CoreAudio round trip per device — also a
WASAPI/PulseAudio round trip on Windows/Linux), and the debounced settings
save wrote the RON file synchronously on the UI thread. Both stalled the
frame loop while AppKit competed for the same thread to deliver window-move
events.

## Decision

- **Device enumeration** (`SpectrumApp::refresh_devices`): every 1 s a
  background thread builds its own `CaptureSystem` (enumeration reads the
  system device list, independent of the main-thread host used for opening
  streams) and sends `(loopback, input)` results back over an mpsc channel;
  the Frame handler only polls the receiver. A stale request (> 5 s without
  a result) is treated as lost and re-issued.
- **Settings save** (`audio_spectrum_app::SettingsSaver`): a single writer
  thread owns all RON serialization + file I/O. `save` hands the latest
  snapshot over a channel and never blocks; queued writes collapse to the
  last one (the 300 ms debounce still applies at the call site). The
  process flushes and joins the queue before exit so a save inside the
  debounce window is not lost.

## Consequences

- The UI thread performs no disk I/O and no device enumeration; the same
  per-second and on-change work still happens, just off-thread.
- Enumerated device lists are now up to ~1 s + enumeration-time stale
  (previously enumerated at call time); acceptable for a Source picker.
- A final save submitted within the debounce window survives a normal
  exit (flush before `keep_awake::set_enabled(false)`); a hard kill can
  still lose it, as before.

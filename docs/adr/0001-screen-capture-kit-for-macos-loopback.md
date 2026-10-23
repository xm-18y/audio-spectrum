# macOS Loopback Capture uses ScreenCaptureKit (macOS 13+)

Loopback Capture on macOS has no public CoreAudio API. We adopt ScreenCaptureKit's
current-audio-output stream (via `objc2-screen-capture-kit`) and require macOS 13
Ventura or newer, rather than the widely-used CoreAudio Tap private APIs. On older
macOS, Loopback is unavailable and the app degrades to Input Capture only.

## Considered Options

- **CoreAudio Tap (private API)**: supports macOS 12-, but private symbols break
  across OS updates and are an App Store rejection risk.
- **ScreenCaptureKit**: public, stable, audio-only stream supported from macOS 13.

## Consequences

- Loopback users must run macOS 13+; the app must surface this in the source
  picker rather than failing at stream start.
- The macOS Loopback backend lives behind the `CaptureBackend` trait, so this
  constraint does not leak into `core` or `app`.

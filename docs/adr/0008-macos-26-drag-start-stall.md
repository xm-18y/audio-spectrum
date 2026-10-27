# macOS 26 title-bar drag start stalls the frame loop (winit 0.30)

On macOS 26, starting a title-bar drag visibly stalls the 60 fps frame loop
while it is running; once the window is moving the stutter disappears.
With the frame loop paused (Space in `repro/drag-stutter`), dragging is
smooth. Switching between a flat fill and a 1500-segment canvas makes no
difference — the stall is independent of per-frame cost.

主要原因是Magnet 和 BetterTouchTool 导致的问题，[On MacOS dragging window lags if rendering continuously with Magnet utility enabled and VSync enabled](https://github.com/rust-windowing/winit/issues/1737) 


## Diagnosis

- Pressing the title bar hands the drag to the WindowServer and switches
  the main run loop to `NSEventTrackingRunLoopMode`. The app receives no
  events at that instant (the title bar is not in winit's content view).
- winit 0.30 macOS deliberately services events and redraws **only in the
  default run loop mode** (`platform_impl/macos/observer.rs`: "Any events
  that are queued while running a modal or when live-resizing will instead
  wait, and be delivered to the application afterwards"; a trade-off for
  supporting live resize of blocking modals, winit #1779). The
  self-sustained redraw requests therefore stall.
- The first `windowDidMove` arrives at vsync cadence and is handled
  synchronously through the AppKit delegate path (`maybe_queue_window_event`
  → `handle_event`, bypassing default-mode queuing), which pumps the iced
  event/redraw cycle again — hence "stutter at start, smooth once moving".
- With the loop paused there is no pending redraw to stall — dragging is
  smooth, and the frame cost does not matter at all.

This is a framework-level scheduling limitation; the app cannot opt into
event-tracking mode (winit rules it out on purpose).

## Considered Options

- **App-level fix**: none — the redraw servicing mode is chosen by winit.
- **Reducing frame cost**: irrelevant, the stall is a fixed gap until the
  first `windowDidMove`, not cost-proportional (verified via the repro's
  1/2 canvas-load toggle).
- **Upstream fix / upgrade**: `cargo update -p iced -p winit` and re-test
  with `repro/drag-stutter`; report upstream with the repro if unfixed.

## Consequences

- Documented in README「已知取舍」; the minimal repro lives in
  `repro/drag-stutter` (workspace-excluded, `iced 0.14` + `winit 0.30.13`
  pinned to match the app's Cargo.lock).
- The app-level main-thread fixes in ADR-0007 removed the ~1 Hz device
  enumeration and settings-save hitches; this residual drag-start stall is
  the only remaining artifact and is framework-owned.

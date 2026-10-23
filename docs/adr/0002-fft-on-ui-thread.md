# Spectrum FFT runs on the UI thread

The spectrum pipeline (Hann window, 2048-point real FFT, Band aggregation,
smoothing) executes on the UI thread once per rendered frame (egui in
ADR-0002's day, now the iced shell's `Message::Frame` handler), reading the
newest samples from the lock-free ring buffer. A 2048-point real FFT costs tens
of microseconds against a 16.7ms frame budget, so a dedicated DSP worker thread
would add synchronization machinery with no measurable benefit.

## Considered Options

- **Dedicated DSP thread**: the conventional audio-engineering layout, but it
  requires an extra thread, channels, and backpressure handling for zero
  user-visible gain at this frame size.

## Consequences

- The audio callback must only write to the ring buffer and return immediately:
  never allocate, lock, or block.
- If the frame size grows well beyond 2048 (e.g. a future high-resolution
  spectrogram mode), re-test the frame budget assumption and revisit this
  decision.

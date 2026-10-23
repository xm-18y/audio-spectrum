# music-spec

A cross-platform (Windows/macOS/Linux) desktop application that displays the real-time frequency spectrum of audio being played or recorded on the machine.

## Language

**Spectrum**:
The magnitudes of all audible frequencies at a single moment, rendered as animated bars or a curve.
_Avoid_: FFT result, waveform, VU meter

**Spectrogram**:
A scrolling time×frequency heat map: one column per aggregation interval, colored by that interval's per-Band maximum. Reuses the Spectrum's log Band axis.
_Avoid_: waterfall, spectrogram history

**Input Capture**:
Acquiring audio from microphone or line-in devices.
_Avoid_: recording (implies persistence), microphone capture

**Loopback Capture**:
Acquiring the audio the system is currently playing to its output device.
_Avoid_: system audio, screen capture audio, what-you-hear

**Capture Backend**:
A platform-specific implementation that produces PCM streams for either Input or Loopback Capture.
_Avoid_: driver, audio API wrapper

### Spectrum rendering

**Bin**:
One of the frequency slots produced by a single FFT frame; 2048 samples yield 1024 bins.
_Avoid_: band, bucket

**Band**:
One displayed bar on the log frequency axis; each Band aggregates the Bins that fall inside its frequency range.
_Avoid_: bin, bar, column

**Peak Hold**:
The per-Band retained maximum, decaying slowly so recent peaks remain visible.
_Avoid_: peak marker, max line

**Reconnect**:
The automatic rebuilding of the capture stream after the active audio device changes or disappears, shown briefly in the UI.
_Avoid_: restart, refresh

**Source**:
The user-selected pairing of a capture kind (Input Capture or Loopback Capture) with a specific device; exactly one Source is active at a time.
_Avoid_: device, input, stream, channel

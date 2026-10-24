# audio-spectrum

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

### Updater

**Update Check**:
The act of querying the release source for a newer version, performed automatically at startup and manually from the update window; a failed check is silent by design. Rendered in the UI as 检测更新.
_Avoid_: 检查更新, upgrade, 更新检查

**Update Asset**:
The release artifact matching the current platform; its file name carries the version label and platform key, which is how the updater identifies candidates.
_Avoid_: 安装包, installer, binary

**Swap**:
Replacing the running app as one felt motion: the old executable is renamed aside, the new one takes its place, and a fresh process is launched. Rendered in the UI as 换装.
_Avoid_: 覆盖安装, hot update, in-place overwrite

**Red Dot**:
The small badge on the About menu's top-right corner meaning a newer version has been found or downloaded and awaits the Swap.
_Avoid_: 角标, notification badge

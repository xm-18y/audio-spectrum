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

### Views and Effects

**View**:
One of the eight display modes: Spectrum, Spectrogram, the four Effects, Static, Screensaver. Exactly one is active; the View menu, the S key, and the Effects Window tabs all address the same list.
_Avoid_: 页面, screen, tab

**Effect**:
A music-reactive view beyond the Spectrum; one of Waveform, Particles, Glow, or Disc. The Spectrum itself is not an Effect.
_Avoid_: 动效, visualizer, 视图特效

**Waveform**:
An Effect tracing the momentary sample amplitude across time, left to right. Time-domain, unlike the Spectrum.
_Avoid_: 示波器, 频谱, PCM 图

**Particles**:
An Effect of music-driven particles emitted and pulsing with the Bass Level.
_Avoid_: 粒子系统, 星尘

**Glow**:
An Effect of pulsing layered light rendered from Band energy.
_Avoid_: 光晕, 辉光, 律动光效

**Disc**:
The NetEase-style CD Effect: a spinning disc with tonearm and center Cover, pulsing with the Bass Level.
_Avoid_: 黑胶, vinyl, CD 播放器, 唱片机

**Cover**:
The image shown at a Disc's center: a user-selected image or the built-in default disc art — not necessarily real album art.
_Avoid_: 专辑封面, album art, 头图

**Bass Level**:
The shared per-frame loudness of the lowest Bands; every Effect pulses to it in sync.
_Avoid_: 节拍, beat, 音量, 低频均值

**Static**:
A View showing only the background image (theme base color when none is set) with nothing on top; not audio-reactive.
_Avoid_: 壁纸, 相框, 图片模式

**Screensaver**:
A View of ambient animations — morphing lines, ribbons, aurora, pipes, bubbles, starfield — that ignores the audio source.
_Avoid_: 待机, 省电模式, 动画壁纸

**Effects Window**:
The tabbed window opened from the View menu: one tab per View plus a 通用 tab for global options; each tab holds that View's options. The former settings window is dissolved into it.
_Avoid_: 设置窗, 特效选择器, 配置窗

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

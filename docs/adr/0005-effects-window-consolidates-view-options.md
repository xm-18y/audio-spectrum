# The Effects Window consolidates all per-view options

Per-View options used to live in two places: the View menu (Spectrum's Band-count
slider, Decay-speed submenu) and a separate settings window (Peak Hold, grid,
style, background image). With eight Views arriving — Spectrum, Spectrogram,
four music-reactive Effects (Waveform, Particles, Glow, Disc) and the
non-reactive Static and Screensaver — each with its own options, we decided
every View's options live in one tabbed Effects Window (特效窗体): one tab per
View, plus a 通用 tab for global options such as the background image. The
settings window is dissolved, its menu entry now opens the Effects Window's
通用 tab, and the View menu keeps only view switching, fullscreen, window
controls, and the Effects entry.

## Considered Options

- Keep menu + settings window as-is (rejected: eight Views would make the menu
  unusable and leave two config homes).
- Only the four Effects get tabs (rejected: "清理外部的选项" asked for full
  consolidation; half-moved options are worse than either extreme).

## Consequences

- The Spectrum's existing flat settings keys stay put; the new per-View
  sections (waveform/particles/glow/disc/screensaver) are added with
  struct-level `serde(default)`, so old `settings.ron` files load unchanged
  and simply get defaults for the new sections.
- The S key cycles the same ordered View list the tabs and menu address.

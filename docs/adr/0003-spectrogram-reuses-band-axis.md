# Spectrogram reuses the Band log axis instead of a linear frequency axis

Conventional spectrograms render time×frequency on a linear frequency axis.
music-spec's Spectrogram deliberately reuses the Spectrum's 96-Band log
axis (same `Analyzer`, same column semantics: a column is the per-Band
maximum of one aggregation interval). The two views become two renderings of
one mental model — the same feature lights up at the same horizontal position
in both — at the cost of deviating from measurement-tool convention, which is
acceptable for a companion visualizer (the agreed soul scenario).

## Considered Options

- **Linear-frequency spectrogram**: conventional, but needs a second
  frequency mapping and a second mental model alongside the Band-based bars.
- **Band-based (chosen)**: zero new concepts; core and capture untouched.

## Consequences

- The Spectrogram's frequency resolution is the Bands' (log-spaced); low
  frequencies get fewer pixels per Hz than a linear plot would show —
  visually compensated by the log spacing matching perception.
- Column history, aggregation and axis live in `music-spec-core`
  (`Spectrograph`), testable headlessly; rendering is a texture upload in
  the app (single quad per frame, keeping the 60 fps budget; see the HUD).

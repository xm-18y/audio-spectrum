# In-app self-update: GitHub Releases candidates, silent checks, auto-install Swap

The app checks GitHub Releases at startup (and on demand from the 检测更新
window) without any prompt, compares versions by semver only, downloads the
platform Update Asset with a live progress bar, and Swap-installs it the
moment the download finishes — no confirmation step. The Swap convention is
fixed: download/extract under `.update-tmp` next to the running executable
(same volume so rename works), rename the old executable to `.old`, move the
new one in, spawn it, exit; leftovers are cleaned on next launch. The rolling
`latest` prerelease participates as a candidate because it *is* this
project's primary distribution channel (see `.github/workflows/build.yml`),
and the asset naming contract (`audio-spectrum-<label>-<platform>`) is what
the updater parses, so changing either side is a coordinated break.

## Considered Options

- **Two-stage install (download → 立即安装 button)**: tried briefly; rejected —
  the extra state (`Ready`) and confirmation contradicted the zero-disturbance
  companion principle it was meant to serve; download already has explicit
  consent (the 更新 button).
- **iced `Subscription` for download progress**: rejected in favor of
  `std::thread` + `mpsc` polled once per frame — same shape as the capture
  ring-buffer model (ADR-0002), no async-runtime interaction, and the frame
  loop is already guaranteed to run.
- **Timestamp-aware comparison** (CI label `v0.7.0-YYYYMMDD-HHMM`): rejected —
  same-version rebuilds would nag users for zero functional change; semver is
  the meaningful signal.
- **Skip-this-version**: rejected — the Red Dot lives in one 7px corner, not a
  dialog; skip state would need persistence rules (does a new patch reset it?)
  that outweigh the benefit.
- **SHA256 verification of the asset**: deferred — HTTPS transport integrity
  suffices for the current threat model; adding it requires CI to publish
  `.sha256` first.

## Consequences

- A failed startup check is silent by design; the error surfaces only inside
  the 检测更新 window (retry button). A crashed check thread resets the state
  machine to `Idle` so it can be re-run.
- Platforms outside the CI matrix (Windows x64 / macOS arm64 / Linux x64)
  report "auto-update unsupported" rather than guessing at assets.
- Red Dot states are exactly `Available` and `Downloading`; it disappears
  when the Swap restarts the process into the new version.

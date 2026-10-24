# Keep wgpu, pin DX12 on Windows, apply fullscreen after the first frame

On machines with virtual display drivers (IddSampleDriver / Virtual Display
Driver) and AMD GPUs, iced 0.14 + wgpu 27 crashed in two ways:

1. Creating the window directly in fullscreen poisoned the image-texture atlas
   (`Texture with 'iced_wgpu::image texture atlas' label is invalid` inside
   `Renderer::new`) — while the identical creation sequence succeeded in an
   isolated probe on every adapter of the same machine, and succeeded in the
   app when the window was created windowed.
2. Any later fullscreen transition made `Surface::get_current_texture` return
   a `SurfaceError` on the **Vulkan** backend (wgpu's default pick on this
   machine), and iced 0.14's `present` reports every `SurfaceError` as an
   uncaptured wgpu error — whose default handler panics. iced's wgpu →
   tiny-skia fallback never engages because the failure panics instead of
   returning `Err`.

An automated probe (window starts windowed, switches to fullscreen after 2s,
exits healthy after 6s) showed **DX12 and GL survive the transition; only
Vulkan panics**. We therefore:

- keep wgpu (GPU rendering — tiny-skia was tried and rejected: ~7 fps at
  fullscreen on this machine, ~140 ms per frame, with multi-second stalls on
  view switches),
- pin the wgpu backend to **DX12 on Windows** (`pin_windows_wgpu_backend`,
  setting `WGPU_BACKEND` before the instance is created; a user-provided
  `WGPU_BACKEND` wins),
- apply startup fullscreen **after the first frame** via `window::set_mode`
  instead of at window creation.

Measured on the affected machine (fullscreen, DX12): Static/Particles/Spectrum
all render in ~8.5–9.5 ms per frame — comfortably 60 fps.

## Considered Options

- tiny-skia (software) renderer (tried, reverted): crash-free but unusable
  performance at fullscreen on virtual displays.
- Vendor-patch iced_wgpu's `present` to tolerate transient `SurfaceError`s
  (rejected: vendored dependency in-tree; the DX12 pin achieves the same with
  three lines).
- Keep Vulkan and tolerate errors per-frame (rejected: same vendoring cost).

## Consequences

- Windows runs on DX12 even where Vulkan might be marginally faster; the
  app's GPU workload is trivial, so this is acceptable.
- Exotic setups can still override via the standard `WGPU_BACKEND` env var.
- If iced later tolerates `SurfaceError`s in `present`, the pin can be dropped.

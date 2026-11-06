# GPUI migration spike

Temporary, standalone crate used to validate GPUI as the replacement for the
GTK frontend on **Windows and Linux**.

It is intentionally not a workspace member (it has its own `[workspace]`), so
the normal `lan-mouse` CI/build is unaffected. The `GPUI spike` workflow builds
it on ubuntu-latest and windows-2022/2025 (MSVC) without GTK/MSYS2.

What this validates:
- `gpui 0.2.2` from crates.io includes the Linux (Wayland/X11) and Windows
  backends; no unpublished `gpui_platform` crate is needed for this version.
- Linux needs `libxkbcommon-x11` (GPUI links it even for a Wayland build) and
  the usual Wayland/X11/fontconfig dev packages.
- `libc = 0.2.174` is pinned because GPUI 0.2.2 pulls
  `zed-async-tar -> xattr 0.2.3`, which does not build against newer libc.
- Windows must use the **MSVC** toolchain (Zed does not support MSYS2/MinGW
  for GPUI).

Status (2026-11-06): `cargo build --locked` succeeds on ubuntu-latest,
windows-2022 and windows-2025 (MSVC) in the `GPUI spike` workflow.

Not yet validated here: system tray integration, IPC bridge, GPU/driver matrix,
DPI/IME/accessibility and performance numbers. Those are the next go/no-go
items before the frontend rewrite starts.

Delete this directory and `.github/workflows/gpui-spike.yml` once the real
frontend and its CI are in place.

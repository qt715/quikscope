# Changelog

## 0.3.1 — 2026-11-20

### Packaging

- The Windows installer and uninstaller use the quikscope icon and branded
  artwork that stays sharp at every display scaling, with shorter page texts,
  component descriptions, a "Start quikscope now" option on the finish page
  (not in silent installs) and size and project links in Installed Apps.
- GPUI's Windows backend (`gpui-pre-windows` 0.3.10) is vendored with one fix,
  described in `vendor/gpui-pre-windows/QUIKSCOPE-PATCH.md`.

### Fixed

- Windows: closing a window no longer occasionally logs
  `[ERROR] Invalid window handle. (0x80070578)`. GPUI kept redrawing a window
  for a moment after destroying it.
- Windows: a daemon started from the Start menu, at login or by the installer
  writes `%LOCALAPPDATA%\quikscope\daemon.log` too; only one started by a
  command logged before.

## 0.3.0 — 2026-11-19

### Recording

- `quikscope record video` and `quikscope record animation` record a region
  (selected over the live desktop), an output or the active window. Running the
  same command again stops and saves; `record pause|resume|cancel|status`
  control the running recording, and `record gif` records an animation as GIF
  whatever the configured animation format.
- Videos: MP4, MKV or MOV with H.264 picture and Opus sound, 60 fps by default,
  with desktop audio and any number of microphones or other devices mixed in
  (`quikscope audio-devices` lists them). H.264 is encoded on the GPU through
  Vulkan Video (NVIDIA, AMD and Intel with recent drivers), otherwise by the
  built-in OpenH264 software encoder.
- MP4 recordings are fragmented by default and written under their final
  name, so a crash or kill leaves a file that plays up to its last fragment;
  `record.mp4_fragmented = false` writes a classic MP4 when recording ends.
  The next daemon start recovers a classic MP4 or MOV cut short this way, and
  reopens animations that were never encoded in the annotator.
- A capture larger than any H.264 encoder on the machine accepts is recorded
  scaled down, with a notification.
- Animations: GIF (palette per frame, dithered), WebP, AVIF or JPEG XL, 30 fps
  by default, recorded losslessly first so annotating and re-exporting loses
  nothing. Every frame keeps its exact display time.
- A thin outline marks the recorded area without being recorded. Finished files
  go on the clipboard as files (paste into a chat or file manager) and show a
  notification with a thumbnail; clicking it opens the editor.
- Linux system tray icon (StatusNotifierItem) with elapsed time, pause/resume,
  stop, discard, audio devices and frame rates; the same controls in the Windows
  tray. New shortcuts on Windows (Ctrl+Shift+PrintScreen, Ctrl+Alt+PrintScreen),
  which Settings can enable and disable like the others.
- `record status` reports `saving 42` (percent written) while a stopped
  recording is being finished, and the tray shows the percentage; `record
  cancel` or the tray's Cancel saving stop a long save, keeping an animation's
  lossless recording. Quitting quikscope (`quikscope quit`, the tray's Quit,
  stopping its service or logging out) saves a running recording and waits
  for exports and saves still running; the systemd unit allows ten minutes
  for that. An unavailable audio device is skipped with a warning.
- On wlroots compositors frames are copied into GPU buffers (DMA-BUF) whenever
  the compositor offers them. Shared memory is only a fallback, with a warning
  in the log: Hyprland reads shared-memory captures back on its main thread,
  which can make games stutter. Recording also works on X11, Windows (Desktop
  Duplication) and, experimentally, through the ScreenCast portal on GNOME/KDE.
  The portal's permission is remembered per monitor, so recording another
  monitor asks again.
- `default-output` and `default-input` follow the system's default device,
  also when it changes or disappears during a recording. On Windows any
  device can be recorded, including multichannel (7.1) headsets.
- On wlroots the recording outline stays visible over fullscreen windows.

### Editors

- Videos open in a small video editor: playback with sound, a filmstrip
  timeline with trim handles (or `I` / `O`), frame stepping, mute, and export of
  the trimmed range as MP4, MKV, MOV, GIF, WebP, AVIF or JPEG XL, saved to the
  recordings folder or copied as a file. `F` opens the current frame in the
  annotator.
- Animations open in the annotator: annotations apply to every frame and Save
  or Copy re-encode the animation.
- The top bar is split up: tools stay at the top, colors/stroke/fill move to a
  side panel that appears while drawing, undo/redo and zoom sit bottom left and
  copy/save/settings bottom right.
- Settings has a Recording section: formats and frame rates, recordings
  folder, audio devices, encoder, crash-safe MP4, outline, pointer, clipboard,
  notification, editor and (Linux) tray options.

### Opening files

- `quikscope open` takes several files and opens the latest capture without
  one. The desktop entry (`quikscope open %F`) registers PNG, JPEG, WebP, GIF,
  AVIF, JPEG XL, MP4, MKV and MOV, so file managers can open them in
  quikscope; the Windows installer adds quikscope to Explorer's Open with.
- Videos must be H.264 (with AAC, Opus or no sound); HEVC, VP9 and AV1 videos
  are not supported. Animated GIF, WebP, AVIF, JPEG XL and APNG files open in
  the annotator.

### Daemon

- `quikscope version` prints the client's and the running daemon's versions
  and warns when they differ (for example after an upgrade without restarting
  the daemon).
- `QUIKSCOPE_NO_PREWARM=1` on the daemon skips the GPU prewarm window at
  start-up.
- A 0.2 configuration whose `[hotkeys]` shortcut equals one of the new
  recording defaults stays valid: that default is left unbound instead of the
  whole file being rejected.
- The daemon log has timestamps and keeps the previous run's lines:
  an auto-started daemon appends to `daemon.log`, which moves to
  `daemon.log.1` above 1 MiB.

### Packaging

- No FFmpeg dependency: quikscope writes MP4, MOV and MKV itself and builds in
  its codecs (H.264 through Vulkan Video or OpenH264, Opus, and native GIF,
  WebP, AVIF and JPEG XL encoders). Packages ship and depend on no codec
  libraries; building from source needs CMake and a C/C++ compiler (NASM
  optional), and on Linux clang and the PipeWire, PulseAudio, ALSA and libgbm
  development files.

### Fixed

- Two screenshots saved at the same time (in the same second, or with a fixed
  `filename`) are written to separate files instead of one overwriting the other.
- Windows: a command that starts the daemon while its output is captured
  (`quikscope version | Out-String`, or `cmd /c "quikscope … > file"` from a
  caller capturing `cmd`'s output) no longer hangs until the daemon quits, and
  output redirected to a file from `cmd` is written instead of lost.

### Removed

- macOS support (last available in 0.2.0).

## 0.2.0 — 2026-11-17

### Capture

- `quikscope pick` (and **Pick color** in the Windows tray and macOS menu):
  freezes the screen with a magnifier at the pointer, prints the clicked
  pixel's hex color and opens a window that copies it as HEX, RGB, CMYK,
  HSV or HSL.

### Editor

- Eyedropper in the color picker (or `I`): picks a color from anywhere on
  the desktop, the editor included, into the drawing color. The picker also
  copies the current color as HEX, RGB, CMYK, HSV or HSL.

### Fixed

- Windows: commands such as `quikscope capture` and `quikscope pick` now
  start the daemon on a fresh user profile instead of failing with
  "A socket operation encountered a dead network".

## 0.1.0 — 2026-11-07

First release.

### Capture

- Capture a region, output, active window, whole desktop or fixed rectangle.
  Keep the screen frozen while selecting, or select over the live desktop.
- Run a resident daemon for quick repeated captures; commands start it on demand.
- Copy captures to the clipboard, save them, or open the editor from a notification.
- Reopen the latest capture with its annotations and last-used tool.
- Choose capture defaults in Settings and the first-run welcome window.

### Editor

- Annotate with rectangles, ellipses, arrows, lines, pen, highlighter, text,
  numbered steps and pixelation; crop, zoom, pan and undo/redo.
- Select multiple annotations to move, recolor, duplicate or reorder together.
- Customize colors and fonts, including multilingual text and input methods.
- Save annotated images or copy them directly to the clipboard.
- Extract text using optional Tesseract language models, including screenshots
  mixing several languages. Extracted text can appear in notification history;
  use `--no-notify` when handling sensitive text.

### Platforms

- Linux wlroots capture and selection use screencopy, layer-shell and
  data-control protocols. Hyprland also provides window snapping and active-window capture.
- X11 supports capture, selection, image/text clipboard and EWMH window snapping.
  Live selection requires a compositor, an alpha visual and X Present; otherwise
  it falls back to frozen selection.
- Experimental GNOME/KDE Screenshot portal integration has only been tested
  against a simulated portal under headless labwc, not those desktops themselves.
- Windows offers DXGI capture with GDI fallback, clipboard, tray menu, global
  shortcuts, notifications and per-user autostart. Frozen and live region
  selection were verified on one Windows 11 monitor at 96 DPI. Windows 10,
  mixed DPI, multiple monitors, window snapping and OCR remain unverified.
- macOS 12.3+: ScreenCaptureKit Retina capture, CoreGraphics window snapping,
  PNG/TIFF/text pasteboard, menu-bar actions, configurable global shortcuts,
  bundled notifications and per-user LaunchAgent autostart. Apple Silicon
  and Intel app builds share one bundle definition; interactive checks remain outstanding.
- Application icon for Linux launchers and notifications, X11 editor and
  settings windows, and the Windows executable and tray.

### Reliability and security

- Automated regression coverage includes annotations, settings and capture flows.
- Private per-user IPC fallback directories, bounded requests and raw-cache
  validation protect against malformed local input. Windows OCR excludes the
  current directory from DLL lookup; configuration writes sync before replacement.

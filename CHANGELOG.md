# Changelog

## 0.3.0 — unreleased

### Recording

- `quikscope record video` and `quikscope record animation` record a region
  (selected over the live desktop), an output or the active window. Running the
  same command again stops and saves; `record pause|resume|cancel|status`
  control the running recording.
- Videos: MP4, MKV or MOV (H.264 with AAC, or Opus in MKV), 60 fps by default,
  with desktop audio and any number of microphones or other devices mixed in
  (`quikscope audio-devices` lists them). GPU encoders (NVENC, VAAPI, Quick Sync,
  AMF, Media Foundation, VideoToolbox) are used when they work.
- Animations: GIF (two-pass palette), WebP, AVIF or JPEG XL, 30 fps by default,
  recorded losslessly first so annotating and re-exporting loses nothing.
- A thin outline marks the recorded area without being recorded. Finished files
  go on the clipboard as files (paste into a chat or file manager) and show a
  notification with a thumbnail; clicking it opens the editor.
- Linux system tray icon (StatusNotifierItem) with elapsed time, pause/resume,
  stop, discard, audio devices and frame rates; the same controls in the Windows
  tray and macOS menu bar. New shortcuts on Windows (Ctrl+Shift+PrintScreen,
  Alt+Shift+PrintScreen) and macOS (Cmd+Shift+9, Cmd+Shift+0).
- On wlroots compositors frames are copied into GPU buffers (DMA-BUF), never
  shared memory: Hyprland reads shared-memory captures back on its main thread,
  which makes games stutter. Recording also works on X11, Windows (Desktop
  Duplication), macOS (ScreenCaptureKit) and, experimentally, through the
  ScreenCast portal on GNOME/KDE.

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
  folder, audio devices, encoder, outline, pointer, clipboard, notification,
  editor and (Linux) tray options.

### Opening files

- `quikscope open` takes several files and opens the latest capture without
  one. The desktop entry (`quikscope open %F`) registers PNG, JPEG, WebP, GIF,
  AVIF, JPEG XL, MP4, MKV, MOV and WebM, so file managers can open them in
  quikscope; the Windows installer adds quikscope to Explorer's Open with, and
  the macOS app accepts files from Finder.

### Packaging

- quikscope now links FFmpeg. The .deb/.rpm, Windows installer and macOS app
  bundle an LGPL FFmpeg build; Arch and `cargo install` use the system FFmpeg
  (and need its development files, libgbm, PipeWire and PulseAudio libraries).

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

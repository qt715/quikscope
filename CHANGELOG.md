# Changelog

## 0.1.0 — unreleased

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

# Changelog

## 0.1.0 — unreleased

First release.

- Daemon with line-based IPC over a Unix socket, started on demand by any command.
- Capture modes: frozen-screen region selection across all outputs (with
  window snapping on Hyprland), focused output, active window, whole screen,
  and fixed geometry.
- Clipboard ownership through `wlr-data-control`; desktop notification with a
  thumbnail whose click opens the editor.
- GPUI annotation editor: rectangle, ellipse, arrow, line, pen, highlighter,
  text, numbered steps, pixelate, crop; select/move, undo/redo, zoom/pan with
  cubic-bezier eased zoom transitions; refits to the window on resize;
  `Ctrl+S` to `~/Pictures/Screenshots`, `Ctrl+C` to copy.
- `open-last` and `open <file>`; annotations survive closing the editor. The
  last-capture cache has a single ordered writer, so rapid or overlapping
  captures always leave the newest one, and `quit` waits for it.
- `moment notifications on|off|toggle|status`: switch capture notifications
  at runtime; the change is saved to the config file.
- Unit, golden-image, GPU/CPU parity and headless end-to-end test suites; CI.

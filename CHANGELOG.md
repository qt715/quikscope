# Changelog

## 0.1.0 — unreleased

First release.

### Platforms

- wlroots compositors (Hyprland, sway, river, labwc, …): `wlr-screencopy`
  capture, layer-shell selection overlays and `wlr-data-control` clipboard
  ownership; window snapping, `window` mode and focused output via Hyprland
  IPC.
- GNOME and KDE Plasma through the Screenshot portal: layer-shell selection
  where offered, otherwise a single fullscreen selection tracking the output
  it lands on; clear permission errors; data-control or Xwayland clipboard.
- X11: persistent MIT-SHM capture with GetImage fallback, RandR monitors,
  XFixes cursor, EWMH window snapping, override-redirect overlays, and
  PNG/text CLIPBOARD ownership with large-image INCR transfers.
- Windows 10/11: resident DXGI capture with GDI fallback, per-monitor-v2 DPI,
  DWM window snapping, delayed PNG/DIB clipboard, tray menu, configurable
  global hotkeys, clickable shell notifications and per-user `autostart`.
- `QUICKSCOPE_BACKEND=wlroots|portal|x11` forces a Linux backend.

### Capture and daemon

- Daemon with line-based IPC over a local socket, started on demand by any
  command.
- Capture modes: frozen-screen region selection across all outputs, focused
  output, active window, whole screen and fixed geometry.
- Optional live region selection (`freeze = false`, CLI `--no-freeze`, IPC
  `no-freeze`): dim outside the rectangle, keep its interior transparent, and
  capture pixels after the overlays close and teardown is synchronized.
  Frozen selection remains the default (`--freeze` / IPC `freeze` overrides).
  Portal permission dialogs appear after live selection; fullscreen GNOME
  transparency depends on the compositor. X11 without compositing, an alpha
  visual or X Present falls back to frozen selection. The Windows path
  compiles but has not been verified natively.
- With native wlroots capture, selection overlays are created while the
  capture runs.
- Desktop notification with a thumbnail whose click opens the editor;
  `quickscope notifications on|off|toggle|status` switches them at runtime
  and saves the change to the config file.
- `open-last` (with the tool used last) and `open <file>`; annotations
  survive closing the editor. The last-capture cache has a single ordered
  writer, so rapid or overlapping captures always leave the newest one, and
  `quit` waits for it.
- Themed settings window from `settings`, the editor gear and the Windows tray;
  capture/editor/OCR defaults save immediately with formatting-preserving,
  validated config edits. Windows hotkeys can be enabled/disabled live.
- First-run welcome chooses frozen/live region selection and common capture
  defaults; Done or closing it writes the config and completes onboarding.

### Editor

- Rectangle, ellipse, arrow, line, pen, highlighter, text, numbered steps,
  pixelate and crop; undo/redo; `Ctrl+S` to `~/Pictures/Screenshots`,
  `Ctrl+C` to copy.
- Zoom between fixed levels (10% steps up to 100%, 25% above, 100% = actual
  pixels) with cubic-bezier eased transitions; the view refits on resize.
- Select tool: multi-selection by marquee (fully contained), Shift-click and
  select-all; group move, nudge, style and delete as one undo step;
  duplicate with `Ctrl+D`; bring to front / send to back; hover cursor and
  outline on the item a press would grab.
- Drawings that extend past the screenshot grow the saved image, with a
  transparent margin; crops may include them.
- Color picker (hue/saturation wheel, value and alpha sliders, hex and RGBA
  fields) with live recoloring as one undo step.
- Shortcut help from `?`, `F1` or the toolbar; constrained to the window with
  scrolling and narrow-window reflow, including during text editing with `F1`.

### Text

- Font bar for text: family (installed fonts plus bundled Inter
  variable/italic), weight, italic and size; `Ctrl+B`/`Ctrl+I`/
  `Ctrl+Shift+.`/`Ctrl+Shift+,`; `[editor] font_*` defaults.
- Text in any language: input methods (fcitx5, IBus) compose into the text
  tool and the font search field; shaping with rustybuzz, bidirectional
  text, per-grapheme fallback to installed fonts, colour emoji, and caret
  movement and deletion by grapheme.
- Text extraction (Tesseract, loaded at run time): `capture … --text` and
  `Ctrl+Shift+C` copy a screenshot's text, each line read in its own
  language; `[ocr] languages` in the config.

### Quality

- Unit, golden-image, GPU/CPU parity, editor interaction, text extraction,
  Wayland and X11 end-to-end suites; CI on Linux and Windows with release
  packages for both.

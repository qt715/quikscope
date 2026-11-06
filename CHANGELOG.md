# Changelog

## 0.1.0 — unreleased

First release.

- Daemon with line-based IPC over a Unix socket, started on demand by any command.
- Capture modes: frozen-screen region selection across all outputs (with
  window snapping on Hyprland), focused output, active window, whole screen,
  and fixed geometry.
- Clipboard ownership through `wlr-data-control`; desktop notification with a
  thumbnail whose click opens the editor.
- X11 backend: persistent MIT-SHM capture with GetImage fallback, RandR monitors,
  XFixes cursor, EWMH window snapping, override-redirect region overlays, and
  PNG/text CLIPBOARD ownership with large-image INCR transfers.
- GPUI annotation editor: rectangle, ellipse, arrow, line, pen, highlighter,
  text, numbered steps, pixelate, crop; select/move, undo/redo, zoom/pan with
  cubic-bezier eased zoom transitions; refits to the window on resize;
  `Ctrl+S` to `~/Pictures/Screenshots`, `Ctrl+C` to copy.
- `open-last` and `open <file>`; annotations survive closing the editor. The
  last-capture cache has a single ordered writer, so rapid or overlapping
  captures always leave the newest one, and `quit` waits for it.
- `quickscope notifications on|off|toggle|status`: switch capture notifications
  at runtime; the change is saved to the config file.
- Color picker (hue/saturation wheel, value and alpha sliders, hex and RGBA
  fields) behind the palette button; live recoloring of the selection as one
  undo step.
- Font customization for text: family (installed fonts plus bundled Inter
  variable/italic), weight, italic and size from a font bar above the text,
  `Ctrl+B`/`Ctrl+I`/`Ctrl+Shift+.`/`Ctrl+Shift+,` shortcuts, and
  `[editor] font_*` defaults in the config.
- Text in any language: input methods (fcitx5, IBus) compose into the text
  tool and the font search field, with the pre-edit underlined in place and
  the candidate window at the caret; shaping with rustybuzz (kerning,
  ligatures, Arabic joining, Indic conjuncts), bidirectional text, per-grapheme
  fallback to installed fonts (CJK variant chosen from the text or locale),
  colour emoji, and caret movement and deletion by grapheme.
- Text extraction (Tesseract, loaded at run time): `capture … --text` and
  `Ctrl+Shift+C` / the scan button in the editor copy a screenshot's text.
  Each line is read in its own language (English first, then every
  installed model re-reads doubtful lines in parallel); CJK and Thai
  spacing, mixed dark/light layouts, `[ocr] languages` in the config.
- Drawings that extend past the screenshot grow the saved image, with a
  transparent margin; the editor refits to show them and crops may include
  them.
- Zoom moves between fixed levels (10% steps up to 100%, 25% above, 100% =
  actual pixels), one level per wheel notch, still eased; fitting snaps to
  a level.
- `open-last` opens the editor with the tool used last.
- Select tool: hand cursor and an outline on the item a press would grab,
  and a selected item drags from anywhere inside its box.
- Unit, golden-image, GPU/CPU parity, editor interaction, text extraction
  and headless end-to-end test suites; CI.
